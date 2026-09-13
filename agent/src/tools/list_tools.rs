use std::sync::Arc;

use rig_core::completion::ToolDefinition;
use rig_core::tool::{IntoToolOutput, PortableTool, ToolExecutionError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Result, invalid};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListToolsArgs {
    /// Tool name to inspect. Returns its full input schema. Omit to list
    /// every tool with a short description.
    pub detail: Option<String>,
}

#[derive(Debug)]
pub struct ListToolsOutput {
    pub text: String,
}

impl IntoToolOutput for ListToolsOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        Ok(ToolOutput::text(self.text))
    }
}

/// Introspection over the session's registered tools. Holds a snapshot of
/// the dispatch definitions taken at registration time.
#[derive(Clone)]
pub struct ListTools(pub Arc<Vec<ToolDefinition>>);

impl ListTools {
    fn execute(&self, args: ListToolsArgs) -> Result<ListToolsOutput> {
        if let Some(name) = args.detail.as_deref() {
            if let Some(definition) = self.0.iter().find(|definition| definition.name == name) {
                let schema = serde_json::to_string_pretty(&definition.parameters)
                    .unwrap_or_else(|_| definition.parameters.to_string());
                return Ok(ListToolsOutput {
                    text: format!("{name}:\n\nInput schema:\n{schema}"),
                });
            }
            return Err(invalid(format!(
                "unknown tool: {name}. Call list_tools() to see available tools."
            )));
        }
        let mut entries: Vec<(&str, &str)> = self
            .0
            .iter()
            .map(|definition| {
                (
                    definition.name.as_str(),
                    definition.description.lines().next().unwrap_or("").trim(),
                )
            })
            .collect();
        entries.sort_unstable();
        let mut text = String::from("Available tools:\n");
        for (name, description) in entries {
            text.push_str(&format!("- {name}: {description}\n"));
        }
        Ok(ListToolsOutput { text })
    }
}

impl PortableTool for ListTools {
    const NAME: &'static str = "list_tools";
    type Args = ListToolsArgs;
    type Output = ListToolsOutput;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Introspect the available tools. With no arguments, lists every registered tool with a \
         one-line description. With detail=\"<name>\", returns that tool's full input schema."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ListToolsArgs))
            .expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> super::Result<Self::Output> {
        self.execute(args)
    }
}
