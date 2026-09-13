//! AGENTS.md instructions discovery (ported from Craft's
//! `craft-agent/src/agent/instructions.rs`). Collects project, local, and
//! global instruction files into the text that feeds the system prompt's
//! `{{instructions}}` slot, and provides subdirectory-scoped discovery used by
//! the read tool to inject instructions on first touch.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const INSTRUCTION_FILES: &[&str] = &[
    "AGENTS.md",
    "CLAUDE.md",
    ".github/copilot-instructions.md",
    "COPILOT.md",
    ".cursorrules",
    ".windsurfrules",
    ".clinerules",
    "CONVENTIONS.md",
    "GEMINI.md",
    "CODING_AGENT.md",
];

const LOCAL_INSTRUCTION_FILE: &str = "AGENTS.local.md";
const GLOBAL_INSTRUCTION_FILE: &str = "AGENTS.md";

/// Canonical paths of instruction files already injected into the
/// conversation, so a file is only ever shown to the agent once.
#[derive(Clone, Default)]
pub struct LoadedInstructions(Arc<Mutex<HashSet<PathBuf>>>);

impl LoadedInstructions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` when `path` was already present (i.e. it should be
    /// skipped); inserts it otherwise.
    pub fn contains_or_insert(&self, path: PathBuf) -> bool {
        let mut set = self.0.lock().unwrap_or_else(|e| e.into_inner());
        !set.insert(path)
    }
}

#[derive(Default)]
pub struct Instructions {
    pub text: String,
    pub loaded: LoadedInstructions,
}

pub fn is_instruction_file(name: &str) -> bool {
    name == LOCAL_INSTRUCTION_FILE
        || INSTRUCTION_FILES
            .iter()
            .any(|f| *f == name || Path::new(f).file_name().is_some_and(|n| n == name))
}

/// Ascend from `cwd` (inclusive) up to and including the first directory
/// containing a `.git` marker, or to the filesystem root when none exists.
fn find_project_ancestor_dirs(cwd: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    let mut current = Some(cwd.to_path_buf());
    std::iter::from_fn(move || {
        let dir = current.take()?;
        if !dir.join(".git").exists() {
            current = dir.parent().map(Path::to_path_buf);
        }
        Some(dir)
    })
}

fn read_instruction(path: &Path, loaded: &LoadedInstructions) -> Option<(PathBuf, String)> {
    let canonical = path.canonicalize().ok()?;
    if loaded.contains_or_insert(canonical.clone()) {
        return None;
    }
    let content = fs::read_to_string(&canonical).ok()?;
    Some((canonical, content))
}

fn collect_instruction_files(
    cwd: &str,
    home: Option<&Path>,
    xdg_config: Option<&Path>,
    loaded: &LoadedInstructions,
) -> Vec<(String, String)> {
    let mut out = Vec::new();

    let ancestor_dirs: Vec<_> = find_project_ancestor_dirs(Path::new(cwd)).collect();
    let has_git_root = ancestor_dirs.iter().any(|dir| dir.join(".git").exists());
    let project_dirs = if has_git_root {
        ancestor_dirs
    } else {
        ancestor_dirs.into_iter().take(1).collect()
    };

    for dir in project_dirs.into_iter().rev() {
        for filename in INSTRUCTION_FILES {
            if let Some((canonical, content)) = read_instruction(&dir.join(filename), loaded) {
                out.push((
                    format!("Project instructions ({})", canonical.display()),
                    content,
                ));
                break;
            }
        }

        if let Some((canonical, content)) =
            read_instruction(&dir.join(LOCAL_INSTRUCTION_FILE), loaded)
        {
            out.push((
                format!("Local instructions ({})", canonical.display()),
                content,
            ));
        }
    }

    for dir in crate::paths::config_search_dirs_from(home, xdg_config) {
        let path = dir.join(GLOBAL_INSTRUCTION_FILE);
        if let Some((canonical, content)) = read_instruction(&path, loaded) {
            let label = format!("Global instructions ({})", canonical.display());
            out.push((label, content));
            break;
        }
    }

    out
}

pub fn load_instructions(cwd: &str) -> Instructions {
    load_instructions_with_home(
        cwd,
        crate::paths::home().as_deref(),
        crate::paths::xdg_config_dir().ok().as_deref(),
    )
}

pub(crate) fn load_instructions_with_home(
    cwd: &str,
    home: Option<&Path>,
    xdg_config: Option<&Path>,
) -> Instructions {
    let mut instr = Instructions::default();
    let files = collect_instruction_files(cwd, home, xdg_config, &instr.loaded);

    for (label, content) in files {
        instr.text.push_str(&format!("\n\n{label}:\n{content}"));
    }

    instr
}

/// Discover instruction files in directories strictly between `dir` and
/// `cwd` (walking `dir` upward, exclusive of `cwd`). Each level contributes at
/// most one file (first name match wins); already-loaded files are skipped and
/// marked loaded. Returns `(canonical_path, content)` pairs, closest first.
pub fn find_subdirectory_instructions(
    dir: &Path,
    cwd: &Path,
    loaded: &LoadedInstructions,
) -> Vec<(String, String)> {
    let Ok(cwd) = cwd.canonicalize() else {
        return Vec::new();
    };
    let Ok(dir) = dir.canonicalize() else {
        return Vec::new();
    };

    if !dir.starts_with(&cwd) || dir == cwd {
        return Vec::new();
    }

    let mut results = Vec::new();
    let mut current = dir.as_path();
    while current != cwd {
        for filename in INSTRUCTION_FILES {
            if let Some((canonical, content)) = read_instruction(&current.join(filename), loaded) {
                results.push((canonical.display().to_string(), content));
                break;
            }
        }
        current = match current.parent() {
            Some(p) => p,
            None => break,
        };
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_instruction_file_matches_direct_and_nested_names() {
        for name in [
            "AGENTS.md",
            "CLAUDE.md",
            "copilot-instructions.md",
            ".cursorrules",
            "AGENTS.local.md",
        ] {
            assert!(is_instruction_file(name), "{name} should match");
        }
        assert!(!is_instruction_file("random.md"));
        assert!(!is_instruction_file("not-AGENTS.md"));
    }

    #[test]
    fn load_instructions_merges_project_and_local() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "team rules").unwrap();
        fs::write(dir.path().join("AGENTS.local.md"), "my preferences").unwrap();

        let text = load_instructions_with_home(dir.path().to_str().unwrap(), None, None).text;
        assert!(text.contains("team rules"));
        assert!(text.contains("my preferences"));
        assert!(
            text.find("team rules").unwrap() < text.find("my preferences").unwrap(),
            "project instructions should come before local instructions"
        );
    }

    #[test]
    fn load_instructions_local_without_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.local.md"), "solo preferences").unwrap();
        assert!(
            load_instructions_with_home(dir.path().to_str().unwrap(), None, None)
                .text
                .contains("solo preferences")
        );
    }

    #[test]
    fn load_instructions_empty_when_no_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_instructions_with_home(dir.path().to_str().unwrap(), None, None)
                .text
                .is_empty()
        );
    }

    #[test]
    fn load_instructions_empty_when_home_has_no_global_file() {
        let cwd = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        assert!(
            load_instructions_with_home(cwd.path().to_str().unwrap(), Some(home.path()), None)
                .text
                .is_empty()
        );
    }

    #[test]
    fn load_instructions_includes_global_from_home() {
        let cwd = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".craft")).unwrap();
        fs::write(home.path().join(".craft").join("AGENTS.md"), "global rules").unwrap();

        let text =
            load_instructions_with_home(cwd.path().to_str().unwrap(), Some(home.path()), None).text;
        assert!(text.contains("global rules"));
    }

    #[test]
    fn load_instructions_global_search_order() {
        // The legacy `~/.craft` dir wins when both hold a file; otherwise the
        // XDG dir is searched even though the legacy dir exists.
        for (write_legacy, wanted, unwanted) in [
            (false, "xdg global rules", "legacy global rules"),
            (true, "legacy global rules", "xdg global rules"),
        ] {
            let cwd = tempfile::tempdir().unwrap();
            let home = tempfile::tempdir().unwrap();
            let xdg = tempfile::tempdir().unwrap();
            let legacy = home.path().join(".craft");
            fs::create_dir(&legacy).unwrap();
            if write_legacy {
                fs::write(legacy.join(GLOBAL_INSTRUCTION_FILE), "legacy global rules").unwrap();
            }
            fs::write(xdg.path().join(GLOBAL_INSTRUCTION_FILE), "xdg global rules").unwrap();

            let text = load_instructions_with_home(
                cwd.path().to_str().unwrap(),
                Some(home.path()),
                Some(xdg.path()),
            )
            .text;
            assert!(text.contains(wanted), "got {text}");
            assert!(!text.contains(unwanted), "got {text}");
        }
    }

    #[test]
    fn load_instructions_includes_parent_directory_instructions() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();

        let sub = dir.path().join("crates").join("a");
        fs::create_dir_all(&sub).unwrap();

        fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();
        fs::write(sub.join("AGENTS.md"), "crate rules").unwrap();

        let text = load_instructions_with_home(sub.to_str().unwrap(), None, None).text;
        assert!(
            text.contains("crate rules"),
            "should load instructions from cwd"
        );
        assert!(
            text.contains("root rules"),
            "should load instructions from project root"
        );
        assert!(
            text.find("root rules").unwrap() < text.find("crate rules").unwrap(),
            "root instructions should come before closer instructions"
        );
    }

    #[test]
    fn find_subdirectory_instructions_discovers_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("src").join("api");
        fs::create_dir_all(&sub).unwrap();
        fs::write(dir.path().join("src").join("AGENTS.md"), "api rules").unwrap();

        let loaded = LoadedInstructions::new();
        let results = find_subdirectory_instructions(&sub, dir.path(), &loaded);

        assert_eq!(results.len(), 1);
        assert!(results[0].0.ends_with("AGENTS.md"));
        assert_eq!(results[0].1, "api rules");
    }

    #[test]
    fn find_subdirectory_instructions_skips_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();

        let loaded = LoadedInstructions::new();
        let from_root = find_subdirectory_instructions(dir.path(), dir.path(), &loaded);
        assert!(from_root.is_empty(), "should skip root-level directory");
    }

    #[test]
    fn find_subdirectory_instructions_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("src");
        fs::create_dir_all(&sub).unwrap();
        let agents_path = sub.join("AGENTS.md");
        fs::write(&agents_path, "rules").unwrap();

        let canonical = agents_path.canonicalize().unwrap();
        let loaded = LoadedInstructions::new();
        loaded.contains_or_insert(canonical);
        let pre_loaded = find_subdirectory_instructions(&sub, dir.path(), &loaded);
        assert!(pre_loaded.is_empty(), "should skip already-loaded files");

        let loaded = LoadedInstructions::new();
        let first = find_subdirectory_instructions(&sub, dir.path(), &loaded);
        let second = find_subdirectory_instructions(&sub, dir.path(), &loaded);
        assert_eq!(first.len(), 1);
        assert!(
            second.is_empty(),
            "should not return same file twice across calls"
        );
    }

    #[test]
    fn load_instructions_populates_loaded_set() {
        let dir = tempfile::tempdir().unwrap();
        let agents_path = dir.path().join("AGENTS.md");
        fs::write(&agents_path, "content").unwrap();

        let instr = load_instructions_with_home(dir.path().to_str().unwrap(), None, None);
        assert!(
            instr
                .loaded
                .contains_or_insert(agents_path.canonicalize().unwrap())
        );
    }
}
