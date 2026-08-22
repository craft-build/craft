//! On-disk skill storage for the desktop Skills section.
//!
//! A skill is a directory `<skills-dir>/<name>/SKILL.md` with YAML frontmatter
//! (`name`, `description`, `when_to_use`) and a markdown body. Discovery rules
//! intentionally mirror `craft-agent`'s `Discovery::discover_dirs("skills",
//! "SKILL.md")`: project scopes (each ancestor of the cwd times
//! `PROJECT_PREFIXES`) then global config dirs, closest scope wins by name.
//! Built-in (binary-embedded) skills are not listed here; the desktop crate
//! has no craft-agent dependency.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use craft_storage::paths;

const SKILL_FILE: &str = "SKILL.md";
const SKILLS_DIR: &str = "skills";
const PROJECT_PREFIXES: &[&str] = &[".craft", ".agents", ".claude", ".opencode"];
const MAX_NAME_LEN: usize = 64;

/// Where a skill lives on disk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SkillScope {
    /// Under the current project's `<prefix>/skills`.
    Project,
    /// User-global config (`~/.craft/skills` or the XDG craft config).
    Global,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    /// Directory name the skill lives in; the canonical identifier.
    pub name: String,
    pub description: String,
    pub when_to_use: String,
    pub body: String,
    /// Path to the skill's `SKILL.md`.
    pub path: PathBuf,
    pub scope: SkillScope,
}

/// Editor form state for creating or editing a skill.
#[derive(Clone, Debug)]
pub struct SkillDraft {
    /// Path of the existing `SKILL.md` when editing; `None` when creating.
    pub target: Option<PathBuf>,
    pub name: String,
    pub description: String,
    pub when_to_use: String,
    pub body: String,
    pub scope: SkillScope,
}

impl SkillDraft {
    pub fn new() -> Self {
        Self {
            target: None,
            name: String::new(),
            description: String::new(),
            when_to_use: String::new(),
            body: String::new(),
            scope: SkillScope::Project,
        }
    }

    pub fn from_skill(s: &Skill) -> Self {
        Self {
            target: Some(s.path.clone()),
            name: s.name.clone(),
            description: s.description.clone(),
            when_to_use: s.when_to_use.clone(),
            body: s.body.clone(),
            scope: s.scope,
        }
    }
}

impl Default for SkillDraft {
    fn default() -> Self {
        Self::new()
    }
}

/// Skill names must be kebab-case slugs: lowercase alphanumerics and single
/// dashes, starting with an alphanumeric.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name is required".into());
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!("name must be at most {MAX_NAME_LEN} characters"));
    }
    let valid = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.ends_with('-')
        && !name.contains("--");
    if valid {
        Ok(())
    } else {
        Err("name must be kebab-case (lowercase letters, digits, single dashes)".into())
    }
}

pub fn render_skill_md(name: &str, description: &str, when_to_use: &str, body: &str) -> String {
    let body = body.trim();
    let description = yaml_scalar(description);
    let when_to_use = yaml_scalar(when_to_use);
    format!(
        "---\nname: {name}\ndescription: {description}\nwhen_to_use: {when_to_use}\n---\n\n{body}\n"
    )
}

/// Render a frontmatter value as a valid YAML scalar. Plain when safe,
/// double-quoted (with escaping, newlines flattened) otherwise, so the file
/// still parses as real YAML (craft-agent reads frontmatter via serde_yaml).
fn yaml_scalar(value: &str) -> String {
    let value = value.trim();
    if needs_quotes(value) {
        let escaped = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', " ");
        format!("\"{escaped}\"")
    } else {
        value.to_string()
    }
}

fn needs_quotes(value: &str) -> bool {
    const RESERVED: &[&str] = &["true", "false", "yes", "no", "null", "on", "off", "~"];
    value.is_empty()
        || value.contains([':', '#', '\n', '"', '\''])
        || value.starts_with(['[', '{', '&', '*', '!', '|', '>', '%', '@', '`', '-', '?'])
        || RESERVED.contains(&value.to_ascii_lowercase().as_str())
}

/// Undo [`yaml_scalar`]: strip surrounding double quotes and unescape.
fn unquote(value: &str) -> String {
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(escaped) => out.push(escaped),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Split `SKILL.md` content into frontmatter fields and body. Missing
/// frontmatter yields empty fields and the whole content as body.
pub fn parse_skill_md(content: &str) -> (String, String, String) {
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return (String::new(), String::new(), content.trim().to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (String::new(), String::new(), content.trim().to_string());
    };
    let front = &rest[..end];
    let body = rest[end + "\n---".len()..]
        .trim_start_matches(['\r', '\n'])
        .to_string();
    let mut description = String::new();
    let mut when_to_use = String::new();
    for line in front.lines() {
        if let Some(v) = line.strip_prefix("description:") {
            description = unquote(v.trim());
        } else if let Some(v) = line.strip_prefix("when_to_use:") {
            when_to_use = unquote(v.trim());
        }
    }
    (description, when_to_use, body.trim_end().to_string())
}

/// List skills visible from `cwd`: project scopes then global dirs, closest
/// scope shadowing farther ones by directory name.
pub fn list(cwd: &Path) -> Vec<Skill> {
    let home = paths::home();
    let xdg_config = paths::xdg_paths().ok().map(|x| x.config);
    let global = paths::user_config_dirs(home.as_deref(), xdg_config.as_deref(), SKILLS_DIR);
    list_in(cwd, &global)
}

/// Core of [`list`] with injectable global dirs, for testability.
pub fn list_in(cwd: &Path, global_dirs: &[PathBuf]) -> Vec<Skill> {
    let mut skills = Vec::new();
    for ancestor in cwd.ancestors() {
        for prefix in PROJECT_PREFIXES {
            let dir = ancestor.join(prefix).join(SKILLS_DIR);
            // Under $HOME the ancestor walk hits the legacy global dir too;
            // leave it to the global pass so the scope label stays honest.
            if global_dirs.contains(&dir) {
                continue;
            }
            collect_dir(&dir, SkillScope::Project, &mut skills);
        }
    }
    for dir in global_dirs {
        collect_dir(dir, SkillScope::Global, &mut skills);
    }
    let mut seen = HashSet::new();
    skills.retain(|s| seen.insert(s.name.clone()));
    skills
}

/// Directory the "project skill" create action writes into.
pub fn project_write_dir(cwd: &Path) -> PathBuf {
    cwd.join(PROJECT_PREFIXES[0]).join(SKILLS_DIR)
}

/// Directory the "global skill" create action writes into. `config_dir`
/// resolves the legacy `~/.craft` when it exists, else the XDG config dir,
/// and ensures the base directory exists.
pub fn global_write_dir() -> Result<PathBuf, String> {
    paths::config_dir()
        .map_err(|e| format!("cannot resolve global config dir: {e}"))
        .map(|d| d.join(SKILLS_DIR))
}

/// Write a new skill into `skills_dir` (e.g. from [`project_write_dir`] /
/// [`global_write_dir`]). Fails when a skill of the same name already exists.
pub fn create(skills_dir: &Path, draft: &SkillDraft) -> Result<PathBuf, String> {
    let name = draft.name.trim();
    validate_name(name)?;
    let dir = skills_dir.join(name);
    let path = dir.join(SKILL_FILE);
    if path.exists() {
        return Err(format!(
            "skill '{name}' already exists at {}",
            path.display()
        ));
    }
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    fs::write(
        &path,
        render_skill_md(name, &draft.description, &draft.when_to_use, &draft.body),
    )
    .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(path)
}

/// Overwrite an existing skill file in place. The canonical identity is the
/// containing directory (craft's discovery contract), which is derived from
/// the path rather than trusting the draft name.
pub fn update(path: &Path, draft: &SkillDraft) -> Result<(), String> {
    let name = path
        .parent()
        .and_then(|dir| dir.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| draft.name.trim());
    validate_name(name)?;
    fs::write(
        path,
        render_skill_md(name, &draft.description, &draft.when_to_use, &draft.body),
    )
    .map_err(|e| format!("failed to write {}: {e}", path.display()))
}

/// Remove the skill's whole directory (`<dir>/SKILL.md` plus any extra files).
pub fn delete(skill: &Skill) -> Result<(), String> {
    let dir = skill.path.parent().unwrap_or_else(|| Path::new(""));
    fs::remove_dir_all(dir).map_err(|e| format!("failed to delete {}: {e}", dir.display()))
}

fn collect_dir(dir: &Path, scope: SkillScope, out: &mut Vec<Skill>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(dir = %dir.display(), error = %e, "skills dir unreadable");
            }
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let marker = path.join(SKILL_FILE);
        let content = match fs::read_to_string(&marker) {
            Ok(c) => c,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(skill = %marker.display(), error = %e, "skill file unreadable");
                }
                continue;
            }
        };
        let (description, when_to_use, body) = parse_skill_md(&content);
        out.push(Skill {
            name,
            description,
            when_to_use,
            body,
            path: marker,
            scope,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const SAMPLE: &str = "---\nname: run\ndescription: Build and run\nwhen_to_use: When running the app\n---\n\nDo the thing.\n";

    fn write_skill(dir: &Path, name: &str, content: &str) {
        create_dir_and_file(dir, name, content);
    }

    fn create_dir_and_file(dir: &Path, name: &str, content: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join(SKILL_FILE), content).unwrap();
    }

    #[test]
    fn parse_with_frontmatter() {
        let (desc, when, body) = parse_skill_md(SAMPLE);
        assert_eq!(desc, "Build and run");
        assert_eq!(when, "When running the app");
        assert_eq!(body, "Do the thing.");
    }

    #[test]
    fn parse_without_frontmatter_returns_full_body() {
        let (desc, when, body) = parse_skill_md("just text\n");
        assert!(desc.is_empty() && when.is_empty());
        assert_eq!(body, "just text");
    }

    #[test]
    fn render_then_parse_roundtrip() {
        let md = render_skill_md("demo", "A demo", "When demoing", "Steps here.");
        let (desc, when, body) = parse_skill_md(&md);
        assert_eq!(desc, "A demo");
        assert_eq!(when, "When demoing");
        assert_eq!(body, "Steps here.");
        assert!(md.starts_with("---\nname: demo\n"));
    }

    #[test_case("run"; "plain")]
    #[test_case("my-cool-skill2"; "kebab with digits")]
    fn validate_name_accepts_ok(name: &str) {
        validate_name(name).unwrap();
    }

    #[test_case(""; "empty")]
    #[test_case("My Skill"; "spaces and caps")]
    #[test_case("-lead"; "leading dash")]
    #[test_case("trail-"; "trailing dash")]
    #[test_case("a--b"; "double dash")]
    #[test_case("snake_case"; "underscore")]
    fn validate_name_rejects_bad(name: &str) {
        assert!(validate_name(name).is_err());
    }

    #[test]
    fn body_starting_with_bullet_survives_roundtrip() {
        let content = render_skill_md("demo", "d", "w", "- First step\n- Second step");
        let (_, _, body) = parse_skill_md(&content);
        assert_eq!(body, "- First step\n- Second step");
    }

    #[test]
    fn crlf_frontmatter_parses() {
        let (desc, when, body) = parse_skill_md(
            "---\r\nname: run\r\ndescription: Build and run\r\nwhen_to_use: When running\r\n---\r\n\r\nBody.\r\n",
        );
        assert_eq!(desc, "Build and run");
        assert_eq!(when, "When running");
        assert_eq!(body, "Body.");
    }

    #[test]
    fn quoted_scalars_roundtrip_and_stay_valid_yaml() {
        let desc = "Use when in doubt: it fixes #1 \"for real\"";
        let md = render_skill_md("demo", desc, "when", "body");
        let (parsed_desc, _, _) = parse_skill_md(&md);
        assert_eq!(parsed_desc, desc);

        // The frontmatter craft-agent reads must be valid real YAML.
        let end = md.find("\n---\n").unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&md[4..end]).unwrap();
        assert_eq!(
            parsed.get("description").and_then(|v| v.as_str()),
            Some(desc)
        );
    }

    #[test]
    fn list_in_labels_home_level_global_dir_as_global() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let cwd = home.join("proj/nested");
        fs::create_dir_all(&cwd).unwrap();
        // Same dir reachable as ancestor prefix (.craft at $HOME) and global.
        let global = home.join(".craft/skills");
        write_skill(&global, "far", SAMPLE);

        let skills = list_in(&cwd, std::slice::from_ref(&global));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].scope, SkillScope::Global);
    }

    #[test]
    fn update_derives_name_from_directory_not_draft() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills");
        let draft = SkillDraft {
            name: "demo".into(),
            description: "d".into(),
            when_to_use: "w".into(),
            body: "b".into(),
            ..SkillDraft::new()
        };
        let path = create(&dir, &draft).unwrap();
        let renamed = SkillDraft {
            name: "other".into(),
            ..draft
        };
        update(&path, &renamed).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("name: demo"), "{content}");
    }

    #[test]
    fn list_in_scopes_and_shadows_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj/nested");
        fs::create_dir_all(&cwd).unwrap();
        let global = tmp.path().join("global/skills");
        write_skill(&cwd.join(".craft/skills"), "near", SAMPLE);
        write_skill(&tmp.path().join("proj/.agents/skills"), "near", SAMPLE);
        write_skill(&global, "far", SAMPLE);
        write_skill(&global, "near", SAMPLE);

        let skills = list_in(&cwd, &[global]);
        let by_name = |n: &str| skills.iter().find(|s| s.name == n).unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(by_name("near").scope, SkillScope::Project);
        assert!(by_name("near").path.starts_with(&cwd));
        assert_eq!(by_name("far").scope, SkillScope::Global);
        assert_eq!(by_name("near").description, "Build and run");
    }

    #[test]
    fn create_update_delete_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills");
        let draft = SkillDraft {
            name: "demo".into(),
            description: "A demo".into(),
            when_to_use: "When demoing".into(),
            body: "Steps.".into(),
            ..SkillDraft::new()
        };
        let path = create(&dir, &draft).unwrap();
        assert!(path.is_file());

        let exists_err = create(&dir, &draft).unwrap_err();
        assert!(exists_err.contains("already exists"));

        let updated = SkillDraft {
            body: "New steps.".into(),
            ..draft.clone()
        };
        update(&path, &updated).unwrap();
        let (_, _, body) = parse_skill_md(&fs::read_to_string(&path).unwrap());
        assert_eq!(body, "New steps.");

        let skill = Skill {
            name: "demo".into(),
            description: String::new(),
            when_to_use: String::new(),
            body: String::new(),
            path,
            scope: SkillScope::Project,
        };
        delete(&skill).unwrap();
        assert!(!dir.join("demo").exists());
    }

    #[test]
    fn create_rejects_invalid_name() {
        let tmp = tempfile::tempdir().unwrap();
        let draft = SkillDraft {
            name: "Bad Name".into(),
            ..SkillDraft::new()
        };
        assert!(create(tmp.path(), &draft).is_err());
        assert!(!tmp.path().join("Bad Name").exists());
    }
}
