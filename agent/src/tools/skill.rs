use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput, ToolScheduling};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::error::{Error, Result, definition, finish, invalid};
use crate::skills::Discovery;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillArgs {
    /// Name of the skill to load.
    pub name: String,
}

#[derive(Debug)]
pub struct SkillOutput {
    pub text: String,
}

impl super::error::IntoToolResult for SkillOutput {
    fn into_tool_result(self) -> ToolOutput {
        super::error::text_output(self.text)
    }
}

/// Loads a discovered skill's full SKILL.md body, numbered per line and
/// prefixed with its location. Skills are re-discovered on every call so
/// project changes between turns are picked up; the description's skill
/// list is a registration-time snapshot.
#[derive(Clone)]
pub struct Skill(pub Discovery);

impl Skill {
    /// Discovery rooted at the given working directory, with the user's
    /// home and XDG config from the environment.
    pub fn new(cwd: impl Into<std::path::PathBuf>) -> Self {
        Self(Discovery::new(
            cwd.into(),
            crate::paths::home(),
            crate::paths::xdg_config_dir().ok(),
        ))
    }

    fn execute(&self, args: SkillArgs) -> Result<SkillOutput> {
        let name = args.name.trim();
        if name.is_empty() {
            return Err(invalid("name is required"));
        }
        let Some(skill) = self.0.find(name) else {
            return Err(invalid(format!(
                "skill not found: {name}{}",
                self.0.skill_list()
            )));
        };
        let mut text = String::new();
        for (index, line) in skill.content.lines().enumerate() {
            text.push_str(&format!("{:4} | {line}\n", index + 1));
        }
        Ok(SkillOutput {
            text: format!("{}\n{}", skill.location(), text.trim_end()),
        })
    }
}

const SKILL_DESCRIPTION: &str =
    "Load a skill that provides instructions and workflows for specific tasks.";

#[async_trait]
impl ToolImpl for Skill {
    type Args = SkillArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        // The description's skill list is a registration-time snapshot, as
        // it always was.
        definition::<SkillArgs>(
            "skill",
            &format!("{SKILL_DESCRIPTION}{}", self.0.skill_list()),
        )
    }

    // Discovery reads the project, home, and XDG skill trees — state no
    // other tool writes through its claims, so the call is independent.

    fn scheduling(&self, _input: &serde_json::Value) -> ToolScheduling {
        ToolScheduling::Resources {
            reads: Vec::new(),
            writes: Vec::new(),
        }
    }

    async fn call(&self, _context: &ToolContext<'_>, args: SkillArgs) -> Result<ToolOutput> {
        finish(Self::execute(self, args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn skill_in(dir: &std::path::Path, name: &str, body: &str) -> Skill {
        let path = dir.join(".craft/skills").join(name).join("SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        Skill(Discovery::new(dir.to_path_buf(), None, None))
    }

    async fn invoke(tool: &Skill, name: &str) -> animist::agent::tool::ToolOutput {
        let mut harness = animist::agent::testing::ToolHarness::builder()
            .build()
            .expect("harness");
        harness
            .call(tool.clone(), serde_json::json!({ "name": name }))
            .await
            .expect("skill outcome")
    }

    fn text(output: &animist::agent::tool::ToolOutput) -> String {
        crate::history::ext::tool_content_text(&output.content)
    }

    #[tokio::test]
    async fn loads_skill_with_location_and_line_numbers() {
        let tmp = TempDir::new().unwrap();
        let tool = skill_in(tmp.path(), "audit", "---\nname: audit\n---\nfirst\nsecond");
        let output = invoke(&tool, "audit").await;
        let text = text(&output);
        assert!(text.contains(".craft/skills/audit/SKILL.md"), "{text}");
        assert!(text.contains("   1 | ---"), "{text}");
        assert!(text.contains("   5 | second"), "{text}");
    }

    #[tokio::test]
    async fn loads_builtin_without_filesystem() {
        let tmp = TempDir::new().unwrap();
        let tool = Skill(Discovery::new(tmp.path().to_path_buf(), None, None));
        let output = invoke(&tool, "run").await;
        assert!(text(&output).contains("<builtin>/skills/run/SKILL.md"));
    }

    #[tokio::test]
    async fn unknown_skill_lists_available() {
        let tmp = TempDir::new().unwrap();
        let tool = skill_in(tmp.path(), "audit", "---\ndescription: audits\n---\nbody");
        let output = invoke(&tool, "nope").await;
        assert!(output.is_error);
        let message = text(&output);
        assert!(message.contains("skill not found: nope"), "{message}");
        assert!(message.contains("- audit: audits"), "{message}");
        assert!(message.contains("- verify:"), "{message}");
    }

    #[tokio::test]
    async fn empty_name_is_invalid() {
        let tmp = TempDir::new().unwrap();
        let tool = Skill(Discovery::new(tmp.path().to_path_buf(), None, None));
        let output = invoke(&tool, "  ").await;
        assert!(output.is_error);
    }

    #[test]
    fn description_lists_skills() {
        let tmp = TempDir::new().unwrap();
        let tool = skill_in(tmp.path(), "audit", "---\ndescription: audits\n---\nbody");
        let description = tool.definition().description;
        assert!(description.contains("skill"), "{description}");
        assert!(description.contains("- audit: audits"), "{description}");
    }
}
