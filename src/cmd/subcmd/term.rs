use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use color_eyre::Result;
use color_eyre::eyre::Context;
use serde::{Deserialize, Serialize};

use craft_agent::ToolOutput;
use craft_providers::{Message, TokenUsage};
use craft_storage::StateDir;
use craft_storage::sessions::Session;

use crate::cli::{ShellKind, TermAction};
use crate::cmd::headless::{self, HeadlessOptions};
use crate::print::OutputFormat;

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

fn log(command: String) -> Result<()> {
    let trimmed = command.trim();
    if trimmed.is_empty() || trimmed.starts_with("craft term") || trimmed.starts_with("@craft") {
        return Ok(());
    }
    let storage = StateDir::resolve().context("resolve data directory")?;
    let path = storage.path().join(SHELL_HISTORY_FILE);
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .to_string_lossy()
        .into_owned();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = ShellEntry {
        cwd,
        command: trimmed.to_string(),
        ts,
    };
    let line = serde_json::to_string(&entry).unwrap_or_default();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .context("open shell history")?;
    writeln!(file, "{line}").context("write shell history")?;
    drop(file);
    rotate_history(&path);
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

    let outcome = headless::run_headless(HeadlessOptions {
        model,
        prompt: query,
        yolo: false,
        no_plugins: false,
        no_rtk: false,
        extra_excluded_tools: vec![],
        context,
        persist_session: false,
        max_turns: None,
        allowed_tools: vec![],
        stream: matches!(output_format, OutputFormat::Text),
    })
    .await?;
    headless::print_outcome(&outcome, output_format);
    Ok(())
}

fn info() -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .to_string_lossy()
        .into_owned();
    match Session::<Message, TokenUsage, ToolOutput>::latest(&cwd, &storage)
        .ok()
        .flatten()
    {
        Some(s) => println!("Active session: {}", s.id),
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

fn read_history() -> Vec<String> {
    let Ok(storage) = StateDir::resolve() else {
        return Vec::new();
    };
    let path = storage.path().join(SHELL_HISTORY_FILE);
    let mut contents = String::new();
    if fs::File::open(&path)
        .and_then(|mut f| f.read_to_string(&mut contents))
        .is_err()
    {
        return Vec::new();
    }
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .to_string_lossy()
        .into_owned();
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
