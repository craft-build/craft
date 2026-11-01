//! Workspace-scoped filesystem tools as native animist `ToolImpl`s.
//!
//! These checks prevent accidental escapes, not hostile filesystem races. The
//! caller must supply a trusted workspace and apply any approval/sandbox policy.
//!
//! Tools render their readable-text contract for both the model and ACP:
//! JSON renders to text, images become `ToolContent::Image`. Conflicts are
//! expressed as [`animist::agent::tool::ToolScheduling`] claims (see
//! [`claims`]), with a cross-agent path lock ([`locks`]) for subagents and
//! concurrent sessions sharing one workspace.

pub(crate) mod apply_patch;
pub(crate) mod bash;
mod batch;
mod claims;
mod delete;
mod edit;
pub(crate) mod error;
pub(crate) mod fuzzy_replace;
mod glob;
mod grep;
pub(crate) mod inplace_edit;
mod list;
mod list_tools;
pub(crate) mod locks;
pub(crate) mod move_file;
mod multiedit;
mod question;
pub(crate) mod read;
mod retrieve;
mod review;
mod sessions;
pub(crate) mod skill;
pub(crate) mod ssrf;
mod task;
mod todo_write;
mod view_image;
mod webfetch;
mod websearch;
pub(crate) mod worktree;
mod write;

#[cfg(test)]
mod tests;

pub use apply_patch::{ApplyPatch, ApplyPatchArgs, ApplyPatchOutput, patch_paths};
pub use bash::{
    Bash, BashArgs, BashKill, BashKillArgs, BashOutput, BashStatus, BashStatusArgs, BashWatch,
    BashWatchArgs,
};
pub use batch::{Batch, BatchArgs, BatchOutput, MAX_BATCH_SIZE};
pub use delete::{Delete, DeleteArgs, DeleteOutput};
pub use edit::{
    Edit, EditArgs, EditLines, EditLinesArgs, EditLinesOutput, EditOutput, InsertLines,
    InsertLinesArgs, InsertLinesOutput,
};
pub use glob::{Glob, GlobArgs, GlobOutput};
pub use grep::{Grep, GrepArgs, GrepMatch, GrepOutput};
pub use list::{List, ListArgs, ListOutput};
pub use list_tools::{ListTools, ListToolsArgs, ListToolsOutput};
pub use move_file::{MoveFile, MoveFileArgs, MoveFileOutput};
pub use multiedit::{EditEntry, MultiEdit, MultiEditArgs, MultiEditOutput};
pub use question::{
    ASK_TIMEOUT, AskQuestions, DismissAsk, Question, QuestionAnswer, QuestionArgs, QuestionOption,
    QuestionOutput, QuestionSpec, decode_answer, encode_answer, is_question_answer,
};
pub use read::{Read, ReadArgs, ReadLine, ReadOutput};
pub use retrieve::{Retrieve, RetrieveArgs, RetrieveOutput};
pub use review::{REVIEWER_PROMPT, Review, ReviewArgs, ReviewOutput};
pub use sessions::{Sessions, SessionsArgs, SessionsOutput};
pub use skill::{Skill, SkillArgs, SkillOutput};
pub use task::{NoSubagents, SpawnSubagent, Task, TaskArgs, TaskOutput};
pub(crate) use todo_write::flatten_todos;
pub use todo_write::{Todo, TodoWrite, TodoWriteArgs, TodoWriteOutput};
pub use view_image::{ViewImage, ViewImageArgs, ViewImageOutput};
pub use webfetch::{Webfetch, WebfetchArgs, WebfetchOutput};
pub use websearch::{Websearch, WebsearchArgs, WebsearchOutput};
pub use write::{Write, WriteArgs, WriteOutput};

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use animist::agent::tool::{
    ToolContents, ToolContext, ToolDefinition, ToolExecutor, ToolExecutorImpl, ToolOutput,
    ToolRegistry, ToolScheduling,
};
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncReadExt as _;

use crate::mcp::session::McpPart;
use crate::mcp::{McpHandle, McpToolOutput};

use crate::history::ImageSource;

pub(crate) const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_LINE_BYTES: usize = 2048;

/// Every tool name registered by [`Workspace::registry`]. MCP server names are
/// rejected when they collide with one of these (they would shadow a builtin).
const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read",
    "grep",
    "glob",
    "list",
    "edit",
    "edit_lines",
    "insert_lines",
    "multiedit",
    "apply_patch",
    "write",
    "delete",
    "move_file",
    "bash",
    "bash_status",
    "bash_watch",
    "bash_kill",
    "todo_write",
    "retrieve",
    "skill",
    "sessions",
    "webfetch",
    "websearch",
    "view_image",
    "question",
    "batch",
    "list_tools",
    "task",
    "review",
];

pub fn is_builtin_tool(name: &str) -> bool {
    BUILTIN_TOOL_NAMES.contains(&name) || crate::knowledge::is_argosy_tool(name)
}

/// Every native tool name the registry can serve: the builtin table plus
/// the argosy natives. The CLI's tool-name validation checks against this
/// so the valid-name list cannot drift from the registry.
pub fn native_tool_names() -> Vec<&'static str> {
    let mut names = BUILTIN_TOOL_NAMES.to_vec();
    names.extend(
        crate::knowledge::tool_definitions()
            .iter()
            .map(|def| def.native),
    );
    names.sort_unstable();
    names.dedup();
    names
}

/// One fixed root shared by all tools in an agent, plus the state every
/// tool family needs. Tools hold the workspace (or a slice of it) and run
/// directly as animist `ToolImpl`s: there is no dispatcher between the
/// registry and the tool, and no global lock around filesystem I/O.
/// Ordering within one agent comes from scheduling claims; ordering across
/// agents (subagents, concurrent sessions) comes from [`locks::PathLocks`].
#[derive(Clone)]
pub struct Workspace {
    root: Arc<PathBuf>,
    locks: locks::PathLocks,
    loaded_instructions: crate::instructions::LoadedInstructions,
    todos: todo_write::TodoStore,
    snapshots: crate::snapshot::SnapshotManager,
    bash_jobs: bash::BashJobs,
    /// Resolved once so the `sessions` tool lists without re-walking XDG.
    state_dir: Option<crate::storage::StateDir>,
    /// Host seam for the `question` tool (A.5): the TUI installs the
    /// interactive asker per turn; the default dismisses headlessly.
    questions: Arc<dyn question::AskQuestions>,
    /// Session plan file (C.17): the one write target allowed outside the
    /// workspace root. Shared so every clone sees the set, and kept for the
    /// whole session once allocated — mode switches must not revoke it.
    plan_path: Arc<RwLock<Option<PathBuf>>>,
    /// MCP client handle (B.11): when set, [`Workspace::registry`] appends
    /// one executor per published MCP tool under its `server__tool` wire
    /// name. Shared like every other cell so the set is visible through
    /// clones.
    mcp: Arc<RwLock<Option<McpHandle>>>,
    /// Host seam for the `task` tool (A.5): the TUI and headless surfaces
    /// install a subagent launcher per turn; the default reports that
    /// subagents are unavailable.
    subagents: Arc<dyn task::SpawnSubagent>,
    /// Session sandbox policy (resolved from config/env at construction or
    /// installed by the surface). `bash` reads it at execution time.
    sandbox: crate::sandbox::SandboxPolicyCell,
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> error::Result<Self> {
        let root = std::fs::canonicalize(root).map_err(error::io_error)?;
        if !root.is_dir() {
            return Err(error::invalid("workspace root must be a directory"));
        }
        if root.components().any(|c| is_git_component(c.as_os_str())) {
            return Err(error::denied("workspace must not be inside Git metadata"));
        }
        Ok(Self {
            locks: locks::PathLocks::new(),
            root: Arc::new(root.clone()),
            loaded_instructions: crate::instructions::LoadedInstructions::new(),
            todos: Default::default(),
            snapshots: crate::snapshot::SnapshotManager::new(root.clone()),
            bash_jobs: Default::default(),
            state_dir: crate::storage::StateDir::resolve().ok(),
            questions: Arc::new(question::DismissAsk),
            plan_path: Arc::new(RwLock::new(None)),
            mcp: Arc::new(RwLock::new(None)),
            subagents: Arc::new(task::NoSubagents),
            sandbox: Default::default(),
        })
    }

    /// Test seam: point the plans-dir read exemption at an isolated state
    /// dir instead of the developer's real one.
    #[cfg(test)]
    pub(crate) fn with_state_dir(mut self, dir: crate::storage::StateDir) -> Self {
        self.state_dir = Some(dir);
        self
    }

    /// Install the subagent seam (A.5). Taken on a workspace clone per turn
    /// (like the question asker) so the launcher can carry the turn's cancel
    /// token, history snapshot, and event channel.
    pub fn with_subagents(mut self, spawn: Arc<dyn task::SpawnSubagent>) -> Self {
        self.subagents = spawn;
        self
    }

    /// A child workspace rooted at `root` for a worktree-isolated subagent
    /// (hardening/04): tools, snapshots, bash jobs and the plan file anchor
    /// at the child root, while the session's cross-root services —
    /// instruction dedupe, the todo plan, MCP tools, the sandbox policy and
    /// the cross-agent path locks — stay shared with this workspace. The
    /// child gets no subagent seam: worktree children cannot recurse.
    pub(crate) fn child_workspace(&self, root: impl AsRef<Path>) -> error::Result<Self> {
        let mut child = Self::new(root)?;
        child.loaded_instructions = self.loaded_instructions.clone();
        child.todos = self.todos.clone();
        child.state_dir = self.state_dir.clone();
        child.mcp = self.mcp.clone();
        child.sandbox = self.sandbox.clone();
        child.locks = self.locks.clone();
        Ok(child)
    }

    /// Install the session's MCP client (B.11). Call before the first turn
    /// registers tools, after awaiting `McpHandle::ready` where the caller
    /// must not ship a prompt without the MCP tools.
    pub fn set_mcp(&self, handle: Option<McpHandle>) {
        let mut cell = self.mcp.write().unwrap_or_else(|e| e.into_inner());
        *cell = handle;
    }

    pub fn mcp(&self) -> Option<McpHandle> {
        self.mcp.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Install the session's resolved sandbox policy (from `[sandbox]`
    /// config + yolo). Call before the first turn registers tools.
    pub fn set_sandbox_policy(&self, policy: crate::sandbox::SandboxPolicy) {
        let mut cell = self.sandbox.write().unwrap_or_else(|e| e.into_inner());
        cell.policy = policy;
    }

    pub fn with_sandbox_policy(self, policy: crate::sandbox::SandboxPolicy) -> Self {
        self.set_sandbox_policy(policy);
        self
    }

    pub fn sandbox_policy(&self) -> crate::sandbox::SandboxPolicy {
        self.sandbox
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .policy
            .clone()
    }

    pub(crate) fn sandbox_cell(&self) -> crate::sandbox::SandboxPolicyCell {
        self.sandbox.clone()
    }

    /// Share the session's instruction-dedupe set so instruction files are
    /// injected into tool output at most once per session.
    pub fn with_loaded_instructions(
        mut self,
        loaded: crate::instructions::LoadedInstructions,
    ) -> Self {
        self.loaded_instructions = loaded;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The workspace root as the string anchor for path normalization: the
    /// same spelling scheduling claims, path locks, and the dedup cache use.
    pub(crate) fn root_key(&self) -> Option<String> {
        Some(self.root.display().to_string())
    }

    pub(crate) fn locks(&self) -> &locks::PathLocks {
        &self.locks
    }

    /// Cross-agent write guard for one call's write targets: the same
    /// normalized keys the scheduling claim uses, held for the duration of
    /// the write.
    pub(crate) async fn write_guard(
        &self,
        paths: &[String],
        cancel: &animist::agent::Cancellation,
    ) -> error::Result<locks::WriteGuard> {
        let keys = claims::write_keys(self.root_key().as_deref(), paths);
        self.locks.write(&keys, cancel).await
    }

    /// Cross-agent read guard for one read target: a shared guard under the
    /// root, so a workspace-wide writer still excludes it.
    pub(crate) async fn read_guard(
        &self,
        paths: &[String],
        cancel: &animist::agent::Cancellation,
    ) -> error::Result<locks::ReadGuard> {
        self.locks
            .read(
                &claims::read_keys(self.root_key().as_deref(), paths),
                cancel,
            )
            .await
    }

    /// Set (or clear) the session's plan file (C.17). The plan file lives
    /// in the state dir, outside the workspace root; `walk` exempts exactly
    /// this path from containment. Install when a plan is allocated and
    /// leave it for the rest of the session — Build-mode turns that
    /// implement the plan must keep the exception.
    pub fn set_plan_path(&self, path: Option<PathBuf>) {
        *self.plan_path.write().unwrap_or_else(|e| e.into_inner()) = path;
    }

    fn plan_target(&self, requested: &str) -> bool {
        let root = self.root_key();
        let guard = self.plan_path.read().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().is_some_and(|plan| {
            crate::run::dedup::normalize_write_path_with_root(root.as_deref(), requested)
                == crate::run::dedup::normalize_write_path_with_root(
                    root.as_deref(),
                    &plan.display().to_string(),
                )
        })
    }

    /// Read-only exemption for the state dir's plans directory
    /// (`<state>/plans`): the agent may read the plan files it holds —
    /// including plans from earlier sessions — but never write there.
    /// Only the exact session plan file ([`Self::plan_target`]) is
    /// writable outside the workspace.
    fn plans_read_target(&self, requested: &str) -> bool {
        let root = self.root_key();
        let normalize =
            |path: &str| crate::run::dedup::normalize_write_path_with_root(root.as_deref(), path);
        self.state_dir.as_ref().is_some_and(|dir| {
            let plans = normalize(&dir.path().join("plans").display().to_string());
            Path::new(&normalize(requested)).starts_with(plans)
        })
    }

    /// Install the question seam (A.5). Taken on a workspace clone per
    /// turn (like the subagent launcher) so the asker can carry the turn's
    /// cancel token and event channel.
    pub fn with_questions(mut self, asker: Arc<dyn question::AskQuestions>) -> Self {
        self.questions = asker;
        self
    }

    /// The session-shared snapshot manager backing `/undo`.
    pub fn snapshots(&self) -> &crate::snapshot::SnapshotManager {
        &self.snapshots
    }

    /// Reset the todo store for a fresh session (`/new`, `/clear`): the
    /// workspace outlives sessions, but the previous session's plan must
    /// not leak into the next one.
    pub fn clear_todos(&self) {
        self.todos.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Test seam: observe the todo store, so tests can prove a session
    /// reset actually dropped the previous plan.
    #[cfg(test)]
    pub(crate) fn todos(&self) -> Vec<Todo> {
        self.todos.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Capture `path`'s pre-write contents for `/undo`. Write-family tools
    /// call this after resolving their target and before mutating.
    pub(crate) async fn note_snapshot(&self, path: &Path) {
        // sync snapshot store: the capture reads up to 5 MiB under its
        // state lock.
        let snapshots = self.snapshots.clone();
        let path = path.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || snapshots.note(&path)).await;
    }

    /// The full tool registry for a turn: every builtin, the argosy
    /// knowledge tools under their native names, `batch`, the session's MCP
    /// tools, and the self-describing `list_tools` entry. One constructor
    /// row per tool lives in [`Self::register_builtins`]; no dispatcher or
    /// decorator sits between the registry and the tool.
    #[expect(
        clippy::result_large_err,
        reason = "AgentError is animist's public error type and cannot be boxed here"
    )]
    pub fn registry(
        &self,
        history: &animist::history::History,
    ) -> std::result::Result<ToolRegistry, animist::agent::AgentError> {
        let mut registry = ToolRegistry::default();
        self.register_builtins(
            &mut registry,
            self.sandbox_cell(),
            BUILTIN_TOOL_NAMES,
            history,
        )?;
        registry.register(Batch)?;
        // The full registry serves every argosy native; the subagent and
        // reviewer budgets filter by their allowed lists.
        self.register_argosy(&mut registry, None)?;
        self.register_mcp(&mut registry)?;
        self.register_list_tools(&mut registry)?;
        Ok(registry)
    }

    /// The restricted registry for a `task`-spawned subagent (A.5):
    /// `general` adds the write family to the read-only research set;
    /// neither includes `task`, `question`, or `sessions`. MCP tools are
    /// inherited, so a subagent's fan-outs cannot escape its tool budget.
    #[expect(
        clippy::result_large_err,
        reason = "AgentError is animist's public error type and cannot be boxed here"
    )]
    pub fn subagent_registry(
        &self,
        general: bool,
        history: &animist::history::History,
    ) -> std::result::Result<ToolRegistry, animist::agent::AgentError> {
        let allowed: &[&str] = if general {
            crate::subagent::GENERAL_TOOLS
        } else {
            crate::subagent::RESEARCH_TOOLS
        };
        let mut registry = ToolRegistry::default();
        // Read-only workers never inherit workspace-write, even though
        // their budget has no bash today: the shared cell must not leak it
        // to future additions.
        let sandbox = if general {
            self.sandbox_cell()
        } else {
            self.frozen_read_only_cell()
        };
        self.register_builtins(&mut registry, sandbox, allowed, history)?;
        if allowed.contains(&"batch") {
            registry.register(Batch)?;
        }
        self.register_argosy(&mut registry, Some(allowed))?;
        self.register_mcp(&mut registry)?;
        self.register_list_tools(&mut registry)?;
        Ok(registry)
    }

    /// The reviewer subagent's registry (Phase 5 of the argosy
    /// integration): the read-only research set plus the argosy review
    /// workflow. The reviewer never writes.
    #[expect(
        clippy::result_large_err,
        reason = "AgentError is animist's public error type and cannot be boxed here"
    )]
    pub fn reviewer_registry(
        &self,
        history: &animist::history::History,
    ) -> std::result::Result<ToolRegistry, animist::agent::AgentError> {
        let allowed = crate::subagent::REVIEWER_TOOLS;
        let mut registry = ToolRegistry::default();
        // The reviewer never writes.
        self.register_builtins(
            &mut registry,
            self.frozen_read_only_cell(),
            allowed,
            history,
        )?;
        self.register_argosy(&mut registry, Some(allowed))?;
        self.register_list_tools(&mut registry)?;
        Ok(registry)
    }

    /// A frozen read-only sandbox cell: never observes later session policy
    /// installs.
    fn frozen_read_only_cell(&self) -> crate::sandbox::SandboxPolicyCell {
        let mut policy = self.sandbox_policy();
        policy.mode = crate::sandbox::SandboxMode::ReadOnly;
        policy.writable_roots = Vec::new();
        Arc::new(RwLock::new(crate::sandbox::SandboxState {
            policy,
            note_shown: false,
        }))
    }

    /// One registration row per tool, keyed by the budget name the
    /// subagent tool sets use (`move_file` for the `move` tool's wire
    /// name): a wire name absent here is simply never registered, never
    /// rescued by a wildcard arm. `bash` reads `sandbox` at execution time;
    /// read-only workers get a frozen read-only cell so the shared table
    /// cannot leak workspace-write to future additions.
    #[expect(
        clippy::result_large_err,
        reason = "AgentError is animist's public error type and cannot be boxed here"
    )]
    fn register_builtins(
        &self,
        registry: &mut ToolRegistry,
        sandbox: crate::sandbox::SandboxPolicyCell,
        allowed: &[&str],
        history: &animist::history::History,
    ) -> std::result::Result<(), animist::agent::AgentError> {
        if allowed.contains(&"read") {
            registry.register(Read(self.clone()))?;
        }
        if allowed.contains(&"grep") {
            registry.register(Grep(self.clone()))?;
        }
        if allowed.contains(&"glob") {
            registry.register(Glob(self.clone()))?;
        }
        if allowed.contains(&"list") {
            registry.register(List(self.clone()))?;
        }
        if allowed.contains(&"edit") {
            registry.register(Edit(self.clone()))?;
        }
        if allowed.contains(&"edit_lines") {
            registry.register(EditLines(self.clone()))?;
        }
        if allowed.contains(&"insert_lines") {
            registry.register(InsertLines(self.clone()))?;
        }
        if allowed.contains(&"multiedit") {
            registry.register(MultiEdit(self.clone()))?;
        }
        if allowed.contains(&"apply_patch") {
            registry.register(ApplyPatch(self.clone()))?;
        }
        if allowed.contains(&"write") {
            registry.register(Write(self.clone()))?;
        }
        if allowed.contains(&"delete") {
            registry.register(Delete(self.clone()))?;
        }
        if allowed.contains(&"move_file") {
            registry.register(MoveFile(self.clone()))?;
        }
        if allowed.contains(&"bash") {
            registry.register(Bash(self.clone(), sandbox))?;
        }
        if allowed.contains(&"bash_status") {
            registry.register(BashStatus(self.clone()))?;
        }
        if allowed.contains(&"bash_watch") {
            registry.register(BashWatch(self.clone()))?;
        }
        if allowed.contains(&"bash_kill") {
            registry.register(BashKill(self.clone()))?;
        }
        if allowed.contains(&"todo_write") {
            registry.register(TodoWrite(self.clone()))?;
        }
        if allowed.contains(&"retrieve") {
            registry.register(Retrieve(history.clone()))?;
        }
        if allowed.contains(&"skill") {
            registry.register(Skill::new(self.root().to_path_buf()))?;
        }
        if allowed.contains(&"sessions") {
            registry.register(Sessions::new(
                self.root().display().to_string(),
                self.state_dir.clone(),
            ))?;
        }
        if allowed.contains(&"webfetch") {
            registry.register(Webfetch)?;
        }
        if allowed.contains(&"websearch") {
            registry.register(Websearch)?;
        }
        if allowed.contains(&"view_image") {
            registry.register(ViewImage(self.clone()))?;
        }
        if allowed.contains(&"question") {
            registry.register(Question(self.questions.clone()))?;
        }
        if allowed.contains(&"task") {
            registry.register(Task(self.subagents.clone()))?;
        }
        if allowed.contains(&"review") {
            registry.register(Review(self.subagents.clone()))?;
        }
        Ok(())
    }

    /// The argosy knowledge tools (Phase 1 of the argosy integration): one
    /// executor per argosy definition under its native name, filtered by
    /// the same budget list as the builtins. Reports render to text via
    /// [`crate::knowledge::render_report_text`] — embedded bodies
    /// (`content`, pre-rendered `text`, `drafted`) surface directly instead
    /// of as pretty JSON.
    #[expect(
        clippy::result_large_err,
        reason = "AgentError is animist's public error type and cannot be boxed here"
    )]
    fn register_argosy(
        &self,
        registry: &mut ToolRegistry,
        allowed: Option<&[&str]>,
    ) -> std::result::Result<(), animist::agent::AgentError> {
        for def in crate::knowledge::tool_definitions() {
            if let Some(allowed) = allowed
                && !allowed.contains(&def.native)
            {
                continue;
            }
            registry.register_executor(ToolExecutor::new(ArgosyTool {
                native: def.native,
                description: def.description.clone(),
                schema: def.schema.clone(),
                reads: self.normalized_root(),
            }))?;
        }
        Ok(())
    }

    /// One executor per published MCP tool (B.11). The model sees the
    /// `server__tool` wire name; the executor resolves it back to the
    /// qualified name the manager's tool index is keyed by.
    #[expect(
        clippy::result_large_err,
        reason = "AgentError is animist's public error type and cannot be boxed here"
    )]
    fn register_mcp(
        &self,
        registry: &mut ToolRegistry,
    ) -> std::result::Result<(), animist::agent::AgentError> {
        let Some(handle) = self.mcp() else {
            return Ok(());
        };
        for descriptor in handle.tool_descriptors() {
            registry.register_executor(ToolExecutor::new(McpTool {
                handle: handle.clone(),
                qualified: descriptor.qualified_name.clone(),
                definition: ToolDefinition::builder()
                    .name(descriptor.wire_name.clone())
                    .description(descriptor.description.clone())
                    .input_schema(descriptor.parameters.clone())
                    .build(),
                read_only: descriptor.read_only_hint == Some(true),
            }))?;
        }
        Ok(())
    }

    /// `list_tools` describes the finished table, so it registers last.
    #[expect(
        clippy::result_large_err,
        reason = "AgentError is animist's public error type and cannot be boxed here"
    )]
    fn register_list_tools(
        &self,
        registry: &mut ToolRegistry,
    ) -> std::result::Result<(), animist::agent::AgentError> {
        let definitions = registry.definitions();
        registry.register(ListTools(Arc::new(definitions)))?;
        Ok(())
    }

    fn normalized_root(&self) -> String {
        crate::run::dedup::normalize_write_path_with_root(
            self.root_key().as_deref(),
            &self.root.display().to_string(),
        )
    }

    /// Shared validation walk for the read and write paths: strips the
    /// workspace prefix from absolute requests, then walks component by
    /// component rejecting `..`, Git metadata, and symlinked components.
    /// With `create_dirs` (write path), missing directory components are
    /// created and the final component is returned as the file name instead
    /// of being appended. Metadata-only: safe to call from async tool bodies
    /// and blocking workers alike.
    fn walk<'a>(
        &self,
        requested: &'a str,
        create_dirs: bool,
    ) -> error::Result<(PathBuf, Option<Component<'a>>)> {
        if requested.is_empty() {
            return Err(error::invalid("path must not be empty"));
        }
        // Outside-workspace exemptions (C.17): the session's plan file, plus
        // — read only — the state dir's plans directory. Both are exempt from
        // workspace containment only; the component validation below still
        // applies, just walked from the path's own root.
        let exempt =
            self.plan_target(requested) || (!create_dirs && self.plans_read_target(requested));
        let requested = Path::new(requested);
        let (relative, mut path) = if exempt {
            (requested, PathBuf::new())
        } else if requested.is_absolute() {
            (
                requested
                    .strip_prefix(self.root())
                    .map_err(|_| error::denied("path is outside the workspace"))?,
                self.root().to_path_buf(),
            )
        } else {
            (requested, self.root().to_path_buf())
        };
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            if create_dirs && components.peek().is_none() {
                return match component {
                    Component::Normal(name) if !is_git_component(name) => {
                        Ok((path, Some(component)))
                    }
                    _ => Err(error::denied(
                        "paths must remain inside the workspace; '..', '.', and Git metadata are not allowed",
                    )),
                };
            }
            match component {
                Component::CurDir => continue,
                Component::Normal(name) if is_git_component(name) => {
                    return Err(error::denied("access to Git metadata is not allowed"));
                }
                // Exempt targets are absolute paths outside the workspace:
                // anchor the walk at the path's own root/prefix.
                Component::RootDir | Component::Prefix(_) if exempt => {
                    path = component.as_os_str().into();
                    continue;
                }
                Component::Normal(_) => path.push(component),
                _ => {
                    return Err(error::denied(
                        "paths must remain inside the workspace; '..' is not allowed",
                    ));
                }
            }
            if create_dirs
                && let Err(error) = std::fs::create_dir(&path)
                && error.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(error::io_error(error));
            }
            if std::fs::symlink_metadata(&path)
                .map_err(error::io_error)?
                .file_type()
                .is_symlink()
            {
                return Err(error::denied("symlink paths are not supported"));
            }
        }
        Ok((path, None))
    }

    /// Existing paths only. Reject `..`, Git metadata, and every symlink
    /// component, including links whose targets are inside the workspace.
    pub(crate) fn resolve(&self, requested: &str) -> error::Result<PathBuf> {
        self.walk(requested, false).map(|(path, _)| path)
    }

    pub(crate) fn file(&self, requested: &str) -> error::Result<PathBuf> {
        let path = self.resolve(requested)?;
        if !std::fs::symlink_metadata(&path)
            .map_err(error::io_error)?
            .is_file()
        {
            return Err(error::invalid("path must identify a regular file"));
        }
        Ok(path)
    }

    /// Resolution for writes: like [`Self::resolve`], but the final component
    /// may not exist yet, missing parent directories are created, and an
    /// existing destination must be a regular file (never a symlink).
    pub(crate) fn target(&self, requested: &str) -> error::Result<PathBuf> {
        let (mut path, name) = self.walk(requested, true)?;
        let Some(component) = name else {
            return Err(error::invalid("path must name a file, not a directory"));
        };
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return Err(error::invalid("path must identify a regular file")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error::io_error(error)),
        }
        Ok(path)
    }

    pub(crate) fn display(&self, path: &Path) -> String {
        path.strip_prefix(self.root())
            .unwrap_or(path)
            .iter()
            .map(|component| component.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// One argosy knowledge tool under its native name. Claim: `reads` of the
/// workspace root key — knowledge queries overlap file reads and each other
/// (the service serializes its own mutations internally) and exclude
/// writers, which claim the root key as their topmost ancestor.
struct ArgosyTool {
    native: &'static str,
    description: String,
    schema: Value,
    reads: String,
}

#[async_trait]
impl ToolExecutorImpl for ArgosyTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builder()
            .name(self.native)
            .description(self.description.clone())
            .input_schema(self.schema.clone())
            .build()
    }

    fn scheduling(&self, _input: &Value) -> ToolScheduling {
        ToolScheduling::Resources {
            reads: vec![self.reads.clone()],
            writes: Vec::new(),
        }
    }

    async fn execute(
        &self,
        _context: &ToolContext<'_>,
        input: Value,
    ) -> std::result::Result<ToolOutput, animist::agent::tool::ToolError> {
        match crate::knowledge::call_tool(self.native.to_owned(), input).await {
            Ok(report) => Ok(error::text_output(crate::knowledge::render_report_text(
                self.native,
                &report,
            ))),
            Err(message) => Ok(error::error_result(message)),
        }
    }
}

/// One published MCP tool. Read-only servers (their own annotation) claim
/// independence and overlap other calls; every other MCP tool runs
/// exclusively, because a server can touch anything.
struct McpTool {
    handle: McpHandle,
    qualified: String,
    definition: ToolDefinition,
    read_only: bool,
}

#[async_trait]
impl ToolExecutorImpl for McpTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn scheduling(&self, _input: &Value) -> ToolScheduling {
        if self.read_only {
            ToolScheduling::Resources {
                reads: Vec::new(),
                writes: Vec::new(),
            }
        } else {
            ToolScheduling::Exclusive
        }
    }

    async fn execute(
        &self,
        context: &ToolContext<'_>,
        input: Value,
    ) -> std::result::Result<ToolOutput, animist::agent::tool::ToolError> {
        // The turn's cancellation stops the in-flight server call: the
        // session notifies the server instead of parking until it finishes.
        let outcome = self
            .handle
            .call_tool_cancellable(&self.qualified, &input, &context.run.cancellation)
            .await;
        Ok(match outcome {
            Ok(output) => error::contents_output(mcp_contents(output)),
            Err(error) => error::error_result(error.to_string()),
        })
    }
}

/// Map an MCP tool's output to model-visible contents, preserving the
/// server's block order: text parts as text, images as base64 image content
/// interleaved where they appeared.
fn mcp_contents(output: McpToolOutput) -> ToolContents {
    if !output.has_images() {
        return output.joined_text().into();
    }
    let parts = output
        .parts
        .into_iter()
        .map(|part| match part {
            McpPart::Text(text) => animist::agent::tool::ToolContent::Text { text },
            McpPart::Image(image) => match media_type(&image.mime) {
                Some(media_type) => animist::agent::tool::ToolContent::Image {
                    source: ImageSource::Base64 {
                        media_type: media_type.to_owned(),
                        data: image.data,
                    },
                },
                // Unknown mimes cannot be served as image content; note the
                // drop instead of losing the image silently.
                None => animist::agent::tool::ToolContent::Text {
                    text: format!("[unsupported image mime: {}]", image.mime),
                },
            },
        })
        .collect::<Vec<_>>();
    ToolContents::from(parts)
}

/// The model-visible media type for an MCP image mime. `image/heic`,
/// `image/heif`, and `image/svg+xml` are served as `image/png`, as the rig
/// bridge did.
fn media_type(mime: &str) -> Option<&'static str> {
    match mime.to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/png" => Some("image/png"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        "image/heic" | "image/heif" | "image/svg+xml" => Some("image/png"),
        _ => None,
    }
}

fn is_git_component(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().eq_ignore_ascii_case(".git")
}

pub(crate) async fn read_bytes(path: &Path) -> error::Result<Vec<u8>> {
    let file = tokio::fs::File::open(path).await.map_err(error::io_error)?;
    if !file.metadata().await.map_err(error::io_error)?.is_file() {
        return Err(error::invalid("path must identify a regular file"));
    }
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(error::io_error)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(error::invalid("file exceeds the 8 MiB tool size limit"));
    }
    Ok(bytes)
}

/// Stage `after` beside `parent` as a fresh temp file carrying
/// `permissions`, fsynced and ready for an atomic rename. Callers hold the
/// workspace path lock across stage → verify → [`commit_staged`].
pub(crate) async fn stage_bytes(
    parent: &Path,
    after: &[u8],
    permissions: &std::fs::Permissions,
) -> error::Result<tempfile::NamedTempFile> {
    stage(parent, after, Some(permissions)).await
}

/// Like [`stage_bytes`] without permission preservation: fresh files take
/// the temp file's default permissions, as full-file writes always have.
pub(crate) async fn stage_bytes_for_write(
    parent: &Path,
    after: &[u8],
) -> error::Result<tempfile::NamedTempFile> {
    stage(parent, after, None).await
}

async fn stage(
    parent: &Path,
    after: &[u8],
    permissions: Option<&std::fs::Permissions>,
) -> error::Result<tempfile::NamedTempFile> {
    // tempfile + fsync are sync-only; keep the staged write off the executor.
    let parent = parent.to_path_buf();
    let after = after.to_vec();
    let permissions = permissions.cloned();
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut staged = tempfile::NamedTempFile::new_in(&parent).map_err(error::io_error)?;
        staged.write_all(&after).map_err(error::io_error)?;
        if let Some(permissions) = permissions {
            staged
                .as_file()
                .set_permissions(permissions)
                .map_err(error::io_error)?;
        }
        staged.as_file().sync_all().map_err(error::io_error)?;
        Ok(staged)
    })
    .await
    .map_err(|source| error::Error::Blocking {
        source: Box::new(source),
    })?
}

/// Atomically move a staged file onto `path`: one rename syscall.
pub(crate) async fn commit_staged(
    staged: tempfile::NamedTempFile,
    path: &Path,
) -> error::Result<()> {
    staged
        .persist(path)
        .map(|_| ())
        .map_err(|error| error::io_error(error.error))
}

pub(crate) fn text(bytes: Vec<u8>) -> error::Result<String> {
    if bytes.contains(&0) {
        return Err(error::invalid(
            "binary files are not supported by this text tool",
        ));
    }
    String::from_utf8(bytes).map_err(|_| error::invalid("file is not valid UTF-8 text"))
}

pub(crate) fn clip(text: &str, max_bytes: usize) -> (&str, bool) {
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], end < text.len())
}

/// Clip tool output at [`MAX_OUTPUT_BYTES`] (char-boundary safe, via [`clip`])
/// and append a marker when anything was dropped. The single output-clipping
/// implementation shared by the output-producing tools.
pub(crate) fn truncate_output(text: &str) -> String {
    let (clipped, truncated) = clip(text, MAX_OUTPUT_BYTES);
    if truncated {
        format!("{clipped}\n... [output truncated]")
    } else {
        clipped.to_string()
    }
}

#[cfg(test)]
mod plan_walk_tests {
    //! Plan-mode (C.17) containment exemption: only the workspace-boundary
    //! rule is relaxed for the plan file; the component validation (no
    //! `..`, no symlinks, no Git metadata) still applies.

    use super::*;

    fn plan_workspace() -> (tempfile::TempDir, tempfile::TempDir, Workspace, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let state = tempfile::tempdir().unwrap();
        // Canonicalize so the plan path itself is free of symlinked
        // components (macOS tempdirs live under symlinked /var).
        let plan = state.path().canonicalize().unwrap().join("PLAN.md");
        (dir, state, workspace, plan)
    }

    #[tokio::test]
    async fn plan_file_write_outside_workspace_still_works() {
        let (_dir, _state, workspace, plan) = plan_workspace();
        workspace.set_plan_path(Some(plan.clone()));
        let requested = plan.to_string_lossy().into_owned();
        let target = workspace.target(&requested).unwrap();
        assert_eq!(target, plan);
        std::fs::write(&target, "the plan").unwrap();
        assert_eq!(workspace.resolve(&requested).unwrap(), plan);
        assert_eq!(std::fs::read_to_string(&plan).unwrap(), "the plan");
    }

    #[tokio::test]
    async fn plan_file_write_creates_missing_parent_dirs() {
        let (_dir, _state, workspace, plan) = plan_workspace();
        let nested = plan.parent().unwrap().join("nested/deep/PLAN.md");
        workspace.set_plan_path(Some(nested.clone()));
        let target = workspace.target(&nested.to_string_lossy()).unwrap();
        assert_eq!(target, nested);
        std::fs::write(&target, "plan").unwrap();
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "plan");
    }

    #[tokio::test]
    async fn plan_path_with_parent_component_is_rejected() {
        let (_dir, _state, workspace, plan) = plan_workspace();
        workspace.set_plan_path(Some(plan.clone()));
        // `sub/../PLAN.md` normalizes to the plan path (so it passes the
        // plan_target check) but must still fail component validation.
        let sneaky = plan
            .parent()
            .unwrap()
            .join("sub")
            .join("..")
            .join("PLAN.md");
        let sneaky = sneaky.to_string_lossy().into_owned();
        assert!(workspace.target(&sneaky).is_err());
        assert!(workspace.resolve(&sneaky).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn plan_path_through_symlinked_directory_is_rejected() {
        use std::os::unix::fs::symlink;
        let (_dir, _state, workspace, plan) = plan_workspace();
        workspace.set_plan_path(Some(plan.clone()));
        // A symlink to the plan directory resolves (in normalization) to the
        // plan path, but the raw walk must refuse the symlinked component.
        let link_dir = tempfile::tempdir().unwrap();
        let link = link_dir.path().canonicalize().unwrap().join("link");
        symlink(plan.parent().unwrap(), &link).unwrap();
        let via_link = link.join("PLAN.md").to_string_lossy().into_owned();
        assert!(workspace.target(&via_link).is_err());
        assert!(workspace.resolve(&via_link).is_err());
    }

    /// Workspace with the plans-dir exemption pointed at an isolated state
    /// dir. Returns both tempdir guards plus the canonical state path so
    /// requested paths share its spelling (macOS tempdirs live under
    /// symlinked /var).
    fn plans_workspace() -> (tempfile::TempDir, tempfile::TempDir, PathBuf, Workspace) {
        let (root, state, workspace, _) = plan_workspace();
        let canonical = state.path().canonicalize().unwrap();
        let state_dir = crate::storage::StateDir::from_path(canonical.clone());
        (root, state, canonical, workspace.with_state_dir(state_dir))
    }

    #[tokio::test]
    async fn plans_dir_reads_outside_workspace_are_exempt() {
        let (_root, _state, state, workspace) = plans_workspace();
        let earlier = state.join("plans/old-plan.md");
        std::fs::create_dir_all(earlier.parent().unwrap()).unwrap();
        std::fs::write(&earlier, "# earlier session's plan").unwrap();
        let requested = earlier.to_string_lossy().into_owned();
        assert_eq!(workspace.resolve(&requested).unwrap(), earlier);
        assert_eq!(workspace.file(&requested).unwrap(), earlier);
    }

    #[tokio::test]
    async fn plans_dir_writes_outside_workspace_are_refused() {
        let (_root, _state, state, workspace) = plans_workspace();
        let earlier = state.join("plans/old-plan.md");
        std::fs::create_dir_all(earlier.parent().unwrap()).unwrap();
        let requested = earlier.to_string_lossy().into_owned();
        assert!(workspace.target(&requested).is_err());
    }

    #[tokio::test]
    async fn state_dir_paths_outside_plans_are_still_refused() {
        let (_root, _state, state, workspace) = plans_workspace();
        let sessions = state.join("sessions/session.json");
        let requested = sessions.to_string_lossy().into_owned();
        assert!(workspace.resolve(&requested).is_err());
        assert!(workspace.target(&requested).is_err());
    }

    /// The implement flow: after clear-context-and-implement the turn runs
    /// in Build mode, which installs no plan path — the session's plan file
    /// must keep its exemption so the agent can read (and update) it.
    #[tokio::test]
    async fn plan_file_exemption_survives_the_build_mode_implement_turn() {
        let (_dir, _state, workspace, plan) = plan_workspace();
        workspace.set_plan_path(Some(plan.clone()));
        let requested = plan.to_string_lossy().into_owned();
        std::fs::write(workspace.target(&requested).unwrap(), "# the plan").unwrap();
        // The Build-mode turn installs nothing; the earlier set must hold.
        assert_eq!(workspace.file(&requested).unwrap(), plan);
    }
}

#[cfg(test)]
mod truncate_output_tests {
    use super::{MAX_OUTPUT_BYTES, truncate_output};

    #[test]
    fn respects_cap_boundary_and_marker() {
        assert_eq!(truncate_output("hello"), "hello");
        let long = "ä".repeat(MAX_OUTPUT_BYTES); // 2 bytes per char, cap lands mid-char
        let truncated = truncate_output(&long);
        assert!(truncated.len() < long.len());
        assert!(truncated.ends_with("\n... [output truncated]"));
        let clipped = truncated.trim_end_matches("\n... [output truncated]");
        assert!(clipped.len() <= MAX_OUTPUT_BYTES);
    }
}
