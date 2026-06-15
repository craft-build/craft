use std::sync::Arc;

use crate::ToolOutput;
use craft_tool_macro::Tool;
use serde::Deserialize;

use crate::agent::compression_store::SharedCompressionStore;

const RETRIEVE_TOOL_NAME: &str = "retrieve";

#[derive(Tool, Debug, Clone, Deserialize)]
pub(crate) struct Retrieve {
    #[param(description = "Hash of the compressed content to retrieve")]
    hash: String,
}

impl Retrieve {
    pub const NAME: &str = RETRIEVE_TOOL_NAME;
    pub const DESCRIPTION: &str = "Retrieve the original (uncompressed) content for a previously compressed tool output. Use the hash value from a compression marker in the conversation. Compression markers appear as [N lines compressed from M. Retrieve original: hash=HASH] or in stale/superseded read markers that include a hash.";
    pub const EXAMPLES: Option<&str> = Some(r#"[{"hash": "a1b2c3d4"}]"#);

    pub async fn execute(&self, store: &SharedCompressionStore) -> Result<ToolOutput, String> {
        let guard = store.lock().map_err(|e| format!("store lock: {e}"))?;
        match guard.get(&self.hash) {
            Some(original) => Ok(ToolOutput::Plain(original.to_owned())),
            None => Err(format!(
                "no content found for hash={}. The original may have been evicted from the store.",
                self.hash
            )),
        }
    }
}

crate::tools::impl_tool!(
    Retrieve,
    audience = crate::tools::ToolAudience::MAIN | crate::tools::ToolAudience::RESEARCH_SUB,
);

impl crate::tools::ToolInvocation for Retrieve {
    fn start_header(&self) -> crate::tools::HeaderFuture {
        crate::tools::HeaderFuture::Ready(crate::tools::HeaderResult::plain(format!(
            "retrieve {}",
            self.hash
        )))
    }
    fn execute<'a>(
        self: Box<Self>,
        ctx: &'a crate::tools::ToolContext,
    ) -> crate::tools::ExecFuture<'a> {
        let store = Arc::clone(&ctx.compression_store);
        Box::pin(async move { Retrieve::execute(&self, &store).await })
    }
}
