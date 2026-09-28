use rig_core::tool::{IntoToolOutput, PortableTool, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Result, invalid};
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

impl IntoToolOutput for SkillOutput {
    fn into_tool_output(
        self,
    ) -> std::result::Result<ToolOutput, rig_core::tool::ToolExecutionError> {
        Ok(ToolOutput::text(self.text))
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

impl PortableTool for Skill {
    const NAME: &'static str = "skill";
    type Args = SkillArgs;
    type Output = SkillOutput;
    type Error = rig_core::tool::ToolExecutionError;

    fn description(&self) -> String {
        format!(
            "Load a skill that provides instructions and workflows for specific tasks.{}",
            self.0.skill_list()
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(SkillArgs)).expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output> {
        self.execute(args)
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

    #[tokio::test]
    async fn loads_skill_with_location_and_line_numbers() {
        let tmp = TempDir::new().unwrap();
        let tool = skill_in(tmp.path(), "audit", "---\nname: audit\n---\nfirst\nsecond");
        let output = tool
            .call(SkillArgs {
                name: "audit".into(),
            })
            .await
            .unwrap();
        assert!(
            output.text.contains(".craft/skills/audit/SKILL.md"),
            "{text}",
            text = output.text
        );
        assert!(
            output.text.contains("   1 | ---"),
            "{text}",
            text = output.text
        );
        assert!(
            output.text.contains("   5 | second"),
            "{text}",
            text = output.text
        );
    }

    #[tokio::test]
    async fn loads_builtin_without_filesystem() {
        let tmp = TempDir::new().unwrap();
        let tool = Skill(Discovery::new(tmp.path().to_path_buf(), None, None));
        let output = tool.call(SkillArgs { name: "run".into() }).await.unwrap();
        assert!(output.text.contains("<builtin>/skills/run/SKILL.md"));
    }

    #[tokio::test]
    async fn unknown_skill_lists_available() {
        let tmp = TempDir::new().unwrap();
        let tool = skill_in(tmp.path(), "audit", "---\ndescription: audits\n---\nbody");
        let error = tool
            .call(SkillArgs {
                name: "nope".into(),
            })
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("skill not found: nope"), "{message}");
        assert!(message.contains("- audit: audits"), "{message}");
        assert!(message.contains("- verify:"), "{message}");
    }

    #[tokio::test]
    async fn empty_name_is_invalid() {
        let tmp = TempDir::new().unwrap();
        let tool = Skill(Discovery::new(tmp.path().to_path_buf(), None, None));
        assert!(tool.call(SkillArgs { name: "  ".into() }).await.is_err());
    }

    #[test]
    fn description_lists_skills() {
        let tmp = TempDir::new().unwrap();
        let tool = skill_in(tmp.path(), "audit", "---\ndescription: audits\n---\nbody");
        let description = tool.description();
        assert!(description.contains("skill"), "{description}");
        assert!(description.contains("- audit: audits"), "{description}");
    }
}
