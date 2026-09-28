//! G.9 shell integration (`craft term`), ported from the reference
//! `src/cmd/subcmd/term.rs`. The reference's `headless::run_headless` with
//! extra context is replaced by [`crate::cli::run_headless_query`], whose
//! `context` blocks wrap the same `<context>` envelope; the reference's
//! `Session::latest` lookup is replaced by the cwd index
//! (`cwd_latest.json`).

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cli::{OutputFormat, ShellKind, TermAction};
use crate::error::{InvalidSnafu, Result};
use crate::storage::StateDir;

const SHELL_HISTORY_FILE: &str = "shell_history.jsonl";
const MAX_HISTORY: usize = 50;
const HISTORY_ROTATION_CAP: usize = 5000;

#[derive(Serialize, Deserialize)]
struct ShellEntry {
    cwd: String,
    command: String,
    ts: u64,
}

pub async fn run(action: TermAction) -> Result<()> {
    match action {
        TermAction::Init {
            shell,
            with_not_found,
        } => init(shell, with_not_found),
        TermAction::Log { command } => log(command),
        TermAction::Run {
            query,
            model,
            output_format,
        } => run_query(query.join(" "), model, output_format).await,
        TermAction::Info => info(),
    }
}

fn init(shell: ShellKind, with_not_found: bool) -> Result<()> {
    let script = match shell {
        ShellKind::Bash => bash_init(with_not_found),
        ShellKind::Zsh => zsh_init(with_not_found),
        ShellKind::Fish => fish_init(with_not_found),
    };
    print!("{script}");
    Ok(())
}

fn state_dir() -> Result<StateDir> {
    StateDir::resolve().map_err(|e| {
        InvalidSnafu {
            reason: format!("resolve data directory: {e}"),
        }
        .build()
    })
}

fn log(command: String) -> Result<()> {
    let storage = state_dir()?;
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .to_string_lossy()
        .into_owned();
    log_to(&storage.path().join(SHELL_HISTORY_FILE), &cwd, &command)
}

fn log_to(path: &Path, cwd: &str, command: &str) -> Result<()> {
    let trimmed = command.trim();
    if trimmed.is_empty() || trimmed.starts_with("craft term") || trimmed.starts_with("@craft") {
        return Ok(());
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = ShellEntry {
        cwd: cwd.to_string(),
        command: trimmed.to_string(),
        ts,
    };
    let line = serde_json::to_string(&entry).unwrap_or_default();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| {
            InvalidSnafu {
                reason: format!("open shell history: {e}"),
            }
            .build()
        })?;
    writeln!(file, "{line}").map_err(|e| {
        InvalidSnafu {
            reason: format!("write shell history: {e}"),
        }
        .build()
    })?;
    drop(file);
    rotate_history(path);
    Ok(())
}

fn rotate_history(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    let line_cap = metadata.len() / 80;
    if line_cap < HISTORY_ROTATION_CAP as u64 {
        return;
    }
    let Ok(contents) = fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = contents.lines().collect();
    let keep = lines.len().saturating_sub(HISTORY_ROTATION_CAP / 2);
    let rotated: String = lines[keep..].join("\n");
    let _ = fs::write(path, format!("{rotated}\n"));
}

async fn run_query(
    query: String,
    model: Option<String>,
    output_format: OutputFormat,
) -> Result<()> {
    let history = read_history();
    let context = if history.is_empty() {
        Vec::new()
    } else {
        let mut block = String::from("Recent shell commands run by the user (oldest first):\n");
        for (i, cmd) in history.iter().enumerate() {
            block.push_str(&format!("{}. {cmd}\n", i + 1));
        }
        vec![block]
    };

    let config = crate::config::Config::load().await?;
    crate::cli::run_headless_query(
        config,
        crate::cli::HeadlessQuery {
            prompt: query,
            context,
            images: Vec::new(),
            model,
            output_format,
            verbose: false,
            mode: crate::cli::CliMode::default(),
            session_id: None,
        },
    )
    .await
}

fn info() -> Result<()> {
    let storage = state_dir()?;
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .to_string_lossy()
        .into_owned();
    match latest_session_for_cwd(&storage.path().join("cwd_latest.json"), &cwd) {
        Some(id) => println!("Active session: {id}"),
        None => println!("No active session for this directory."),
    }
    let history = read_history();
    if history.is_empty() {
        println!("No logged commands yet. Run `eval \"$(craft term init bash)\"` to start.");
    } else {
        println!("Recent commands:");
        for (i, cmd) in history.iter().enumerate() {
            println!("  {}. {cmd}", i + 1);
        }
    }
    Ok(())
}

/// The cwd index maps cwd → latest session id (see
/// `storage::sessions::index`); the reference calls `Session::latest`.
fn latest_session_for_cwd(index_path: &Path, cwd: &str) -> Option<String> {
    let data = fs::read(index_path).ok()?;
    let index: std::collections::HashMap<String, String> = serde_json::from_slice(&data).ok()?;
    index.get(cwd).filter(|id| !id.is_empty()).cloned()
}

fn read_history() -> Vec<String> {
    let Ok(storage) = StateDir::resolve() else {
        return Vec::new();
    };
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .to_string_lossy()
        .into_owned();
    read_history_from(&storage.path().join(SHELL_HISTORY_FILE), &cwd)
}

fn read_history_from(path: &Path, cwd: &str) -> Vec<String> {
    let mut contents = String::new();
    if fs::File::open(path)
        .and_then(|mut f| f.read_to_string(&mut contents))
        .is_err()
    {
        return Vec::new();
    }
    let mut cmds: Vec<String> = Vec::new();
    for line in contents.lines() {
        if let Ok(entry) = serde_json::from_str::<ShellEntry>(line)
            && entry.cwd == cwd
        {
            cmds.push(entry.command);
        }
    }
    let start = cmds.len().saturating_sub(MAX_HISTORY);
    cmds.into_iter().skip(start).collect()
}

const BASH_BASE: &str = r#"# craft terminal integration (bash)
__craft_preexec() {
  case "$BASH_COMMAND" in
    "craft term "*|"@craft "*|"craft "*) return 0 ;;
  esac
  craft term log "$BASH_COMMAND" >/dev/null 2>&1
}
trap '__craft_preexec' DEBUG
@craft() { craft term run "$*"; }
"#;

const BASH_NOT_FOUND: &str = r#"command_not_found_handle() {
  craft term run "The command '$1' was not found."
  return 127
}
"#;

const ZSH_BASE: &str = r#"# craft terminal integration (zsh)
__craft_preexec() {
  case "$1" in
    "craft term "*|"@craft "*|"craft "*) return 0 ;;
  esac
  craft term log "$1" >/dev/null 2>&1
}
preexec_functions+=(__craft_preexec)
@craft() { craft term run "$*"; }
"#;

const ZSH_NOT_FOUND: &str = r#"command_not_found_handler() {
  craft term run "The command '$1' was not found."
  return 127
}
"#;

const FISH_BASE: &str = r#"# craft terminal integration (fish)
function __craft_preexec --on-event fish_preexec
    switch "$argv"
        case 'craft term *' '@craft *' 'craft *'
            return 0
    end
    craft term log "$argv" >/dev/null 2>&1
end
function @craft
    craft term run $argv
end
"#;

const FISH_NOT_FOUND: &str = r#"function fish_command_not_found
    craft term run "The command '$argv' was not found."
end
"#;

fn bash_init(with_not_found: bool) -> String {
    let mut s = BASH_BASE.to_string();
    if with_not_found {
        s.push_str(BASH_NOT_FOUND);
    }
    s
}

fn zsh_init(with_not_found: bool) -> String {
    let mut s = ZSH_BASE.to_string();
    if with_not_found {
        s.push_str(ZSH_NOT_FOUND);
    }
    s
}

fn fish_init(with_not_found: bool) -> String {
    let mut s = FISH_BASE.to_string();
    if with_not_found {
        s.push_str(FISH_NOT_FOUND);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_scripts_install_the_preexec_hook_and_alias() {
        for (shell, preexec) in [
            (ShellKind::Bash, "trap '__craft_preexec' DEBUG"),
            (ShellKind::Zsh, "preexec_functions+=(__craft_preexec)"),
            (ShellKind::Fish, "--on-event fish_preexec"),
        ] {
            let script = init_script(&shell, false);
            assert!(script.contains(preexec), "missing hook for {shell:?}");
            assert!(script.contains("craft term log"));
            assert!(script.contains("@craft"));
            assert!(
                !script.contains("not_found"),
                "unexpected handler for {shell:?}"
            );
        }
    }

    #[test]
    fn with_not_found_appends_the_handler() {
        for shell in [&ShellKind::Bash, &ShellKind::Zsh, &ShellKind::Fish] {
            assert!(init_script(shell, true).contains("craft term run \"The command '$"));
        }
    }

    fn init_script(shell: &ShellKind, with_not_found: bool) -> String {
        match shell {
            ShellKind::Bash => bash_init(with_not_found),
            ShellKind::Zsh => zsh_init(with_not_found),
            ShellKind::Fish => fish_init(with_not_found),
        }
    }

    #[test]
    fn log_skips_craft_and_empty_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(SHELL_HISTORY_FILE);
        log_to(&path, "/repo", "cargo build").unwrap();
        log_to(&path, "/repo", "  ").unwrap();
        log_to(&path, "/repo", "craft term run fix this").unwrap();
        log_to(&path, "/repo", "@craft what happened").unwrap();
        assert_eq!(read_history_from(&path, "/repo"), ["cargo build"]);
    }

    #[test]
    fn history_is_filtered_by_cwd_and_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(SHELL_HISTORY_FILE);
        for i in 0..(MAX_HISTORY + 5) {
            log_to(&path, "/repo", &format!("cmd {i}")).unwrap();
        }
        log_to(&path, "/elsewhere", "other cwd").unwrap();
        let history = read_history_from(&path, "/repo");
        assert_eq!(history.len(), MAX_HISTORY);
        assert_eq!(history.first().unwrap(), "cmd 5");
        assert_eq!(history.last().unwrap(), &format!("cmd {}", MAX_HISTORY + 4));
        assert_eq!(read_history_from(&path, "/nope"), Vec::<String>::new());
    }

    #[test]
    fn rotate_history_trims_to_half_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(SHELL_HISTORY_FILE);
        let content: String = (0..HISTORY_ROTATION_CAP + 100)
            // Lines must average >80 bytes for the size heuristic to trip.
            .map(|i| {
                format!(
                    "{{\"cwd\":\"/r\",\"command\":\"c{i}\",\"pad\":\"{}\"}}\n",
                    "x".repeat(60)
                )
            })
            .collect();
        std::fs::write(&path, &content).unwrap();
        rotate_history(&path);
        let rotated = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = rotated.lines().collect();
        assert_eq!(lines.len(), HISTORY_ROTATION_CAP / 2);
        assert!(rotated.contains("\"command\":\"c2600\""));
        assert!(!rotated.contains("\"command\":\"c2599\""));
    }

    #[test]
    fn latest_session_reads_the_cwd_index() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cwd_latest.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&std::collections::HashMap::from([
                ("/repo".to_string(), "abc".to_string()),
                ("/empty".to_string(), String::new()),
            ]))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            latest_session_for_cwd(&path, "/repo"),
            Some("abc".to_string())
        );
        assert_eq!(latest_session_for_cwd(&path, "/empty"), None);
        assert_eq!(latest_session_for_cwd(&path, "/missing"), None);
    }
}
