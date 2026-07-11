//! `wiki_read` and `wiki_append` agent tools. They operate on the in-project
//! `.wiki/` knowledge base (see `craft_storage::wiki`). Project root is the
//! agent's current working directory, matching how `resolve_path` works.

use std::env;

use craft_storage::wiki::WikiStore;
use craft_tool_macro::Tool;
use serde::Deserialize;

use super::ToolContext;
use crate::tools::ToolInvocation;
use crate::types::ToolOutput;

const WIKI_READ_DESCRIPTION: &str = "\
Read a page or ingested-source note from the project's local wiki (`.wiki/`). \
The `page` argument is the entry's slug (lowercase, digits, and dashes). \
Returns the markdown body. Wiki content is durable plain markdown committed to the repo.";

const WIKI_APPEND_DESCRIPTION: &str = "\
Append markdown `body` to a wiki page in the project's local wiki (`.wiki/`), \
creating it if it does not exist, then regenerate the wiki index. \
`page` is the entry's slug (lowercase, digits, and dashes). \
Use this to capture durable, shareable project knowledge (decisions, notes, glossary entries).";

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct WikiRead {
    #[param(description = "Slug of the wiki page or source note to read")]
    page: String,
}

impl WikiRead {
    pub const NAME: &str = "wiki_read";
    pub const DESCRIPTION: &str = WIKI_READ_DESCRIPTION;
    pub const EXAMPLES: Option<&str> = Some(r#"[{"page": "glossary"}, {"page": "design-doc"}]"#);

    pub fn start_header(&self) -> String {
        format!("wiki_read({})", self.page)
    }

    pub async fn execute(&self, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        let cwd = env::current_dir().map_err(|e| format!("cwd error: {e}"))?;
        let store = WikiStore::open(&cwd).map_err(|e| e.to_string())?;
        let body = store.read_page(&self.page).map_err(|e| e.to_string())?;
        Ok(ToolOutput::Plain(body))
    }
}

super::impl_tool!(WikiRead, kind = "read");

impl ToolInvocation for WikiRead {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(WikiRead::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { WikiRead::execute(&self, ctx).await.into() })
    }
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct WikiAppend {
    #[param(description = "Slug of the wiki page to append to")]
    page: String,
    #[param(description = "Markdown body to append")]
    body: String,
}

impl WikiAppend {
    pub const NAME: &str = "wiki_append";
    pub const DESCRIPTION: &str = WIKI_APPEND_DESCRIPTION;
    pub const EXAMPLES: Option<&str> = Some(
        "[{\"page\": \"decisions\", \"body\": \"## Use Postgres\\n\\nDecided 2026-01-01 to standardize on Postgres.\"}]",
    );

    pub fn start_header(&self) -> String {
        format!("wiki_append({})", self.page)
    }

    pub async fn execute(&self, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        let cwd = env::current_dir().map_err(|e| format!("cwd error: {e}"))?;
        let store = WikiStore::open(&cwd).map_err(|e| e.to_string())?;
        store
            .append_page(&self.page, &self.body)
            .map_err(|e| e.to_string())?;
        store.rebuild_index().map_err(|e| e.to_string())?;
        Ok(ToolOutput::Plain(format!(
            "appended to wiki page `{}`",
            self.page
        )))
    }
}

super::impl_tool!(WikiAppend, kind = "edit");

impl ToolInvocation for WikiAppend {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(WikiAppend::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { WikiAppend::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    #[tokio::test]
    async fn wiki_append_then_read_roundtrips() {
        let dir = tempfile::TempDir::new().unwrap();
        let prev = env::current_dir().unwrap();
        env::set_current_dir(dir.path()).unwrap();
        let ctx = stub_ctx(&AgentMode::Build);

        let append = WikiAppend::parse_input(
            &serde_json::json!({"page": "decisions", "body": "# Decisions\n\nUse Rust."}),
        )
        .unwrap();
        let out = append.execute(&ctx).await.unwrap();
        assert!(out.as_text().contains("appended"));

        let read = WikiRead::parse_input(&serde_json::json!({"page": "decisions"})).unwrap();
        let body = read.execute(&ctx).await.unwrap();
        assert!(body.as_text().contains("Use Rust."));

        let index = std::fs::read_to_string(dir.path().join(".wiki/index.md")).unwrap();
        assert!(index.contains("[Decisions](pages/decisions.md)"));

        env::set_current_dir(prev).unwrap();
    }
}
