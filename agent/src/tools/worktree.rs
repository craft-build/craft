//! Git worktree isolation for subagents (A.5 `task` tool).
//!
//! Creates a linked git worktree so a subagent can mutate files without
//! affecting the parent's working tree. The worktree shares object storage
//! with the main repo (cheap), and is cleaned up when dropped. Creation
//! fails explicitly when the root is not a git repository or `git worktree
//! add` fails: isolation must never degrade into silently editing the
//! parent tree.
//!
//! Ported from the reference `craft-agent/src/tools/worktree.rs`.

use std::path::PathBuf;
use std::process::Command;

use super::error::{Result, invalid};
use tracing::{debug, warn};

const WORKTREE_BRANCH_PREFIX: &str = "craft-subagent/";

/// A linked git worktree. `Drop` removes the worktree and its branch.
pub struct Worktree {
    path: PathBuf,
    branch: String,
}

impl Worktree {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Create a linked worktree from the repo rooted at `cwd`. Fails with
    /// the reason when git is missing, the root is not a git repo, or
    /// `git worktree add` rejects the request — the caller must not fall
    /// back to running an isolated subagent in the parent tree.
    pub fn create(cwd: &std::path::Path, label: &str) -> Result<Self> {
        if !cwd.is_dir() {
            return Err(invalid(format!("{} is not a directory", cwd.display())));
        }
        let toplevel = git_out(cwd, &["rev-parse", "--show-toplevel"])
            .ok_or_else(|| invalid(format!("{} is not a git repository", cwd.display())))?;
        let toplevel = PathBuf::from(toplevel.trim());

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let slug = slugify(label);
        let branch = format!("{WORKTREE_BRANCH_PREFIX}{slug}-{stamp:x}");

        let parent = std::env::temp_dir().join("craft-worktrees");
        std::fs::create_dir_all(&parent)
            .map_err(|e| invalid(format!("creating {}: {e}", parent.display())))?;
        let dir_name = format!("craft-{slug}-{stamp:x}");
        let path = parent.join(&dir_name);

        let output = Command::new("git")
            .arg("-C")
            .arg(&toplevel)
            .args(["worktree", "add", "-b"])
            .arg(&branch)
            .arg(&path)
            .arg("HEAD")
            .output()
            .map_err(|e| invalid(format!("running git worktree add: {e}")))?;
        if !output.status.success() {
            warn!(
                stderr = %String::from_utf8_lossy(&output.stderr),
                "git worktree add failed"
            );
            return Err(invalid(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        debug!(worktree = %path.display(), branch = %branch, "created worktree");
        Ok(Self { path, branch })
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let remove = Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(["worktree", "remove", "--force", "."])
            .output();
        if let Ok(out) = remove
            && !out.status.success()
        {
            debug!(
                stderr = %String::from_utf8_lossy(&out.stderr),
                "worktree remove logged (may already be gone)"
            );
        }
        let _ = Command::new("git")
            .args(["branch", "-D", &self.branch])
            .output();
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn git_out(cwd: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if out.ends_with('-') {
            continue;
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn slugify_replaces_non_alnum() {
        assert_eq!(slugify("Find auth middleware"), "find-auth-middleware");
        assert_eq!(slugify("a!!b"), "a-b");
        assert_eq!(slugify("  leading"), "leading");
    }

    #[test]
    fn create_fails_explicitly_without_git_repo() {
        if !git_available() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let error = match Worktree::create(dir.path(), "test") {
            Err(error) => error.to_string(),
            Ok(worktree) => panic!("expected failure, got {}", worktree.path().display()),
        };
        assert!(error.contains("not a git repository"), "{error}");
    }

    #[test]
    fn create_and_drop_cleans_up() {
        if !git_available() {
            return;
        }
        let repo = tempfile::TempDir::new().unwrap();
        let ok = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["init", "-q"])
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return;
        }
        std::fs::write(repo.path().join("f.txt"), "hello").unwrap();
        let ok = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["add", "."])
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok);
        let ok = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "init",
            ])
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok);

        let path;
        {
            let wt = Worktree::create(repo.path(), "isolation test").expect("worktree in git repo");
            assert!(wt.path().is_dir());
            assert!(wt.path().join("f.txt").exists());
            path = wt.path().to_path_buf();
        }
        assert!(!path.exists(), "worktree dir should be removed on drop");
    }
}
