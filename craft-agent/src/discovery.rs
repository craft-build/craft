use std::fs;
use std::path::{Path, PathBuf};

use craft_storage::paths;

/// Where a discovered file lives, ordered by proximity. Closer scopes shadow
/// farther ones when two files share the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A project-scoped directory. `depth` is the number of ancestor levels above
    /// the working directory (0 = the working directory itself).
    Project(usize),
    /// The user-global config directory (`~/.config/craft` or legacy `~/.craft`).
    Global,
    /// A skill shipped inside the craft binary itself. Lowest priority: any
    /// project or global skill of the same name shadows it.
    Builtin,
}

impl Scope {
    pub fn is_global(self) -> bool {
        matches!(self, Scope::Global)
    }

    pub fn is_builtin(self) -> bool {
        matches!(self, Scope::Builtin)
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub name: String,
    pub path: PathBuf,
    pub scope: Scope,
    pub content: String,
}

/// Project directory prefixes that may hold a `<prefix>/<kind>` collection, in
/// priority order within a single scope level (earlier wins).
const PROJECT_PREFIXES: &[&str] = &[".craft", ".agents", ".claude", ".opencode"];

/// Unified project-scoped discovery for checks, recipes, and skills. Searches
/// from the working directory up to the filesystem root plus the user-global
/// config directory, with closer scopes shadowing farther ones by name.
pub struct Discovery {
    cwd: PathBuf,
    home: Option<PathBuf>,
    xdg_config: Option<PathBuf>,
}

impl Discovery {
    pub fn new(cwd: PathBuf, home: Option<PathBuf>, xdg_config: Option<PathBuf>) -> Self {
        Self {
            cwd,
            home,
            xdg_config,
        }
    }

    /// Discovery rooted at the current working directory and the user's home.
    pub fn from_env() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let home = paths::home();
        let xdg_config = paths::config_dir().ok();
        Self::new(cwd, home, xdg_config)
    }

    /// The working directory discovery is rooted at.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Closest ancestor of the working directory (inclusive) containing a `.git`
    /// marker, or the working directory itself when none is found.
    pub fn project_root(&self) -> PathBuf {
        for ancestor in self.cwd.ancestors() {
            if ancestor.join(".git").exists() {
                return ancestor.to_path_buf();
            }
        }
        self.cwd.clone()
    }

    /// Discover file-based items (recipes, checks) named by file stem, matching
    /// any of `extensions` (e.g. `["yaml", "yml", "json"]`). Results are ordered
    /// closest scope first; a name only appears once (closest wins).
    pub fn discover_files(&self, kind: &str, extensions: &[&str]) -> Vec<DiscoveredFile> {
        let mut ordered: Vec<DiscoveredFile> = Vec::new();
        for (depth, ancestor) in self.cwd.ancestors().enumerate() {
            for prefix in PROJECT_PREFIXES {
                let dir = ancestor.join(prefix).join(kind);
                self.collect_files(&dir, Scope::Project(depth), extensions, &mut ordered);
            }
        }
        for dir in self.global_dirs(kind) {
            self.collect_files(&dir, Scope::Global, extensions, &mut ordered);
        }
        dedupe_by_name(ordered)
    }

    /// Discover directory-based items (skills) where each subdirectory of
    /// `<prefix>/<kind>` containing `marker` (e.g. `SKILL.md`) is one item named
    /// after the subdirectory. Closer scopes shadow farther ones by name.
    ///
    /// Built-in skills (embedded in the binary) are appended last, so any
    /// project or global skill of the same name shadows them. Only applies to
    /// the `skills` kind with the `SKILL.md` marker.
    pub fn discover_dirs(&self, kind: &str, marker: &str) -> Vec<DiscoveredFile> {
        let mut ordered: Vec<DiscoveredFile> = Vec::new();
        for (depth, ancestor) in self.cwd.ancestors().enumerate() {
            for prefix in PROJECT_PREFIXES {
                let dir = ancestor.join(prefix).join(kind);
                self.collect_dirs(&dir, marker, Scope::Project(depth), &mut ordered);
            }
        }
        for dir in self.global_dirs(kind) {
            self.collect_dirs(&dir, marker, Scope::Global, &mut ordered);
        }
        if kind == "skills" && marker == "SKILL.md" {
            self.collect_builtins(&mut ordered);
        }
        dedupe_by_name(ordered)
    }

    fn collect_builtins(&self, out: &mut Vec<DiscoveredFile>) {
        for (name, content) in crate::builtin_skills::BUILTIN_SKILLS {
            out.push(DiscoveredFile {
                name: (*name).to_owned(),
                path: PathBuf::from(format!("<builtin>/skills/{name}/SKILL.md")),
                scope: Scope::Builtin,
                content: (*content).to_owned(),
            });
        }
    }

    fn global_dirs(&self, kind: &str) -> Vec<PathBuf> {
        paths::user_config_dirs(self.home.as_deref(), self.xdg_config.as_deref(), kind)
    }

    fn collect_files(
        &self,
        dir: &Path,
        scope: Scope,
        extensions: &[&str],
        out: &mut Vec<DiscoveredFile>,
    ) {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            if !extensions.contains(&ext) {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            out.push(DiscoveredFile {
                name,
                path,
                scope,
                content,
            });
        }
    }

    fn collect_dirs(&self, dir: &Path, marker: &str, scope: Scope, out: &mut Vec<DiscoveredFile>) {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            let marker_path = path.join(marker);
            let content = match fs::read_to_string(&marker_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            out.push(DiscoveredFile {
                name,
                path: marker_path,
                scope,
                content,
            });
        }
    }
}

fn dedupe_by_name(mut files: Vec<DiscoveredFile>) -> Vec<DiscoveredFile> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    files.retain(|f| seen.insert(f.name.clone()));
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn discover_files_closest_scope_first() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let project = root.join("proj");
        let nested = project.join("nested");
        fs::create_dir_all(&nested).unwrap();
        write(&project.join(".craft/recipes/audit.yaml"), "project: 1");
        write(&root.join(".craft/recipes/audit.yaml"), "ancestor: 1");

        let discovery = Discovery::new(nested.clone(), None, None);
        let found = discovery.discover_files("recipes", &["yaml", "yml", "json"]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "audit");
        assert!(found[0].path.ends_with("proj/.craft/recipes/audit.yaml"));
        assert_eq!(found[0].scope, Scope::Project(1));
    }

    #[test]
    fn project_shadows_global() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let project = root.join("proj");
        fs::create_dir_all(&project).unwrap();
        let global = root.join("home/.config/craft/recipes");
        fs::create_dir_all(&global).unwrap();

        write(
            &project.join(".craft/recipes/release.yaml"),
            "project version",
        );
        write(&global.join("release.yaml"), "global version");

        let discovery = Discovery::new(project.clone(), Some(root.join("home")), None);
        let found = discovery.discover_files("recipes", &["yaml", "yml", "json"]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].content, "project version");
        assert_eq!(found[0].scope, Scope::Project(0));
    }

    #[test]
    fn global_returned_when_no_project_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let project = root.join("proj");
        fs::create_dir_all(&project).unwrap();
        let global = root.join("home/.craft/recipes");
        fs::create_dir_all(&global).unwrap();

        write(&global.join("only.yaml"), "global");

        let discovery = Discovery::new(project, Some(root.join("home")), None);
        let found = discovery.discover_files("recipes", &["yaml", "yml", "json"]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "only");
        assert!(found[0].scope.is_global());
    }

    #[test]
    fn discover_files_supports_multiple_extensions() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join(".craft/recipes/a.yaml"), "yaml");
        write(&root.join(".craft/recipes/b.yml"), "yml");
        write(&root.join(".craft/recipes/c.json"), "json");
        write(&root.join(".craft/recipes/d.txt"), "ignored");

        let discovery = Discovery::new(root.to_path_buf(), None, None);
        let found = discovery.discover_files("recipes", &["yaml", "yml", "json"]);
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert!(names.contains(&"c"));
        assert!(!names.contains(&"d"));
    }

    #[test]
    fn discover_dirs_uses_marker_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join(".craft/skills/audit/SKILL.md"),
            "name: audit\ndescription: audits",
        );
        write(
            &root.join(".craft/skills/no-skill/README.md"),
            "not a skill",
        );

        let discovery = Discovery::new(root.to_path_buf(), None, None);
        let found = discovery.discover_dirs("skills", "SKILL.md");
        let audit = found
            .iter()
            .find(|f| f.name == "audit")
            .expect("audit skill found");
        assert!(audit.path.ends_with("audit/SKILL.md"));
        assert!(!audit.scope.is_builtin());
        assert!(found.iter().all(|f| f.name != "no-skill"));
    }

    #[test]
    fn discover_dirs_shadows_by_name() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let project = root.join("proj");
        let nested = project.join("deep");
        fs::create_dir_all(&nested).unwrap();
        let global = root.join("home/.config/craft/skills");
        fs::create_dir_all(&global).unwrap();

        write(&project.join(".craft/skills/audit/SKILL.md"), "project");
        write(&global.join("audit/SKILL.md"), "global");

        let discovery = Discovery::new(nested, Some(root.join("home")), None);
        let found = discovery.discover_dirs("skills", "SKILL.md");
        let audit = found
            .iter()
            .find(|f| f.name == "audit")
            .expect("audit skill found");
        assert_eq!(audit.content, "project");
    }

    #[test]
    fn project_root_finds_git_marker() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let project = root.join("repo");
        let nested = project.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("repo/.git"), "gitdir: /tmp").unwrap();

        let discovery = Discovery::new(nested, None, None);
        assert_eq!(discovery.project_root(), project);
    }

    #[test]
    fn project_root_defaults_to_cwd_without_git() {
        let tmp = TempDir::new().unwrap();
        let discovery = Discovery::new(tmp.path().to_path_buf(), None, None);
        assert_eq!(discovery.project_root(), tmp.path());
    }

    #[test]
    fn empty_when_nothing_found() {
        let tmp = TempDir::new().unwrap();
        let discovery = Discovery::new(tmp.path().to_path_buf(), None, None);
        assert!(
            discovery
                .discover_files("recipes", &["yaml", "yml", "json"])
                .is_empty()
        );
        assert!(discovery.discover_dirs("recipes", "SKILL.md").is_empty());
    }

    #[test]
    fn builtins_appended_for_skills() {
        let tmp = TempDir::new().unwrap();
        let discovery = Discovery::new(tmp.path().to_path_buf(), None, None);
        let found = discovery.discover_dirs("skills", "SKILL.md");
        let builtin_names: Vec<&str> = found
            .iter()
            .filter(|f| f.scope.is_builtin())
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(
            builtin_names,
            vec![
                "agents-md-init",
                "debugging",
                "plugin-dev",
                "run",
                "stuck",
                "verify",
            ]
        );
    }

    #[test]
    fn project_skill_shadows_builtin() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join(".craft/skills/run/SKILL.md"),
            "---\nname: run\ndescription: project override\n---\nproject body",
        );

        let discovery = Discovery::new(root.to_path_buf(), None, None);
        let found = discovery.discover_dirs("skills", "SKILL.md");
        let run = found
            .iter()
            .find(|f| f.name == "run")
            .expect("run skill found");
        assert_eq!(
            run.content,
            "---\nname: run\ndescription: project override\n---\nproject body"
        );
        assert!(!run.scope.is_builtin());
        assert_eq!(
            found.iter().filter(|f| f.name == "run").count(),
            1,
            "builtin run must be shadowed, not duplicated"
        );
    }

    #[test]
    fn builtins_not_appended_for_other_kinds() {
        let tmp = TempDir::new().unwrap();
        let discovery = Discovery::new(tmp.path().to_path_buf(), None, None);
        assert!(discovery.discover_dirs("recipes", "SKILL.md").is_empty());
        assert!(discovery.discover_dirs("skills", "RECIPE.md").is_empty());
    }
}
