//! Craft-side reviewer setup. Argosy 0.2's [`argosy::harness::setup_reviewer`]
//! only knows the OpenCode, Claude, and Kiro layouts (fixed dirs, no custom
//! agents-dir parameter), and none matches `.craft/`, so craft writes its own
//! reviewer definition in the same shape: frontmatter for configuration, body
//! for the system prompt.

use std::path::{Path, PathBuf};

const REVIEWER_REL: &str = ".craft/reviewers/reviewer.md";

const REVIEWER_DESCRIPTION: &str = "Reviews code against styleguide rules and best practices; \
     reports prioritized findings (P0-P3) without modifying files. Use when asked to review code, \
     changes, or a diff.";

const REVIEWER_TOOLS: &str = "read, grep, styleguide_list, styleguide_search, styleguide_get, \
     report_finding";

/// Craft's reviewer subagent definition, mirroring what the built-in `review`
/// tool grants the subagent: read-only tools plus styleguide grounding.
pub fn reviewer_definition() -> String {
    format!(
        "---\nname: reviewer\ndescription: {REVIEWER_DESCRIPTION}\ntools: {REVIEWER_TOOLS}\n---\n\
         \nYou are a code reviewer. Review code against styleguide rules and best practices. \
         Report findings with clear priority levels (P0 critical, P1 urgent, P2 normal, P3 low). \
         Be thorough but constructive.\n\n\
         - ALWAYS read the code before reviewing. Never review from descriptions alone.\n\
         - Ground findings in styleguide rules: use `styleguide_search` / `styleguide_get` and \
         cite rule ids (e.g. `rust/naming/snake-case-vars`) in `report_finding`.\n\
         - Never modify, create, or delete files. Report findings only.\n\
         - Each finding must state a concrete failure scenario, not just \"this looks wrong\".\n"
    )
}

/// Writes the reviewer definition under `project_root/.craft/reviewers/`,
/// creating the directory as needed. An existing file is an error unless
/// `force` replaces it (staged write, crash-atomic).
pub fn setup_reviewer(project_root: &Path, force: bool) -> Result<PathBuf, String> {
    let dest = project_root.join(REVIEWER_REL);
    if dest.exists() && !force {
        return Err(format!(
            "reviewer definition already exists at {}; delete it or pass force to replace it",
            dest.display()
        ));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let tmp = dest.with_extension("md.tmp");
    std::fs::write(&tmp, reviewer_definition())
        .and_then(|()| std::fs::rename(&tmp, &dest))
        .map_err(|e| format!("{}: {e}", dest.display()))?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_has_craft_frontmatter_and_grounding_rules() {
        let def = reviewer_definition();
        assert!(def.starts_with("---\nname: reviewer\n"));
        assert!(def.contains("tools: read, grep, styleguide_list"));
        assert!(def.contains("styleguide_search"));
        assert!(def.contains("Never modify, create, or delete files"));
        assert!(def.ends_with('\n'));
    }

    #[test]
    fn setup_writes_definition_and_refuses_overwrite_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let path = setup_reviewer(tmp.path(), false).unwrap();
        assert_eq!(path, tmp.path().join(REVIEWER_REL));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            reviewer_definition()
        );

        std::fs::write(&path, "user edits\n").unwrap();
        assert!(setup_reviewer(tmp.path(), false).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "user edits\n");

        setup_reviewer(tmp.path(), true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            reviewer_definition()
        );
    }
}
