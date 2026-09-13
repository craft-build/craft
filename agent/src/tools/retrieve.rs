use rig_core::tool::{IntoToolOutput, PortableTool, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Result, invalid};
use crate::compression::store::SharedCompressionStore;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetrieveArgs {
    /// Hash of the compressed content to retrieve, taken from a
    /// `[N lines compressed from M. Retrieve original: hash=HASH]` marker.
    pub hash: String,
}

#[derive(Debug)]
pub struct RetrieveOutput {
    pub text: String,
}

impl IntoToolOutput for RetrieveOutput {
    fn into_tool_output(
        self,
    ) -> std::result::Result<ToolOutput, rig_core::tool::ToolExecutionError> {
        Ok(ToolOutput::text(self.text))
    }
}

/// Restores the original text of a replaced tool output by content hash,
/// from the session's compression store.
#[derive(Clone)]
pub struct Retrieve(pub SharedCompressionStore);

impl Retrieve {
    fn execute(&self, args: RetrieveArgs) -> Result<RetrieveOutput> {
        match self.0.lock() {
            Ok(guard) => match guard.get(&args.hash) {
                Some(original) => Ok(RetrieveOutput {
                    text: original.to_owned(),
                }),
                None => Err(invalid(format!(
                    "no content found for hash={}. The original may have been evicted from the \
                     store.",
                    args.hash
                ))),
            },
            Err(_) => Err(invalid("compression store lock was poisoned")),
        }
    }
}

impl PortableTool for Retrieve {
    const NAME: &'static str = "retrieve";
    type Args = RetrieveArgs;
    type Output = RetrieveOutput;
    type Error = rig_core::tool::ToolExecutionError;

    fn description(&self) -> String {
        "Retrieve the original (uncompressed) content for a previously compressed tool output. \
         Use the hash value from a compression marker in the conversation. Markers appear as \
         [N lines compressed from M. Retrieve original: hash=HASH]."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(RetrieveArgs))
            .expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output> {
        self.execute(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(hash: &str) -> RetrieveArgs {
        RetrieveArgs {
            hash: hash.to_owned(),
        }
    }

    #[tokio::test]
    async fn returns_stored_original() {
        let store = crate::compression::store::shared_store();
        let hash = store.lock().unwrap().put("the original text");
        let tool = Retrieve(store);
        let output = tool.call(args(&hash)).await.expect("hit");
        assert_eq!(output.text, "the original text");
    }

    #[tokio::test]
    async fn unknown_hash_is_an_error() {
        let tool = Retrieve(crate::compression::store::shared_store());
        let error = tool.call(args("deadbeef")).await.expect_err("miss");
        assert!(
            error
                .to_string()
                .contains("no content found for hash=deadbeef")
        );
    }
}
