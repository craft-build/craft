use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::components::Action;
use notify::{EventKind, RecursiveMode, Watcher};
use regex::Regex;
use std::sync::LazyLock;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const DEBOUNCE: Duration = Duration::from_millis(800);
const MAX_FILE_SIZE: u64 = 1_000_000;

const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    "dist",
    "build",
    ".cache",
];

static AI_COMMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:#|//|--|;+)\s*ai\b([!?.]?)\s*(.*)").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchAction {
    AddContext,
    CodeChange,
    Ask,
}

#[derive(Debug, Clone)]
pub struct AiComment {
    pub file: PathBuf,
    pub line_num: usize,
    pub text: String,
    pub action: WatchAction,
}

pub struct WatcherHandle {
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WatcherHandle {
    pub fn stop(mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn should_ignore(path: &std::path::Path) -> bool {
    for component in path.components() {
        if let std::path::Component::Normal(seg) = component
            && let Some(s) = seg.to_str()
            && IGNORED_DIRS.contains(&s)
        {
            return true;
        }
    }
    false
}

fn parse_action(prefix: &str) -> WatchAction {
    if prefix.contains('!') {
        WatchAction::CodeChange
    } else if prefix.contains('?') {
        WatchAction::Ask
    } else {
        WatchAction::AddContext
    }
}

pub fn extract_ai_comments(content: &str) -> Vec<AiComment> {
    let mut comments = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if let Some(caps) = AI_COMMENT_RE.captures(line) {
            let suffix = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let action = parse_action(suffix);
            let text = caps
                .get(2)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            comments.push(AiComment {
                file: PathBuf::new(),
                line_num: i + 1,
                text,
                action,
            });
        }
    }
    comments
}

pub fn build_prompt(comments: &[AiComment]) -> Option<(String, WatchAction)> {
    if comments.is_empty() {
        return None;
    }

    let strongest = comments
        .iter()
        .max_by_key(|c| match c.action {
            WatchAction::CodeChange => 3,
            WatchAction::Ask => 2,
            WatchAction::AddContext => 1,
        })
        .map(|c| c.action)?;

    let mut prompt = String::new();
    for c in comments {
        let action_label = match c.action {
            WatchAction::CodeChange => "AI!",
            WatchAction::Ask => "AI?",
            WatchAction::AddContext => "AI",
        };
        let file_display = c.file.to_string_lossy();
        if c.text.is_empty() {
            prompt.push_str(&format!(
                "`{file_display}` line {} [{action_label}]\n",
                c.line_num
            ));
        } else {
            prompt.push_str(&format!(
                "`{file_display}` line {} [{}]: {}\n",
                c.line_num, action_label, c.text
            ));
        }
    }

    match strongest {
        WatchAction::CodeChange => {
            prompt.push_str(
                "\nPlease make the requested changes. After acting, delete the AI! comments you addressed.\n",
            );
        }
        WatchAction::Ask => {
            prompt.push_str("\nPlease answer these questions about the code.\n");
        }
        WatchAction::AddContext => {
            prompt.push_str("\nThese files have been added to your context for awareness.\n");
        }
    }

    Some((prompt, strongest))
}

pub fn spawn_watcher(cwd: PathBuf, action_tx: flume::Sender<Action>) -> Option<WatcherHandle> {
    let running = Arc::new(AtomicBool::new(true));
    let running_proc = Arc::clone(&running);
    let cwd_for_thread = cwd.clone();
    let thread = std::thread::spawn(move || {
        let (notify_tx, notify_rx) = flume::unbounded::<PathBuf>();

        let Ok(mut watcher) = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            if let Ok(event) = res
                && matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
            {
                for path in &event.paths {
                    if !should_ignore(path) && path.is_file() {
                        let _ = notify_tx.send(path.clone());
                    }
                }
            }
        }) else {
            return;
        };

        let _ = watcher.watch(&cwd_for_thread, RecursiveMode::Recursive);

        let mut last_fire: Option<(PathBuf, Instant)> = None;
        while running_proc.load(Ordering::Relaxed) {
            match notify_rx.recv_timeout(POLL_INTERVAL) {
                Ok(path) => {
                    if let Some((last_path, last_time)) = &last_fire
                        && *last_path == path
                        && last_time.elapsed() < DEBOUNCE
                    {
                        continue;
                    }

                    let Ok(meta) = std::fs::metadata(&path) else {
                        continue;
                    };
                    if meta.len() > MAX_FILE_SIZE {
                        continue;
                    }

                    let Ok(content) = std::fs::read_to_string(&path) else {
                        continue;
                    };

                    let mut comments = extract_ai_comments(&content);
                    if comments.is_empty() {
                        continue;
                    }

                    for c in &mut comments {
                        c.file = path.strip_prefix(&cwd).unwrap_or(&path).to_path_buf();
                    }

                    if let Some((prompt, _action)) = build_prompt(&comments) {
                        let file_list = comments
                            .iter()
                            .map(|c| c.file.clone())
                            .collect::<std::collections::HashSet<_>>()
                            .into_iter()
                            .collect::<Vec<_>>();
                        let _ = action_tx.send(Action::WatchPrompt {
                            text: prompt,
                            files: file_list,
                        });
                    }

                    last_fire = Some((path, Instant::now()));
                }
                Err(flume::RecvTimeoutError::Timeout) => {}
                Err(flume::RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(watcher);
    });

    Some(WatcherHandle {
        running,
        thread: Some(thread),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_code_comment() {
        let comments = extract_ai_comments("// AI! rename this to foo\nfn bar() {}\n");
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].action, WatchAction::CodeChange);
        assert_eq!(comments[0].text, "rename this to foo");
    }

    #[test]
    fn extracts_ask_comment() {
        let comments = extract_ai_comments("# AI? what does this do?\n");
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].action, WatchAction::Ask);
    }

    #[test]
    fn extracts_context_comment() {
        let comments = extract_ai_comments("// AI remember this file\n");
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].action, WatchAction::AddContext);
    }

    #[test]
    fn ignores_normal_comments() {
        let comments = extract_ai_comments("// this is a normal comment\n# also normal\n");
        assert!(comments.is_empty());
    }

    #[test]
    fn build_prompt_picks_strongest_action() {
        let comments = vec![
            AiComment {
                file: PathBuf::from("a.rs"),
                line_num: 1,
                text: "context".into(),
                action: WatchAction::AddContext,
            },
            AiComment {
                file: PathBuf::from("b.rs"),
                line_num: 2,
                text: "fix this".into(),
                action: WatchAction::CodeChange,
            },
        ];
        let (prompt, action) = build_prompt(&comments).unwrap();
        assert_eq!(action, WatchAction::CodeChange);
        assert!(prompt.contains("fix this"));
        assert!(prompt.contains("delete the AI!"));
    }

    #[test]
    fn should_ignore_target_dir() {
        assert!(should_ignore(std::path::Path::new(
            "/project/target/debug/foo"
        )));
        assert!(should_ignore(std::path::Path::new(
            "/project/node_modules/pkg"
        )));
        assert!(!should_ignore(std::path::Path::new("/project/src/main.rs")));
    }
}
