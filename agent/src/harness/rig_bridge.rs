//! Temporary bridge wrapping rig `DynamicTool`s as animist tool executors,
//! deleted by docs/animist/06 once the tools are native `ToolImpl`s. Every
//! claim is `Exclusive`, so calls execute serially exactly as the old
//! dispatch waves did.

use async_trait::async_trait;

use animist::agent::tool::{
    ToolDefinition, ToolError, ToolExecutor, ToolExecutorImpl, ToolOutput, ToolRegistry,
    ToolScheduling,
};
use rig_core::tool::DynamicTool;

use crate::run::dispatch::{self, ToolDispatch};

/// One bridged rig tool: its declaration, serial scheduling, and its
/// executor with the call-id task-local scoped around it.
struct RigTool {
    tool: DynamicTool,
}

#[async_trait]
impl ToolExecutorImpl for RigTool {
    fn definition(&self) -> ToolDefinition {
        let definitions = crate::tools::animist_definitions(&[self.tool.definition()]);
        definitions
            .into_iter()
            .next()
            .expect("one definition in, one out")
    }

    fn scheduling(&self, _input: &serde_json::Value) -> ToolScheduling {
        ToolScheduling::Exclusive
    }

    async fn execute(
        &self,
        context: &animist::agent::tool::ToolContext<'_>,
        input: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ToolError> {
        let output = dispatch::scope_call_id(&context.call.id, self.tool.execute(input)).await;
        Ok(match output {
            Ok(output) => ToolOutput::builder()
                .content(dispatch::rig_content_to_contents(output.as_content()))
                .build(),
            // The rig error's own model output wins when it has one; the
            // rendered error text is the fallback — the bytes the old
            // dispatch produced.
            Err(error) => ToolOutput::builder()
                .content(dispatch::output_content_of_error(&error))
                .is_error(true)
                .build(),
        })
    }
}

/// Build the agent's tool registry from a dispatch table; duplicate or empty
/// names fail exactly as native registration does.
#[expect(
    clippy::result_large_err,
    reason = "AgentError is animist's public error type and cannot be boxed here"
)]
pub(crate) fn registry(
    dispatch: &ToolDispatch,
) -> std::result::Result<ToolRegistry, animist::agent::AgentError> {
    let mut registry = ToolRegistry::default();
    for tool in dispatch.dynamic_tools() {
        registry.register_executor(ToolExecutor::new(RigTool { tool: tool.clone() }))?;
    }
    Ok(registry)
}
