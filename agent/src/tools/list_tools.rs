use std::sync::Arc;

use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput, ToolScheduling};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::error::{Error, Result, definition, finish, invalid};

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

impl super::error::IntoToolResult for ListToolsOutput {
    fn into_tool_result(self) -> ToolOutput {
        super::error::text_output(self.text)
    }
}

/// Introspection over the session's registered tools. Holds a snapshot of
/// the registry's definitions taken at registration time.
#[derive(Clone)]
pub struct ListTools(pub Arc<Vec<ToolDefinition>>, pub Arc<Vec<ToolDefinition>>);

impl ListTools {
    fn execute(&self, args: ListToolsArgs, scripted: bool) -> Result<ListToolsOutput> {
        let definitions = if scripted { &self.1 } else { &self.0 };
        if let Some(name) = args.detail.as_deref() {
            if let Some(definition) = definitions
                .iter()
                .find(|definition| definition.name == name)
            {
                let schema = serde_json::to_string_pretty(&definition.input_schema)
                    .unwrap_or_else(|_| definition.input_schema.to_string());
                return Ok(ListToolsOutput {
                    text: format!("{name}:\n\nInput schema:\n{schema}"),
                });
            }
            return Err(invalid(format!(
                "unknown tool: {name}. Call list_tools() to see available tools."
            )));
        }
        let mut entries: Vec<(&str, &str)> = definitions
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

#[async_trait]
impl ToolImpl for ListTools {
    type Args = ListToolsArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<ListToolsArgs>(
            "list_tools",
            "Introspect the available tools. With no arguments, lists every registered tool with a \
             one-line description. With detail=\"<name>\", returns that tool's full input schema.",
        )
    }

    fn metadata(&self) -> std::collections::BTreeMap<String, serde_json::Value> {
        crate::codemode::catalog::model_only_metadata()
    }

    // In-memory snapshot: independent of every other call.

    fn scheduling(&self, _input: &serde_json::Value) -> ToolScheduling {
        ToolScheduling::Resources {
            reads: Vec::new(),
            writes: Vec::new(),
        }
    }

    async fn call(&self, _context: &ToolContext<'_>, args: ListToolsArgs) -> Result<ToolOutput> {
        finish(Self::execute(self, args, false))
    }
}

/// The script-facing twin of `list_tools`. It is code-only and reports the
/// complete registry, so hidden tools never leak through the model-facing tool.
#[derive(Clone)]
pub struct ListToolsCode(pub ListTools);

#[async_trait]
impl ToolImpl for ListToolsCode {
    type Args = ListToolsArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<ListToolsArgs>(
            "list_tools_code",
            "Codemode alias for list_tools that reports every code-callable tool.",
        )
    }

    fn metadata(&self) -> std::collections::BTreeMap<String, serde_json::Value> {
        crate::codemode::catalog::code_only_metadata()
    }

    fn scheduling(&self, input: &serde_json::Value) -> ToolScheduling {
        self.0.scheduling(input)
    }

    async fn call(&self, _context: &ToolContext<'_>, args: ListToolsArgs) -> Result<ToolOutput> {
        finish(ListTools::execute(&self.0, args, true))
    }
}
