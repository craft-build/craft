//! Skills shipped inside the craft binary so every user has them without a
//! project or global install. Embedded at compile time via `include_str!`.
//!
//! Order is alphabetical by name. User, project, and global skills discovered
//! on the filesystem always shadow a built-in of the same name.

/// One built-in skill: `(name, SKILL.md content including frontmatter)`.
pub type BuiltinSkill = (&'static str, &'static str);

/// All built-in skills, sorted alphabetically by name.
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
pub fn get(name: &str) -> Option<&'static str> {
    BUILTIN_SKILLS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, content)| *content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_skills_have_frontmatter_and_body() {
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
    fn sorted_alphabetically() {
        let names: Vec<&str> = BUILTIN_SKILLS.iter().map(|(n, _)| *n).collect();
        let mut expected = names.clone();
        expected.sort();
        assert_eq!(names, expected);
    }

    #[test]
    fn get_returns_content_and_misses_unknown() {
        assert!(get("run").is_some());
        assert!(get("does-not-exist").is_none());
    }
}
