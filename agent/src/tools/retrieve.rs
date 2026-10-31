use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput, ToolScheduling};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::error::{Error, Result, definition, finish, invalid, text_output};
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

impl super::error::IntoToolResult for RetrieveOutput {
    fn into_tool_result(self) -> ToolOutput {
        text_output(self.text)
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

const RETRIEVE_DESCRIPTION: &str = "Retrieve the original (uncompressed) content for a previously compressed tool output. \
     Use the hash value from a compression marker in the conversation. Markers appear as \
     [N lines compressed from M. Retrieve original: hash=HASH].";

#[async_trait]
impl ToolImpl for Retrieve {
    type Args = RetrieveArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<RetrieveArgs>("retrieve", RETRIEVE_DESCRIPTION)
    }

    // The store is guarded by its own lock and nothing else claims it, so
    // retrieval is independent of every other call.

    fn scheduling(&self, _input: &serde_json::Value) -> ToolScheduling {
        ToolScheduling::Resources {
            reads: Vec::new(),
            writes: Vec::new(),
        }
    }

    async fn call(&self, _context: &ToolContext<'_>, args: RetrieveArgs) -> Result<ToolOutput> {
        finish(Self::execute(self, args))
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

    async fn invoke(tool: &Retrieve, args: RetrieveArgs) -> animist::agent::tool::ToolOutput {
        let mut harness = animist::agent::testing::ToolHarness::builder()
            .build()
            .expect("harness");
        harness
            .call(tool.clone(), serde_json::json!({ "hash": args.hash }))
            .await
            .expect("retrieve outcome")
    }

    #[tokio::test]
    async fn returns_stored_original() {
        let store = crate::compression::store::shared_store();
        let hash = store.lock().unwrap().put("the original text");
        let tool = Retrieve(store);
        let output = invoke(&tool, args(&hash)).await;
        assert_eq!(
            crate::history::ext::tool_content_text(&output.content),
            "the original text"
        );
    }

    #[tokio::test]
    async fn unknown_hash_is_an_error() {
        let tool = Retrieve(crate::compression::store::shared_store());
        let output = invoke(&tool, args("deadbeef")).await;
        assert!(output.is_error, "expected an error result");
        let text = crate::history::ext::tool_content_text(&output.content);
        assert!(
            text.contains("no content found for hash=deadbeef"),
            "{text}"
        );
    }
}
