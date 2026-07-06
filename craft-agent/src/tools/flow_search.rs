//! The `flow_search` tool and its backend trait.
//!
//! `flow_search` lets a Flow stage agent retrieve the most relevant persisted
//! workstream documents for a natural-language query, using the semantic index
//! maintained per workstream. The tool is always registered.
//!
//! craft-agent cannot depend on craft-flow (that would be a cycle), so the tool
//! talks to an injectable [`FlowSearchBackend`] trait object held in
//! [`crate::tools::ToolContext`]. craft-flow supplies the impl built on the
//! `EmbeddingService` and the `FlowStore`; every other build leaves it `None`,
//! and the tool then errors cleanly instead of silently no-op'ing.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use craft_tool_macro::Tool;
use serde::Deserialize;

use super::ToolContext;
use crate::tools::ToolInvocation;
use crate::types::ToolOutput;

/// One ranked result row.
#[derive(Debug, Clone)]
pub struct FlowSearchHit {
    pub path: String,
    pub score: f32,
}

/// Boxed search future, kept edition-portable (no `async fn` in the trait) so
/// the trait is object-safe without an `async_trait` dependency.
pub type SearchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<FlowSearchHit>, String>> + Send + 'a>>;

/// Boxed future for reading one workstream document, factored out for the same
/// reason as [`SearchFuture`].
pub type ReadFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

/// Boxed future for listing workstream documents.
pub type ListFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>>;

/// Injected search backend. Implementations embed the query, rank the
/// workstream's indexed documents by cosine similarity, and return the top `k`.
/// [`FlowSearchBackend::workstream`] returning `None` signals that the run is
/// not under Flow mode (or has no active index), so the tool can error cleanly.
pub trait FlowSearchBackend: Send + Sync {
    /// Resolve the current run's project/workstream ids. Returns `None` when
    /// not running under Flow mode, so the tool can error with guidance.
    fn workstream(&self) -> Option<(String, String)>;

    fn search<'a>(
        &'a self,
        project_id: &'a str,
        workstream_id: &'a str,
        query: &'a str,
        k: usize,
    ) -> SearchFuture<'a>;

    /// Read the full contents of one persisted workstream document. Backs the
    /// `flow://<path>` internal URL so a stage agent can fetch the body of a
    /// `flow_search` hit without leaving the `read` tool.
    fn read_document<'a>(
        &'a self,
        project_id: &'a str,
        workstream_id: &'a str,
        rel_path: &'a str,
    ) -> ReadFuture<'a>;

    /// List all persisted document paths in the workstream. Backs
    /// `flow://*` (the "what's available" listing).
    fn list_documents<'a>(&'a self, project_id: &'a str, workstream_id: &'a str) -> ListFuture<'a>;
}

/// Shared, cloneable handle to an optional backend. `None` means Flow semantic
/// search is unavailable (not running inside Flow mode).
pub type FlowSearchHandle = Option<Arc<dyn FlowSearchBackend>>;

const DEFAULT_K: usize = 5;
const MAX_K: usize = 20;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct FlowSearch {
    #[param(description = "Natural-language query describing what you need")]
    query: String,
    #[param(description = "Maximum results to return (default 5, max 20)")]
    k: Option<usize>,
}

impl FlowSearch {
    pub const NAME: &str = "flow_search";
    pub const DESCRIPTION: &str = include_str!("flow_search.md");
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"query": "how is the goal doc structured?", "k": 3}]"#);

    pub fn start_header(&self) -> String {
        let k = self.k.unwrap_or(DEFAULT_K);
        format!("flow_search (k={k}): {}", self.query)
    }

    pub async fn execute(&self, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let Some(backend) = ctx.flow_search.as_ref() else {
            return Err("flow_search is unavailable: run it inside Flow mode \
                        (it needs an active workstream)"
                .to_string());
        };
        let Some((project_id, workstream_id)) = backend.workstream() else {
            return Err(
                "flow_search is only available to Flow stages (no active workstream)".to_string(),
            );
        };
        let k = self.k.unwrap_or(DEFAULT_K).clamp(1, MAX_K);
        let hits = backend
            .search(&project_id, &workstream_id, &self.query, k)
            .await?;
        Ok(ToolOutput::Plain(search_result_text(&hits)))
    }
}

fn search_result_text(hits: &[FlowSearchHit]) -> String {
    if hits.is_empty() {
        return "No matching flow documents found.".to_string();
    }
    let mut out = String::from("Relevant flow documents:\n");
    for hit in hits {
        out.push_str(&format!("- {} (score {:.3})\n", hit.path, hit.score));
    }
    out
}

super::impl_tool!(
    FlowSearch,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::RESEARCH_SUB
        | super::ToolAudience::GENERAL_SUB,
    tier = super::registry::ToolTier::Extended
);

impl ToolInvocation for FlowSearch {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(FlowSearch::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { FlowSearch::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_result_text_empty_is_helpful() {
        assert!(search_result_text(&[]).contains("No matching"));
    }

    #[test]
    fn search_result_text_lists_hits_with_scores() {
        let hits = vec![
            FlowSearchHit {
                path: "tpm.md".to_string(),
                score: 0.9,
            },
            FlowSearchHit {
                path: "plan.md".to_string(),
                score: 0.5,
            },
        ];
        let text = search_result_text(&hits);
        assert!(text.contains("tpm.md"));
        assert!(text.contains("0.900"));
        assert!(text.contains("plan.md"));
    }

    const WS_PROJECT: &str = "proj";
    const WS_ID: &str = "ws-1";

    struct StubBackend {
        hits: Vec<FlowSearchHit>,
    }

    impl FlowSearchBackend for StubBackend {
        fn workstream(&self) -> Option<(String, String)> {
            Some((WS_PROJECT.to_string(), WS_ID.to_string()))
        }
        fn search<'a>(
            &'a self,
            _project_id: &'a str,
            _workstream_id: &'a str,
            _query: &'a str,
            _k: usize,
        ) -> SearchFuture<'a> {
            let hits = self.hits.clone();
            Box::pin(async move { Ok(hits) })
        }
        fn read_document<'a>(
            &'a self,
            _project_id: &'a str,
            _workstream_id: &'a str,
            rel_path: &'a str,
        ) -> ReadFuture<'a> {
            Box::pin(async move { Ok(format!("# body of {rel_path}\n\n(flow document content)")) })
        }
        fn list_documents<'a>(
            &'a self,
            _project_id: &'a str,
            _workstream_id: &'a str,
        ) -> ListFuture<'a> {
            let paths: Vec<String> = self.hits.iter().map(|h| h.path.clone()).collect();
            Box::pin(async move { Ok(paths) })
        }
    }

    fn ctx_with(backend: FlowSearchHandle) -> ToolContext {
        let mut ctx = crate::tools::test_support::stub_ctx_with(
            &crate::AgentMode::Flow(WS_ID.to_string()),
            None,
            Some("flow:ws-1:scout"),
        );
        {
            ctx.flow_search = backend;
        }
        ctx
    }

    #[tokio::test]
    async fn execute_returns_hits_when_backend_is_wired() {
        let backend: FlowSearchHandle = Some(Arc::new(StubBackend {
            hits: vec![FlowSearchHit {
                path: "goal.md".to_string(),
                score: 0.42,
            }],
        }));
        let ctx = ctx_with(backend);
        let tool = FlowSearch {
            query: "acceptance criteria".to_string(),
            k: None,
        };
        let out = tool.execute(&ctx).await.expect("backend is wired");
        let crate::types::ToolOutput::Plain(text) = out else {
            panic!("expected Plain output, got {out:?}");
        };
        assert!(text.contains("goal.md"));
    }

    #[tokio::test]
    async fn execute_errors_with_guidance_when_backend_is_absent() {
        let ctx = ctx_with(None);
        let tool = FlowSearch {
            query: "anything".to_string(),
            k: None,
        };
        let err = tool.execute(&ctx).await.expect_err("no backend");
        assert!(
            err.contains("flow_search is unavailable"),
            "expected guidance, got: {err}"
        );
    }
}
