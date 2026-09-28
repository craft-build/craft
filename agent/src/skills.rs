//! Skills (J.4): compile-time bundled SKILL.md set plus filesystem discovery
//! with project > global > builtin shadowing. Surfaced through the `skill`
//! tool and `skill://` reads in the `read` tool.

use std::fs;
use std::path::{Path, PathBuf};

/// One built-in skill: `(name, SKILL.md content including frontmatter)`.
pub type BuiltinSkill = (&'static str, &'static str);

/// All built-in skills, sorted alphabetically by name. Project and global
/// skills discovered on the filesystem always shadow a built-in of the same
/// name.
pub static BUILTIN_SKILLS: &[BuiltinSkill] = &[
    (
        "agents-md-init",
        include_str!("../skills/agents-md-init/SKILL.md"),
    ),
    ("debugging", include_str!("../skills/debugging/SKILL.md")),
    ("plugin-dev", include_str!("../skills/plugin-dev/SKILL.md")),
    ("run", include_str!("../skills/run/SKILL.md")),
    ("stuck", include_str!("../skills/stuck/SKILL.md")),
    ("verify", include_str!("../skills/verify/SKILL.md")),
];

/// Look up a built-in skill by name, returning its full SKILL.md content.
pub fn builtin(name: &str) -> Option<&'static str> {
    BUILTIN_SKILLS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, content)| *content)
}

/// A discovered flat file (e.g. a recipe), named after its file stem.
/// Closer scopes shadow farther ones by name.
#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub name: String,
    pub path: PathBuf,
    pub scope: Scope,
}

/// Flat-file discovery within `<prefix>/<kind>` directories (e.g.
/// `recipes`), filtered by extension. Skills are directory-based and use
/// [`Discovery::discover_skills`] instead.
impl Discovery {
    /// Discover files named after their stems under each project
    /// ancestor's `<prefix>/<kind>` and the global config dirs, with
    /// closer scopes shadowing farther ones by name.
    pub fn discover_files(&self, kind: &str, extensions: &[&str]) -> Vec<DiscoveredFile> {
        let mut ordered = Vec::new();
        for (depth, ancestor) in self.cwd.ancestors().enumerate() {
            for prefix in PROJECT_PREFIXES {
                let dir = ancestor.join(prefix).join(kind);
                collect_files(&dir, Scope::Project(depth), extensions, &mut ordered);
            }
        }
        for dir in self.global_dirs_for(kind) {
            collect_files(&dir, Scope::Global, extensions, &mut ordered);
        }
        dedupe_files_by_name(ordered)
    }

    fn global_dirs_for(&self, kind: &str) -> Vec<PathBuf> {
        crate::paths::config_search_dirs_from(self.home.as_deref(), self.xdg_config.as_deref())
            .into_iter()
            .map(|dir| dir.join(kind))
            .collect()
    }
}

fn collect_files(dir: &Path, scope: Scope, extensions: &[&str], out: &mut Vec<DiscoveredFile>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !extensions.contains(&ext) {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
            continue;
        };
        if !fs::symlink_metadata(&path)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        out.push(DiscoveredFile {
            name: name.to_owned(),
            path,
            scope,
        });
    }
}

fn dedupe_files_by_name(mut files: Vec<DiscoveredFile>) -> Vec<DiscoveredFile> {
    let mut seen = std::collections::HashSet::new();
    files.retain(|f| seen.insert(f.name.clone()));
    files
}

/// Where a discovered skill lives, ordered by proximity. Closer scopes
/// shadow farther ones when two skills share the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A project-scoped directory. `depth` is the number of ancestor levels
    /// above the working directory (0 = the working directory itself).
    Project(usize),
    /// The user-global config directory (`~/.config/craft` or legacy
    /// `~/.craft`).
    Global,
    /// A skill shipped inside the binary. Lowest priority.
    Builtin,
}

impl Scope {
    pub fn is_builtin(self) -> bool {
        matches!(self, Scope::Builtin)
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredSkill {
    pub name: String,
    pub path: PathBuf,
    pub scope: Scope,
    pub content: String,
}

impl DiscoveredSkill {
    /// Display label for the skill's location: `<builtin>/...` for builtins,
    /// otherwise the discovered path rendered as-is.
    pub fn location(&self) -> String {
        if self.scope.is_builtin() {
            format!("<builtin>/skills/{}/SKILL.md", self.name)
        } else {
            self.path.display().to_string()
        }
    }

    /// The `description` field of the SKILL.md frontmatter, if present.
    pub fn description(&self) -> Option<&str> {
        frontmatter_field(&self.content, "description")
    }
}

/// Project directory prefixes that may hold a `<prefix>/skills` collection,
/// in priority order within a single scope level (earlier wins).
const PROJECT_PREFIXES: &[&str] = &[".craft", ".agents", ".claude", ".opencode"];

/// Filesystem skill discovery. Scans the working directory's ancestors, the
/// user's global config dirs, then the built-in table, with closer scopes
/// shadowing farther ones by name.
#[derive(Debug, Clone)]
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

    /// Discovery rooted at the current working directory and the user's
    /// environment.
    pub fn from_env() -> Self {
        Self::new(
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            crate::paths::home(),
            crate::paths::xdg_config_dir().ok(),
        )
    }

    /// Discover skills: each subdirectory of `<prefix>/skills` containing a
    /// `SKILL.md` is one skill named after the subdirectory. Built-in skills
    /// are appended last, so any project or global skill of the same name
    /// shadows them.
    pub fn discover_skills(&self) -> Vec<DiscoveredSkill> {
        let mut ordered = Vec::new();
        for (depth, ancestor) in self.cwd.ancestors().enumerate() {
            for prefix in PROJECT_PREFIXES {
                let dir = ancestor.join(prefix).join("skills");
                collect_dirs(&dir, Scope::Project(depth), &mut ordered);
            }
        }
        for dir in self.global_dirs() {
            collect_dirs(&dir, Scope::Global, &mut ordered);
        }
        for (name, content) in BUILTIN_SKILLS {
            ordered.push(DiscoveredSkill {
                name: (*name).to_owned(),
                path: PathBuf::from(format!("<builtin>/skills/{name}/SKILL.md")),
                scope: Scope::Builtin,
                content: (*content).to_owned(),
            });
        }
        dedupe_by_name(ordered)
    }

    /// Find a single skill by name, honoring shadowing.
    pub fn find(&self, name: &str) -> Option<DiscoveredSkill> {
        self.discover_skills()
            .into_iter()
            .find(|skill| skill.name == name)
    }

    /// Render the `<available_skills>` block used by the `skill` tool's
    /// description and its not-found error.
    pub fn skill_list(&self) -> String {
        let skills = self.discover_skills();
        if skills.is_empty() {
            return "\n\n<available_skills>\nNo skills available.\n</available_skills>".into();
        }
        let mut names: Vec<&DiscoveredSkill> = skills.iter().collect();
        names.sort_by(|a, b| a.name.cmp(&b.name));
        let lines: Vec<String> = names
            .iter()
            .map(|skill| {
                format!(
                    "- {}: {}",
                    skill.name,
                    skill.description().unwrap_or("(no description)")
                )
            })
            .collect();
        format!(
            "\n\n<available_skills>\n{}\n</available_skills>",
            lines.join("\n")
        )
    }

    fn global_dirs(&self) -> Vec<PathBuf> {
        crate::paths::config_search_dirs_from(self.home.as_deref(), self.xdg_config.as_deref())
            .into_iter()
            .map(|dir| dir.join("skills"))
            .collect()
    }
}

/// Extract a top-level `key: value` field from a leading `---`-fenced
/// frontmatter block. Best-effort: no YAML dependency, no nesting.
fn frontmatter_field<'a>(content: &'a str, key: &str) -> Option<&'a str> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    rest[..end]
        .lines()
        .find_map(|line| {
            let value = line.strip_prefix(key)?.strip_prefix(':')?;
            Some(value.trim())
        })
        .filter(|value| !value.is_empty())
}

fn collect_dirs(dir: &Path, scope: Scope, out: &mut Vec<DiscoveredSkill>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Reject symlinks (skill dirs and markers) so a skill bundle cannot
        // point at files outside the discovered skill roots.
        if !fs::symlink_metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let marker = path.join("SKILL.md");
        if !fs::symlink_metadata(&marker)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(content) = fs::read_to_string(&marker) else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        out.push(DiscoveredSkill {
            name: name.to_owned(),
            path: marker,
            scope,
            content,
        });
    }
}

fn dedupe_by_name(mut skills: Vec<DiscoveredSkill>) -> Vec<DiscoveredSkill> {
    let mut seen = std::collections::HashSet::new();
    skills.retain(|skill| seen.insert(skill.name.clone()));
    skills
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
    fn all_builtin_skills_have_frontmatter_and_body() {
        for (name, content) in BUILTIN_SKILLS {
            assert!(
                content.starts_with("---\n"),
                "{name}: missing opening frontmatter fence"
            );
            assert!(
                content.contains("\n---\n"),
                "{name}: missing closing frontmatter fence"
            );
            assert!(
                content.contains(&format!("name: {name}")),
                "{name}: frontmatter name does not match directory name"
            );
        }
    }

    #[test]
    fn builtins_sorted_alphabetically() {
        let names: Vec<&str> = BUILTIN_SKILLS.iter().map(|(n, _)| *n).collect();
        let mut expected = names.clone();
        expected.sort();
        assert_eq!(names, expected);
    }

    #[test]
    fn builtin_lookup_hits_and_misses() {
        assert!(builtin("run").is_some());
        assert!(builtin("does-not-exist").is_none());
    }

    #[test]
    fn discover_uses_marker_file() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join(".craft/skills/audit/SKILL.md"),
            "---\nname: audit\ndescription: audits\n---\nbody",
        );
        write(
            &tmp.path().join(".craft/skills/no-skill/README.md"),
            "not a skill",
        );

        let found = Discovery::new(tmp.path().to_path_buf(), None, None).discover_skills();
        let audit = found
            .iter()
            .find(|s| s.name == "audit")
            .expect("audit found");
        assert!(audit.path.ends_with("audit/SKILL.md"));
        assert!(!audit.scope.is_builtin());
        assert!(found.iter().all(|s| s.name != "no-skill"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_skill_dirs_and_markers_are_rejected() {
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        write(&outside.join("SKILL.md"), "secret");
        let skills = tmp.path().join(".craft/skills");
        fs::create_dir_all(&skills).unwrap();
        std::os::unix::fs::symlink(&outside, skills.join("linked-dir")).unwrap();

        let marker_dir = skills.join("real");
        fs::create_dir_all(&marker_dir).unwrap();
        std::os::unix::fs::symlink(outside.join("SKILL.md"), marker_dir.join("SKILL.md")).unwrap();

        let found = Discovery::new(tmp.path().to_path_buf(), None, None).discover_skills();
        assert!(
            found
                .iter()
                .all(|s| !s.scope.is_builtin() || s.name != "linked-dir")
        );
        assert!(found.iter().all(|s| s.name != "linked-dir"));
        assert!(found.iter().all(|s| s.name != "real"));
    }

    #[test]
    fn project_shadows_global_and_builtin() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("proj");
        let nested = project.join("deep");
        fs::create_dir_all(&nested).unwrap();
        let global = tmp.path().join("home/.config/craft/skills");
        fs::create_dir_all(&global).unwrap();

        write(&project.join(".craft/skills/audit/SKILL.md"), "project");
        write(&global.join("audit/SKILL.md"), "global");
        write(&project.join(".craft/skills/run/SKILL.md"), "project run");

        let found = Discovery::new(nested, Some(tmp.path().join("home")), None).discover_skills();
        let audit = found.iter().find(|s| s.name == "audit").unwrap();
        assert_eq!(audit.content, "project");
        assert_eq!(audit.scope, Scope::Project(1));
        let run = found.iter().find(|s| s.name == "run").unwrap();
        assert_eq!(run.content, "project run");
        assert!(!run.scope.is_builtin());
        assert_eq!(found.iter().filter(|s| s.name == "run").count(), 1);
    }

    #[test]
    fn global_returned_when_no_project_skill() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("home/.craft/skills");
        fs::create_dir_all(&global).unwrap();
        write(&global.join("only/SKILL.md"), "global body");

        let found = Discovery::new(tmp.path().join("proj"), Some(tmp.path().join("home")), None)
            .discover_skills();
        let only = found.iter().find(|s| s.name == "only").unwrap();
        assert_eq!(only.scope, Scope::Global);
        assert_eq!(only.content, "global body");
    }

    #[test]
    fn prefix_order_wins_within_a_scope() {
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join(".craft/skills/dup/SKILL.md"), "craft wins");
        write(&tmp.path().join(".claude/skills/dup/SKILL.md"), "claude");

        let found = Discovery::new(tmp.path().to_path_buf(), None, None).discover_skills();
        let dup = found.iter().find(|s| s.name == "dup").unwrap();
        assert_eq!(dup.content, "craft wins");
    }

    #[test]
    fn builtins_appended_when_nothing_shadows() {
        let tmp = TempDir::new().unwrap();
        let found = Discovery::new(tmp.path().to_path_buf(), None, None).discover_skills();
        let builtin_names: Vec<&str> = found
            .iter()
            .filter(|s| s.scope.is_builtin())
            .map(|s| s.name.as_str())
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
    fn skill_list_includes_descriptions_sorted() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join(".craft/skills/audit/SKILL.md"),
            "---\nname: audit\ndescription: audits things\n---\nbody",
        );
        let list = Discovery::new(tmp.path().to_path_buf(), None, None).skill_list();
        let audit = list.find("- audit: audits things").expect("audit listed");
        let verify = list.find("- verify:").expect("builtin verify listed");
        assert!(audit < verify || list.find("- agents-md-init:").unwrap() < audit);
    }

    #[test]
    fn frontmatter_field_extracts_description() {
        let content = "---\nname: x\ndescription: does things\n---\nbody";
        assert_eq!(
            frontmatter_field(content, "description"),
            Some("does things")
        );
        assert_eq!(frontmatter_field(content, "name"), Some("x"));
        assert_eq!(frontmatter_field("no frontmatter", "description"), None);
    }

    #[test]
    fn discover_files_closest_scope_first() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("proj");
        let nested = project.join("nested");
        fs::create_dir_all(&nested).unwrap();
        write(&project.join(".craft/recipes/audit.yaml"), "project: 1");
        write(&tmp.path().join(".craft/recipes/audit.yaml"), "ancestor: 1");

        let found =
            Discovery::new(nested, None, None).discover_files("recipes", &["yaml", "yml", "json"]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "audit");
        assert!(found[0].path.ends_with("proj/.craft/recipes/audit.yaml"));
        assert_eq!(found[0].scope, Scope::Project(1));
    }

    #[test]
    fn discover_files_project_shadows_global() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("proj");
        fs::create_dir_all(&project).unwrap();
        let global = tmp.path().join("home/.config/craft/recipes");
        fs::create_dir_all(&global).unwrap();

        write(
            &project.join(".craft/recipes/release.yaml"),
            "project version",
        );
        write(&global.join("release.yaml"), "global version");

        let found = Discovery::new(project, Some(tmp.path().join("home")), None)
            .discover_files("recipes", &["yaml", "yml", "json"]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "release");
        assert_eq!(found[0].scope, Scope::Project(0));
    }

    #[test]
    fn discover_files_supports_multiple_extensions() {
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join(".craft/recipes/a.yaml"), "yaml");
        write(&tmp.path().join(".craft/recipes/b.yml"), "yml");
        write(&tmp.path().join(".craft/recipes/c.json"), "json");
        write(&tmp.path().join(".craft/recipes/d.txt"), "ignored");

        let found = Discovery::new(tmp.path().to_path_buf(), None, None)
            .discover_files("recipes", &["yaml", "yml", "json"]);
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert!(names.contains(&"c"));
        assert!(!names.contains(&"d"));
    }
}
