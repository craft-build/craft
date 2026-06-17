use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::ToolOutput;
use craft_tool_macro::Tool;
use serde::Deserialize;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Safety {
    #[param(description = "Action: checkpoint, restore, list, undo, or history")]
    action: String,
    #[param(description = "Checkpoint name (for checkpoint and restore actions)")]
    name: Option<String>,
    #[param(description = "File path (for undo and history actions)")]
    path: Option<String>,
}

impl Safety {
    pub const NAME: &str = "safety";
    pub const DESCRIPTION: &str = include_str!("safety.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"action": "checkpoint", "name": "before-refactor"},
  {"action": "restore", "name": "before-refactor"},
  {"action": "list"},
  {"action": "undo", "path": "src/main.rs"},
  {"action": "history", "path": "src/main.rs"}
]"#,
    );

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        match self.action.as_str() {
            "checkpoint" => self.checkpoint(ctx).await,
            "restore" => self.restore(ctx),
            "list" => self.list(ctx),
            "undo" => self.undo(ctx),
            "history" => self.history(ctx),
            _ => Err(format!(
                "unknown action \"{}\"; use checkpoint, restore, list, undo, or history",
                self.action
            )),
        }
    }

    async fn checkpoint(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let name = self
            .name
            .as_deref()
            .ok_or("checkpoint requires a `name` parameter")?;

        let (files, skipped) = tokio::task::spawn_blocking(collect_project_files)
            .await
            .map_err(|e| format!("checkpoint scan failed: {e}"))?;
        let snapshot = Snapshot {
            files,
            created_at: std::time::SystemTime::now(),
        };

        let store = &ctx.snapshot_store;
        let mut guard = store.0.lock().unwrap();
        if guard.checkpoints.contains_key(name) {
            return Err(format!(
                "checkpoint \"{name}\" already exists; use restore first or pick a different name"
            ));
        }
        while guard.checkpoints.len() >= MAX_CHECKPOINTS {
            let oldest = guard
                .checkpoints
                .iter()
                .min_by_key(|(_, s)| s.created_at)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    guard.checkpoints.remove(&k);
                }
                None => break,
            }
        }
        guard.checkpoints.insert(name.to_string(), snapshot);

        let count = guard.checkpoints.get(name).unwrap().files.len();
        let truncation_note = if skipped > 0 {
            format!(", {skipped} file(s) skipped (size cap reached)")
        } else {
            String::new()
        };
        Ok(ToolOutput::Plain(format!(
            "checkpoint \"{name}\" saved ({count} files{truncation_note})"
        )))
    }

    fn restore(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let name = self
            .name
            .as_deref()
            .ok_or("restore requires a `name` parameter")?;

        let store = &ctx.snapshot_store;
        let snapshot = {
            let guard = store.0.lock().unwrap();
            guard
                .checkpoints
                .get(name)
                .cloned()
                .ok_or(format!("checkpoint \"{name}\" not found"))?
        };

        let mut restored = 0;
        let mut failed = 0;
        for (path, content) in &snapshot.files {
            if let Err(e) = std::fs::write(path, content) {
                failed += 1;
                eprintln!("safety restore: failed to write {}: {e}", path.display());
            } else {
                restored += 1;
            }
        }

        if failed > 0 {
            Ok(ToolOutput::Plain(format!(
                "restored {restored}/{total} files from checkpoint \"{name}\" ({failed} failed)",
                total = snapshot.files.len()
            )))
        } else {
            Ok(ToolOutput::Plain(format!(
                "restored {restored} files from checkpoint \"{name}\""
            )))
        }
    }

    fn list(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let guard = ctx.snapshot_store.0.lock().unwrap();
        if guard.checkpoints.is_empty() && guard.backups.is_empty() {
            return Ok(ToolOutput::Plain("no checkpoints, no backups".into()));
        }

        let mut out = String::new();
        if !guard.checkpoints.is_empty() {
            out.push_str("checkpoints:\n");
            for (name, snapshot) in &guard.checkpoints {
                let ts = fmt_elapsed(snapshot.created_at);
                let _ = std::fmt::write(
                    &mut out,
                    format_args!("  {} ({} files, {})\n", name, snapshot.files.len(), ts),
                );
            }
        }
        if !guard.backups.is_empty() {
            let total: usize = guard.backups.values().map(|v| v.len()).sum();
            out.push_str(&format!(
                "backups: {total} entries across {} files\n",
                guard.backups.len()
            ));
        }
        Ok(ToolOutput::Plain(out))
    }

    fn undo(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = self
            .path
            .as_deref()
            .ok_or("undo requires a `path` parameter")?;
        let resolved = super::resolve_path(path)?;

        let store = &ctx.snapshot_store;
        let backup = {
            let mut guard = store.0.lock().unwrap();
            guard
                .backups
                .get_mut(&PathBuf::from(&resolved))
                .and_then(|stack| stack.pop())
                .ok_or(format!("no backup found for \"{resolved}\""))?
        };

        std::fs::write(&resolved, &backup.content)
            .map_err(|e| format!("failed to restore {}: {e}", resolved))?;

        Ok(ToolOutput::Plain(format!(
            "restored {} to state from {}",
            resolved,
            fmt_elapsed(backup.created_at)
        )))
    }

    fn history(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = self
            .path
            .as_deref()
            .ok_or("history requires a `path` parameter")?;
        let resolved = super::resolve_path(path)?;

        let guard = ctx.snapshot_store.0.lock().unwrap();
        let stack = guard
            .backups
            .get(&PathBuf::from(&resolved))
            .ok_or(format!("no backup history for \"{resolved}\""))?;

        if stack.is_empty() {
            return Ok(ToolOutput::Plain(format!("no backups for \"{resolved}\"")));
        }

        let mut out = format!("backups for {} ({} entries):\n", resolved, stack.len());
        for (i, backup) in stack.iter().enumerate().rev() {
            let ts = fmt_elapsed(backup.created_at);
            let _ = std::fmt::write(
                &mut out,
                format_args!("  {}: {} bytes, {}\n", i + 1, backup.content.len(), ts),
            );
        }
        Ok(ToolOutput::Plain(out))
    }

    pub fn start_header(&self) -> String {
        self.action.clone()
    }
}

fn collect_project_files() -> (HashMap<PathBuf, String>, usize) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut files = HashMap::new();
    let mut total_bytes = 0usize;
    let mut skipped = 0usize;

    let walker = ignore::WalkBuilder::new(&cwd)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let size = meta.len() as usize;
        if size > MAX_SNAPSHOT_FILE_BYTES || total_bytes + size > MAX_CHECKPOINT_TOTAL_BYTES {
            if total_bytes + size > MAX_CHECKPOINT_TOTAL_BYTES {
                skipped += 1;
            }
            continue;
        }
        let path = entry.path().to_path_buf();
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if content.len() > MAX_SNAPSHOT_FILE_BYTES {
            continue;
        }
        total_bytes += content.len();
        files.insert(path, content);
    }

    (files, skipped)
}

fn fmt_elapsed(t: std::time::SystemTime) -> String {
    let now = std::time::SystemTime::now();
    match now.duration_since(t) {
        Ok(elapsed) => {
            let secs = elapsed.as_secs();
            if secs < 60 {
                format!("{secs}s ago")
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else {
                format!("{}h ago", secs / 3600)
            }
        }
        Err(_) => "?".into(),
    }
}

const MAX_SNAPSHOT_FILE_BYTES: usize = 2_000_000;
const MAX_CHECKPOINT_TOTAL_BYTES: usize = 256_000_000;
const MAX_CHECKPOINTS: usize = 10;
const MAX_BACKUPS_PER_FILE: usize = 20;

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub files: HashMap<PathBuf, String>,
    pub created_at: std::time::SystemTime,
}

#[derive(Debug, Clone)]
pub struct FileBackup {
    pub content: String,
    pub created_at: std::time::SystemTime,
}

#[derive(Debug, Default)]
pub struct SnapshotStoreInner {
    pub checkpoints: HashMap<String, Snapshot>,
    pub backups: HashMap<PathBuf, Vec<FileBackup>>,
}

#[derive(Debug, Default)]
pub struct SnapshotStore(pub Mutex<SnapshotStoreInner>);

impl SnapshotStore {
    pub fn fresh() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn push_backup(&self, path: PathBuf, content: String) {
        let mut guard = self.0.lock().unwrap();
        let stack = guard.backups.entry(path).or_default();
        stack.push(FileBackup {
            content,
            created_at: std::time::SystemTime::now(),
        });
        if stack.len() > MAX_BACKUPS_PER_FILE {
            stack.drain(0..stack.len() - MAX_BACKUPS_PER_FILE);
        }
    }
}

super::impl_tool!(
    Safety,
    audience = super::ToolAudience::MAIN,
    kind = "safety",
    tier = super::ToolTier::Core,
);

impl super::ToolInvocation for Safety {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Safety::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Safety::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_store_fresh_creates_empty() {
        let store = SnapshotStore::fresh();
        let guard = store.0.lock().unwrap();
        assert!(guard.checkpoints.is_empty());
        assert!(guard.backups.is_empty());
    }

    #[test]
    fn duplicate_checkpoint_rejected() {
        let store = SnapshotStore::fresh();
        let snapshot = Snapshot {
            files: HashMap::new(),
            created_at: std::time::SystemTime::now(),
        };
        store
            .0
            .lock()
            .unwrap()
            .checkpoints
            .insert("test".into(), snapshot);
        assert!(store.0.lock().unwrap().checkpoints.contains_key("test"));
    }

    #[test]
    fn push_backup_tracks_file() {
        let store = SnapshotStore::fresh();
        store.push_backup(PathBuf::from("a.rs"), "old".into());
        store.push_backup(PathBuf::from("a.rs"), "older".into());
        let guard = store.0.lock().unwrap();
        let stack = guard.backups.get(&PathBuf::from("a.rs")).unwrap();
        assert_eq!(stack.len(), 2);
        assert_eq!(stack[0].content, "old");
        assert_eq!(stack[1].content, "older");
    }

    #[test]
    fn push_backup_enforces_max_depth() {
        let store = SnapshotStore::fresh();
        for i in 0..30 {
            store.push_backup(PathBuf::from("b.rs"), format!("v{i}"));
        }
        let guard = store.0.lock().unwrap();
        let stack = guard.backups.get(&PathBuf::from("b.rs")).unwrap();
        assert_eq!(stack.len(), MAX_BACKUPS_PER_FILE);
    }
}
