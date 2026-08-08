use std::sync::Arc;

use crate::agent::dedup::ToolDedupCache;
use crate::agent::format::Formatter;
use crate::agent::guardrails::ToolGuardrails;
use crate::agent::snapshot::SnapshotManager;
use crate::agent::trust::TrustTracker;
use crate::agent::validation::Validator;
use crate::cancel::CancelMap;
use crate::mcp::McpHandle;
use crate::permissions::PermissionManager;
use crate::tools::{DynamicContext, FileReadTracker, PromotedTools, ToolBuild};
use craft_config::ToolOutputLines;

pub(super) struct AgentTools {
    pub(super) permissions: Arc<PermissionManager>,
    pub(super) registry: Arc<crate::tools::ToolRegistry>,
    pub(super) tool_build: Option<ToolBuild>,
    pub(super) hooks: Option<Arc<dyn crate::Hooks>>,
    pub(super) fs: Arc<dyn crate::tools::FsBackend>,
    pub(super) snapshot_store: Arc<crate::tools::safety::SnapshotStore>,
    pub(super) pending_edits: Arc<crate::tools::ast_edit::PendingEditStore>,
    pub(super) promoted: PromotedTools,
    pub(super) dynamic: DynamicContext,
    pub(super) mcp: Option<McpHandle>,
    pub(super) dedup_cache: ToolDedupCache,
    pub(super) snapshot: SnapshotManager,
    pub(super) validator: Validator,
    pub(super) formatter: Formatter,
    pub(super) file_tracker: Arc<FileReadTracker>,
    pub(super) tool_output_lines: ToolOutputLines,
    pub(super) host_question_routing: bool,
    pub(super) subagent_cancels: Arc<CancelMap<String>>,
    pub(super) guardrails: ToolGuardrails,
    pub(super) trust_tracker: TrustTracker,
    pub(super) post_tool_empty_retried: bool,
}
