//! Git-backed workspace checkpoints that do not disturb the user's index.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::config::{ProjectAgentConfig, TransportConfig};

#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub label: String,
    pub commit: String,
}

pub struct CheckpointManager {
    config: ProjectAgentConfig,
}

impl CheckpointManager {
    pub fn new(config: ProjectAgentConfig) -> Self {
        Self { config }
    }

    /// Snapshot tracked and untracked, non-ignored files through a temporary
    /// Git index. The real index and working tree are not changed.
    pub fn create(&self, label: &str) -> Result<Checkpoint, String> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let index = format!(".git/forge/checkpoint-index-{id}");
        let quoted_index = shell_words::quote(&index);
        let quoted_label = shell_words::quote(label);
        let script = format!(
            "set -eu; \
             mkdir -p .git/forge; \
             trap 'rm -f {quoted_index} {quoted_index}.lock' EXIT; \
             GIT_INDEX_FILE={quoted_index} git read-tree --empty; \
             GIT_INDEX_FILE={quoted_index} git add -A; \
             tree=$(GIT_INDEX_FILE={quoted_index} git write-tree); \
             parent=$(git rev-parse -q --verify HEAD || true); \
             if [ -n \"$parent\" ]; then \
               commit=$(printf '%s\\n' {quoted_label} | GIT_AUTHOR_NAME=Forge GIT_AUTHOR_EMAIL=forge@localhost GIT_COMMITTER_NAME=Forge GIT_COMMITTER_EMAIL=forge@localhost git commit-tree \"$tree\" -p \"$parent\"); \
             else \
               commit=$(printf '%s\\n' {quoted_label} | GIT_AUTHOR_NAME=Forge GIT_AUTHOR_EMAIL=forge@localhost GIT_COMMITTER_NAME=Forge GIT_COMMITTER_EMAIL=forge@localhost git commit-tree \"$tree\"); \
             fi; \
             git update-ref refs/forge/checkpoints/{id} \"$commit\"; \
             printf '%s' \"$commit\""
        );
        let commit = self.run_shell(&script)?;
        Ok(Checkpoint {
            label: label.into(),
            commit: commit.trim().into(),
        })
    }

    /// Restore the checkpoint into the workspace and index. If the workspace
    /// is dirty, first preserve it as a recoverable "Before restore" ref.
    pub fn restore(&self, checkpoint: &Checkpoint) -> Result<(), String> {
        let status = self.run_git(&["status", "--porcelain"])?;
        if !status.trim().is_empty() {
            self.create("Before checkpoint restore")?;
        }
        self.run_git(&["clean", "-fd"])?;
        self.run_git(&["read-tree", "--reset", "-u", &checkpoint.commit])?;
        Ok(())
    }

    fn run_git(&self, args: &[&str]) -> Result<String, String> {
        let command = format!(
            "git {}",
            args.iter()
                .map(|arg| shell_words::quote(arg))
                .collect::<Vec<_>>()
                .join(" ")
        );
        self.run_shell(&command)
    }

    fn run_shell(&self, script: &str) -> Result<String, String> {
        let output = match &self.config.transport {
            TransportConfig::Local => Command::new("sh")
                .arg("-lc")
                .arg(script)
                .current_dir(expand_home(&self.config.workspace))
                .output(),
            TransportConfig::Ssh {
                host,
                user,
                identity_file,
            } => {
                let destination = user
                    .as_deref()
                    .filter(|user| !user.is_empty())
                    .map(|user| format!("{user}@{host}"))
                    .unwrap_or_else(|| host.clone());
                let remote = format!(
                    "cd {} && sh -lc {}",
                    shell_words::quote(&self.config.workspace.to_string_lossy()),
                    shell_words::quote(script)
                );
                let mut command = Command::new("ssh");
                command.args(["-o", "BatchMode=yes"]);
                if let Some(key) = identity_file {
                    command.arg("-i").arg(key);
                }
                command.arg(destination).arg("--").arg(remote).output()
            }
        }
        .map_err(|error| format!("failed to run checkpoint command: {error}"))?;
        output_text(output)
    }
}

fn output_text(output: Output) -> Result<String, String> {
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|_| "checkpoint command returned non-UTF-8 output".into())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "checkpoint command failed ({}): {}",
            output.status,
            stderr.trim()
        ))
    }
}

fn expand_home(path: &Path) -> PathBuf {
    let mut components = path.components();
    if components
        .next()
        .is_some_and(|part| part.as_os_str() == "~")
        && let Some(home) = std::env::var_os("HOME")
    {
        let mut expanded = PathBuf::from(home);
        expanded.extend(components.map(|part| OsString::from(part.as_os_str())));
        return expanded;
    }
    path.into()
}
