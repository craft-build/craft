//! `wiki_read` and `wiki_append` agent tools. They operate on the in-project
//! `.wiki/` knowledge base (see `craft_storage::wiki`). Project root is the
//! agent's current working directory, matching how `resolve_path` works.

use std::env;

use craft_storage::wiki::{PageMeta, WikiStore};
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
The optional `kind`, `description`, and `tags` fields populate the page's OKF \
frontmatter: `kind` sets the `type` for a new page (default `Note`), and \
`description`/`tags` fill in a missing field on an existing page without \
overwriting what is already there. \
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
    #[param(
        description = "OKF `type` for a new page, e.g. Note, Reference, Decision. Ignored if the page already exists."
    )]
    kind: Option<String>,
    #[param(
        description = "One-line description for the page's OKF frontmatter and the index listing. Fills in a missing field only."
    )]
    description: Option<String>,
    #[param(description = "Tags for the page's OKF frontmatter. Fills in a missing field only.")]
    tags: Option<Vec<String>>,
}

impl WikiAppend {
    pub const NAME: &str = "wiki_append";
    pub const DESCRIPTION: &str = WIKI_APPEND_DESCRIPTION;
    pub const EXAMPLES: Option<&str> = Some(
        "[{\"page\": \"decisions\", \"body\": \"## Use Postgres\\n\\nDecided 2026-01-01 to standardize on Postgres.\", \"kind\": \"Decision\", \"description\": \"Database choice\", \"tags\": [\"database\", \"adr\"]}]",
    );

    pub fn start_header(&self) -> String {
        format!("wiki_append({})", self.page)
    }

    pub async fn execute(&self, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        let cwd = env::current_dir().map_err(|e| format!("cwd error: {e}"))?;
        let store = WikiStore::open(&cwd).map_err(|e| e.to_string())?;
        let timestamp = jiff::Zoned::now()
            .strftime("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let meta = PageMeta {
            kind: self.kind.clone(),
            description: self.description.clone(),
            tags: self.tags.clone().unwrap_or_default(),
        };
        store
            .append_page_meta(&self.page, &self.body, Some(&timestamp), meta)
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

    #[tokio::test]
    async fn wiki_append_with_meta_sets_okf_frontmatter() {
        let dir = tempfile::TempDir::new().unwrap();
        let prev = env::current_dir().unwrap();
        env::set_current_dir(dir.path()).unwrap();
        let ctx = stub_ctx(&AgentMode::Build);

        let append = WikiAppend::parse_input(&serde_json::json!({
            "page": "tech-stack",
            "body": "# Tech Stack\n\nRust.",
            "kind": "Reference",
            "description": "stack overview",
            "tags": ["rust", "toolchain"],
        }))
        .unwrap();
        append.execute(&ctx).await.unwrap();

        let raw = std::fs::read_to_string(dir.path().join(".wiki/pages/tech-stack.md")).unwrap();
        assert!(raw.contains("type: Reference"));
        assert!(raw.contains("description: stack overview"));
        assert!(raw.contains("rust"));

        let index = std::fs::read_to_string(dir.path().join(".wiki/index.md")).unwrap();
        assert!(index.contains("[Tech Stack](pages/tech-stack.md) - stack overview"));

        env::set_current_dir(prev).unwrap();
    }

    #[tokio::test]
    async fn wiki_append_meta_does_not_overwrite_existing_type() {
        let dir = tempfile::TempDir::new().unwrap();
        let prev = env::current_dir().unwrap();
        env::set_current_dir(dir.path()).unwrap();
        let ctx = stub_ctx(&AgentMode::Build);

        let append = WikiAppend::parse_input(&serde_json::json!({
            "page": "decisions",
            "body": "# Decisions\n\nUse Rust.",
            "kind": "Decision",
        }))
        .unwrap();
        append.execute(&ctx).await.unwrap();

        let append2 = WikiAppend::parse_input(&serde_json::json!({
            "page": "decisions",
            "body": "## Also use tokio.",
            "kind": "Reference",
        }))
        .unwrap();
        append2.execute(&ctx).await.unwrap();

        let raw = std::fs::read_to_string(dir.path().join(".wiki/pages/decisions.md")).unwrap();
        assert!(raw.contains("type: Decision"));
        assert!(!raw.contains("type: Reference"));
        assert!(raw.contains("Use Rust."));
        assert!(raw.contains("Also use tokio."));

        env::set_current_dir(prev).unwrap();
    }
}
