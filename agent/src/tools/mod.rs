//! Workspace-scoped filesystem tools for Rig.
//!
//! These checks prevent accidental escapes, not hostile filesystem races. The
//! caller must supply a trusted workspace and apply any approval/sandbox policy.
//!
//! Output types implement `IntoToolOutput` rather than `Serialize`: Rig must send
//! their readable text to both the model and ACP, not serialize the internal fields.

pub(crate) mod apply_patch;
pub(crate) mod bash;
mod batch;
mod delete;
mod edit;
pub(crate) mod fuzzy_replace;
mod glob;
mod grep;
mod inplace_edit;
mod list;
mod list_tools;
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
pub use batch::{Batch, BatchArgs, BatchOutput, SharedDispatch};
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

use std::{
    fs::{self, File},
    io::{self, Read as _},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use rig_core::completion::ToolDefinition;
use rig_core::completion::message::{ImageMediaType, ToolResultContent as RigToolResultContent};
use rig_core::tool::{
    PortableDynamicTool, PortableTool, ToolErrorKind, ToolExecutionError, ToolOutput,
};

pub(crate) type Result<T> = std::result::Result<T, ToolExecutionError>;
pub(crate) const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_LINE_BYTES: usize = 2048;

/// Every tool name registered by [`Workspace::register`]. MCP server names are
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

/// One fixed root shared by all tools in an agent. Calls are serialized on a
/// blocking worker, so filesystem I/O does not block Tokio's executor and two
/// edits from the same tool set cannot overwrite one another.
#[derive(Clone)]
pub struct Workspace {
    root: Arc<PathBuf>,
    lock: Arc<Mutex<()>>,
    loaded_instructions: crate::instructions::LoadedInstructions,
    todos: todo_write::TodoStore,
    compression_store: crate::compression::store::SharedCompressionStore,
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
    plan_path: std::sync::Arc<std::sync::RwLock<Option<PathBuf>>>,
    /// MCP client handle (B.11): when set, `register_with_mode` appends one
    /// portable tool per published MCP tool under its `server__tool` wire name.
    /// Shared like every other cell so the per-turn set is visible through clones.
    mcp: std::sync::Arc<std::sync::RwLock<Option<crate::mcp::McpHandle>>>,
    /// Host seam for the `task` tool (A.5): the TUI and headless surfaces
    /// install a subagent launcher per turn; the default reports that
    /// subagents are unavailable.
    subagents: Arc<dyn task::SpawnSubagent>,
    /// Session-wide before-execute hook (the approval gate). Applied inside
    /// every dispatch-table registration, so the batch fan-out's snapshot of
    /// the table — taken during registration, before any post-register
    /// `ToolDispatch::with_before` could reach it — consults the gate too.
    before: Option<Arc<dyn crate::run::dispatch::BeforeExecute>>,
    /// Phase 6: the turn's cancellation token, installed per turn like the
    /// question seam. MCP tool calls race it so a cancelled turn tells the
    /// server to stop. Shared cell so every clone (batch, subagents) sees
    /// the per-turn set; `None` before a turn installs one.
    mcp_cancel: std::sync::Arc<std::sync::RwLock<Option<crate::run::CancelToken>>>,
    /// Session sandbox policy (resolved from config/env at construction or
    /// installed by the surface). Turn registrations snapshot or narrow it
    /// into a frozen per-turn cell, like every other per-turn decision.
    sandbox: crate::sandbox::SandboxPolicyCell,
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = fs::canonicalize(root).map_err(io_error)?;
        if !root.is_dir() {
            return Err(invalid("workspace root must be a directory"));
        }
        if root.components().any(|c| is_git_component(c.as_os_str())) {
            return Err(denied("workspace must not be inside Git metadata"));
        }
        Ok(Self {
            root: Arc::new(root.clone()),
            lock: Arc::new(Mutex::new(())),
            loaded_instructions: crate::instructions::LoadedInstructions::new(),
            todos: Default::default(),
            compression_store: crate::compression::store::shared_store(),
            snapshots: crate::snapshot::SnapshotManager::new(root.clone()),
            bash_jobs: Default::default(),
            state_dir: crate::storage::StateDir::resolve().ok(),
            questions: Arc::new(question::DismissAsk),
            plan_path: std::sync::Arc::new(std::sync::RwLock::new(None)),
            mcp: std::sync::Arc::new(std::sync::RwLock::new(None)),
            subagents: Arc::new(task::NoSubagents),
            mcp_cancel: Default::default(),
            before: None,
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

    /// Install the session's before-execute hook (approval gate). See the
    /// `before` field: registering through a workspace that carries the
    /// hook is what gates batch children, not a post-register attach.
    pub fn with_before(mut self, hook: Arc<dyn crate::run::dispatch::BeforeExecute>) -> Self {
        self.before = Some(hook);
        self
    }

    /// Install the subagent seam (A.5). Taken on a workspace clone per turn
    /// (like the question asker) so the launcher can carry the turn's cancel
    /// token, history snapshot, and event channel.
    pub fn with_subagents(mut self, spawn: Arc<dyn task::SpawnSubagent>) -> Self {
        self.subagents = spawn;
        self
    }

    /// Install the session's MCP client (B.11). Call before the first turn
    /// registers tools, after awaiting `McpHandle::ready` where the caller
    /// must not ship a prompt without the MCP tools.
    pub fn set_mcp(&self, handle: Option<crate::mcp::McpHandle>) {
        let mut cell = self.mcp.write().expect("mcp cell poisoned");
        *cell = handle;
    }

    pub fn mcp(&self) -> Option<crate::mcp::McpHandle> {
        self.mcp.read().expect("mcp cell poisoned").clone()
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

    /// The per-turn sandbox cell baked into a tool table. Plan mode and
    /// read-only workers get a frozen read-only policy (plan mode keeps the
    /// allocated plan file writable); build/general turns share the session
    /// cell so later policy installs are visible.
    fn turn_sandbox_cell(
        &self,
        mode: &crate::run::AgentMode,
        force_read_only: bool,
    ) -> crate::sandbox::SandboxPolicyCell {
        let read_only = force_read_only || matches!(mode, crate::run::AgentMode::Plan(_));
        if !read_only {
            return self.sandbox.clone();
        }
        let mut policy = self.sandbox_policy();
        policy.mode = crate::sandbox::SandboxMode::ReadOnly;
        policy.writable_roots = mode
            .plan_path()
            .map(|p| vec![p.to_path_buf()])
            .unwrap_or_default();
        std::sync::Arc::new(std::sync::RwLock::new(crate::sandbox::SandboxState {
            policy,
            note_shown: false,
        }))
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

    /// Set (or clear) the session's plan file (C.17). The plan file lives
    /// in the state dir, outside the workspace root; `walk` exempts exactly
    /// this path from containment. Install when a plan is allocated and
    /// leave it for the rest of the session — Build-mode turns that
    /// implement the plan must keep the exception.
    pub fn set_plan_path(&self, path: Option<PathBuf>) {
        *self.plan_path.write().unwrap_or_else(|e| e.into_inner()) = path;
    }

    fn plan_target(&self, requested: &str) -> bool {
        let guard = self.plan_path.read().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().is_some_and(|plan| {
            crate::run::dedup::normalize_write_path(requested)
                == crate::run::dedup::normalize_write_path(&plan.display().to_string())
        })
    }

    /// Read-only exemption for the state dir's plans directory
    /// (`<state>/plans`): the agent may read the plan files it holds —
    /// including plans from earlier sessions — but never write there.
    /// Only the exact session plan file ([`Self::plan_target`]) is
    /// writable outside the workspace.
    fn plans_read_target(&self, requested: &str) -> bool {
        let normalize = crate::run::dedup::normalize_write_path;
        self.state_dir.as_ref().is_some_and(|dir| {
            let plans = normalize(&dir.path().join("plans").display().to_string());
            Path::new(&normalize(requested)).starts_with(&plans)
        })
    }

    /// Install the question seam (A.5). Taken on a workspace clone per
    /// turn (like the approval gate) so the asker can carry the turn's
    /// cancel token and event channel.
    pub fn with_questions(mut self, asker: Arc<dyn question::AskQuestions>) -> Self {
        self.questions = asker;
        self
    }

    /// Install the turn's cancellation token (Phase 6): taken on the same
    /// per-turn clone as the question/subagent seams, so MCP tool calls
    /// registered from it race the turn's cancel.
    pub fn with_cancel(self, cancel: crate::run::CancelToken) -> Self {
        *self.mcp_cancel.write().expect("mcp cancel cell poisoned") = Some(cancel);
        self
    }

    /// The session-shared reversible-compression store: request-build
    /// markers and the `retrieve` tool both use this instance.
    pub fn compression_store(&self) -> &crate::compression::store::SharedCompressionStore {
        &self.compression_store
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
    pub(crate) fn note_snapshot(&self, path: &Path) {
        self.snapshots.note(path);
    }

    /// Register the workspace's tools into our dispatch executor. The batch
    /// tool joins the table before the `list_tools` snapshot is taken, then
    /// receives the finished table through its shared slot so its children
    /// flow through the same interception pipeline.
    pub fn register(&self) -> crate::run::ToolDispatch {
        self.register_with_mode(crate::run::AgentMode::Build)
    }

    /// One constructor row per tool: the single shared table behind both
    /// [`Self::register_with_mode`] and [`Self::register_subagent`], so the
    /// two registrations cannot drift — a wire name absent here is simply
    /// never registered, never rescued by a wildcard arm. The argosy
    /// knowledge tools register as first-class natives under their own
    /// names; both surfaces see them.
    fn builtin_tool_table(
        &self,
        sandbox: crate::sandbox::SandboxPolicyCell,
    ) -> Vec<(&'static str, PortableDynamicTool)> {
        let mut table = self.core_builtin_table(sandbox);
        table.extend(argosy_tools());
        table
    }

    fn core_builtin_table(
        &self,
        sandbox: crate::sandbox::SandboxPolicyCell,
    ) -> Vec<(&'static str, PortableDynamicTool)> {
        vec![
            ("read", dynamic(Read(self.clone()))),
            ("grep", dynamic(Grep(self.clone()))),
            ("glob", dynamic(Glob(self.clone()))),
            ("list", dynamic(List(self.clone()))),
            ("edit", dynamic(Edit(self.clone()))),
            ("edit_lines", dynamic(EditLines(self.clone()))),
            ("insert_lines", dynamic(InsertLines(self.clone()))),
            ("multiedit", dynamic(MultiEdit(self.clone()))),
            ("apply_patch", dynamic(ApplyPatch(self.clone()))),
            ("write", dynamic(Write(self.clone()))),
            ("delete", dynamic(Delete(self.clone()))),
            ("move_file", dynamic(MoveFile(self.clone()))),
            ("bash", dynamic(Bash(self.clone(), sandbox))),
            ("bash_status", dynamic(BashStatus(self.clone()))),
            ("bash_watch", dynamic(BashWatch(self.clone()))),
            ("bash_kill", dynamic(BashKill(self.clone()))),
            ("todo_write", dynamic(TodoWrite(self.clone()))),
            (
                "retrieve",
                dynamic(Retrieve(self.compression_store.clone())),
            ),
            ("skill", dynamic(Skill::new(self.root().to_path_buf()))),
            (
                "sessions",
                dynamic(Sessions::new(
                    self.root().display().to_string(),
                    self.state_dir.clone(),
                )),
            ),
            ("webfetch", dynamic(Webfetch)),
            ("websearch", dynamic(Websearch)),
            ("view_image", dynamic(ViewImage(self.clone()))),
            ("question", dynamic(Question(self.questions.clone()))),
            ("task", dynamic(Task(self.subagents.clone()))),
            ("review", dynamic(Review(self.subagents.clone()))),
        ]
    }

    /// [`Self::register`] with the turn's mode baked into the table — and
    /// into the batch child table — so write-gating is a frozen snapshot for
    /// the whole turn.
    pub fn register_with_mode(&self, mode: crate::run::AgentMode) -> crate::run::ToolDispatch {
        let batch = Batch(std::sync::Arc::new(std::sync::OnceLock::new()));
        let mut tools = self.mode_tool_set(&mode, batch.clone());
        let definitions = tools.iter().map(PortableDynamicTool::definition).collect();
        tools.push(dynamic(ListTools(Arc::new(definitions))));
        let dispatch = crate::run::ToolDispatch::new(tools)
            .with_write_root((*self.root).clone())
            .with_compression_store(self.compression_store.clone())
            .with_snapshots(self.snapshots.clone())
            .with_mode(mode);
        // The gate must be in the table before the batch snapshot below.
        let dispatch = match &self.before {
            Some(hook) => dispatch.with_before(Arc::clone(hook)),
            None => dispatch,
        };
        let _ = batch.0.set(dispatch.clone());
        dispatch
    }

    /// The provider-facing tool definitions a turn in `mode` would register
    /// (including MCP tools and the self-describing `list_tools` entry):
    /// exactly what [`Self::register_with_mode`] sends, so surfaces
    /// computing request overhead cannot drift from the run's own table.
    pub fn tool_definitions(&self, mode: &crate::run::AgentMode) -> Vec<ToolDefinition> {
        let mut tools =
            self.mode_tool_set(mode, Batch(std::sync::Arc::new(std::sync::OnceLock::new())));
        let mut definitions: Vec<ToolDefinition> =
            tools.iter().map(PortableDynamicTool::definition).collect();
        tools.push(dynamic(ListTools(Arc::new(definitions.clone()))));
        definitions.push(tools.last().expect("list_tools just pushed").definition());
        definitions
    }

    /// The mode's full tool set (builtins + batch + MCP), before the
    /// self-describing `list_tools` entry is appended at registration.
    fn mode_tool_set(
        &self,
        mode: &crate::run::AgentMode,
        batch: Batch,
    ) -> Vec<PortableDynamicTool> {
        let sandbox = self.turn_sandbox_cell(mode, false);
        let mut tools: Vec<PortableDynamicTool> = self
            .builtin_tool_table(sandbox)
            .into_iter()
            .map(|(_, tool)| tool)
            .collect();
        tools.push(dynamic(batch));
        if let Some(handle) = self.mcp() {
            let cancel = self
                .mcp_cancel
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            tools.extend(mcp_tools(&handle, cancel));
        }
        tools
    }

    /// The restricted tool table for a `task`-spawned subagent (A.5):
    /// `general` adds the write family to the read-only research set;
    /// neither includes `task`, `question`, or `sessions`. MCP tools are
    /// inherited. The batch child table is built from the same restricted
    /// set, so batch fan-outs cannot escape the subagent's tool budget.
    pub fn register_subagent(&self, general: bool) -> crate::run::ToolDispatch {
        let allowed: &[&str] = if general {
            crate::subagent::GENERAL_TOOLS
        } else {
            crate::subagent::RESEARCH_TOOLS
        };
        let batch = Batch(std::sync::Arc::new(std::sync::OnceLock::new()));
        // Read-only workers never inherit workspace-write, even though their
        // budget has no bash today: the shared table must not leak it to
        // future additions.
        let sandbox = self.turn_sandbox_cell(&crate::run::AgentMode::Build, !general);
        // The restricted table is the shared builtin table filtered by the
        // subagent tool budget.
        let mut tools: Vec<PortableDynamicTool> = self
            .builtin_tool_table(sandbox)
            .into_iter()
            .filter(|(name, _)| allowed.contains(name))
            .map(|(_, tool)| tool)
            .collect();
        if allowed.contains(&"batch") {
            tools.push(dynamic(batch.clone()));
        }
        if let Some(handle) = self.mcp() {
            let cancel = self
                .mcp_cancel
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            tools.extend(mcp_tools(&handle, cancel));
        }
        let definitions = tools.iter().map(PortableDynamicTool::definition).collect();
        tools.push(dynamic(ListTools(Arc::new(definitions))));
        let dispatch = crate::run::ToolDispatch::new(tools)
            .with_write_root((*self.root).clone())
            .with_compression_store(self.compression_store.clone())
            .with_snapshots(self.snapshots.clone())
            .with_mode(crate::run::AgentMode::Build);
        // Same as above: the subagent's own batch fan-out must see the gate.
        let dispatch = match &self.before {
            Some(hook) => dispatch.with_before(Arc::clone(hook)),
            None => dispatch,
        };
        let _ = batch.0.set(dispatch.clone());
        dispatch
    }

    /// The reviewer subagent's table (Phase 5 of the argosy integration):
    /// the read-only research set plus the argosy review workflow, filtered
    /// from the same shared builtin table. Plan mode: the reviewer never
    /// writes.
    pub fn register_reviewer(&self) -> crate::run::ToolDispatch {
        let allowed = crate::subagent::REVIEWER_TOOLS;
        let sandbox = self.turn_sandbox_cell(&crate::run::AgentMode::Build, true);
        let mut tools: Vec<PortableDynamicTool> = self
            .builtin_tool_table(sandbox)
            .into_iter()
            .filter(|(name, _)| allowed.contains(name))
            .map(|(_, tool)| tool)
            .collect();
        let definitions = tools.iter().map(PortableDynamicTool::definition).collect();
        tools.push(dynamic(ListTools(Arc::new(definitions))));
        crate::run::ToolDispatch::new(tools)
            .with_write_root((*self.root).clone())
            .with_compression_store(self.compression_store.clone())
            .with_snapshots(self.snapshots.clone())
            // Build mode is safe here: the reviewer's budget contains no
            // write tools at all.
            .with_mode(crate::run::AgentMode::Build)
    }

    pub(crate) async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Self) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let workspace = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = workspace
                .lock
                .lock()
                .map_err(|_| failure("workspace tool lock was poisoned"))?;
            operation(&workspace)
        })
        .await
        .map_err(|error| failure(format!("filesystem task failed: {error}")))?
    }

    /// Shared validation walk for the read and write paths: strips the
    /// workspace prefix from absolute requests, then walks component by
    /// component rejecting `..`, Git metadata, and symlinked components.
    /// With `create_dirs` (write path), missing directory components are
    /// created and the final component is returned as the file name instead
    /// of being appended.
    fn walk<'a>(
        &self,
        requested: &'a str,
        create_dirs: bool,
    ) -> Result<(PathBuf, Option<Component<'a>>)> {
        if requested.is_empty() {
            return Err(invalid("path must not be empty"));
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
                    .map_err(|_| denied("path is outside the workspace"))?,
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
                    _ => Err(denied(
                        "paths must remain inside the workspace; '..', '.', and Git metadata are not allowed",
                    )),
                };
            }
            match component {
                Component::CurDir => continue,
                Component::Normal(name) if is_git_component(name) => {
                    return Err(denied("access to Git metadata is not allowed"));
                }
                // Exempt targets are absolute paths outside the workspace:
                // anchor the walk at the path's own root/prefix.
                Component::RootDir | Component::Prefix(_) if exempt => {
                    path = component.as_os_str().into();
                    continue;
                }
                Component::Normal(_) => path.push(component),
                _ => {
                    return Err(denied(
                        "paths must remain inside the workspace; '..' is not allowed",
                    ));
                }
            }
            if create_dirs
                && let Err(error) = fs::create_dir(&path)
                && error.kind() != io::ErrorKind::AlreadyExists
            {
                return Err(io_error(error));
            }
            if fs::symlink_metadata(&path)
                .map_err(io_error)?
                .file_type()
                .is_symlink()
            {
                return Err(denied("symlink paths are not supported"));
            }
        }
        Ok((path, None))
    }

    /// Existing paths only. Reject `..`, Git metadata, and every symlink
    /// component, including links whose targets are inside the workspace.
    pub(crate) fn resolve(&self, requested: &str) -> Result<PathBuf> {
        self.walk(requested, false).map(|(path, _)| path)
    }

    pub(crate) fn file(&self, requested: &str) -> Result<PathBuf> {
        let path = self.resolve(requested)?;
        if !fs::symlink_metadata(&path).map_err(io_error)?.is_file() {
            return Err(invalid("path must identify a regular file"));
        }
        Ok(path)
    }

    /// Resolution for writes: like [`Self::resolve`], but the final component
    /// may not exist yet, missing parent directories are created, and an
    /// existing destination must be a regular file (never a symlink).
    pub(crate) fn target(&self, requested: &str) -> Result<PathBuf> {
        let (mut path, name) = self.walk(requested, true)?;
        let Some(component) = name else {
            return Err(invalid("path must name a file, not a directory"));
        };
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return Err(invalid("path must identify a regular file")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
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

fn is_git_component(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().eq_ignore_ascii_case(".git")
}

pub(crate) fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(invalid("path must identify a regular file"));
    }
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(invalid("file exceeds the 8 MiB tool size limit"));
    }
    Ok(bytes)
}

pub(crate) fn text(bytes: Vec<u8>) -> Result<String> {
    if bytes.contains(&0) {
        return Err(invalid("binary files are not supported by this text tool"));
    }
    String::from_utf8(bytes).map_err(|_| invalid("file is not valid UTF-8 text"))
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

pub(crate) fn invalid(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::new(ToolErrorKind::InvalidArgs, message)
}

pub(crate) fn denied(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::refused(message)
}

pub(crate) fn failure(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::new(ToolErrorKind::Other, message)
}

pub(crate) fn not_found(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::new(ToolErrorKind::NotFound, message)
}

pub(crate) fn io_error(error: io::Error) -> ToolExecutionError {
    let kind = match error.kind() {
        io::ErrorKind::NotFound => ToolErrorKind::NotFound,
        io::ErrorKind::PermissionDenied => ToolErrorKind::PermissionDenied,
        _ => ToolErrorKind::Other,
    };
    ToolExecutionError::new(kind, error.to_string())
}

// Keep typed argument schemas and a portable, context-free tool boundary for
// each tool. `dynamic` adapts them into the erased executor's tool set.
macro_rules! impl_tool {
    ($tool:ident, $args:ty, $output:ty, $name:literal, $description:literal) => {
        impl rig_core::tool::PortableTool for $tool {
            const NAME: &'static str = $name;
            type Args = $args;
            type Output = $output;
            type Error = rig_core::tool::ToolExecutionError;

            fn description(&self) -> String {
                $description.into()
            }

            fn parameters(&self) -> serde_json::Value {
                serde_json::to_value(schemars::schema_for!($args)).unwrap_or_else(|error| {
                    tracing::error!(
                        tool = $name,
                        %error,
                        "tool schema serialization failed; serving a bare object schema"
                    );
                    serde_json::json!({"type": "object"})
                })
            }

            async fn call(&self, args: Self::Args) -> super::Result<Self::Output> {
                self.0
                    .run(move |workspace| Self::execute(workspace, args))
                    .await
            }
        }
    };
}
pub(crate) use impl_tool;

/// The argosy knowledge tools (Phase 1 of the argosy integration): one
/// portable tool per argosy definition under its native name, calling the
/// in-process service on the blocking pool. The report renders to text via
/// [`crate::knowledge::render_report_text`] — embedded bodies (`content`,
/// pre-rendered `text`, `drafted`) surface directly instead of as pretty
/// JSON.
fn argosy_tools() -> Vec<(&'static str, PortableDynamicTool)> {
    crate::knowledge::tool_definitions()
        .iter()
        .map(|def| {
            let name = def.name.clone();
            (
                def.native,
                PortableDynamicTool::new(
                    def.native,
                    def.description.clone(),
                    def.schema.clone(),
                    move |arguments| {
                        let name = name.clone();
                        Box::pin(async move {
                            match crate::knowledge::call_tool(name.clone(), arguments).await {
                                Ok(report) => Ok(ToolOutput::text(
                                    crate::knowledge::render_report_text(&name, &report),
                                )),
                                Err(e) => Err(failure(e)),
                            }
                        })
                    },
                ),
            )
        })
        .collect()
}

/// One portable tool per published MCP tool (B.11). The model sees the
/// `server__tool` wire name; the closure resolves it back to the qualified
/// name the manager's tool index is keyed by.
fn mcp_tools(
    handle: &crate::mcp::McpHandle,
    cancel: Option<crate::run::CancelToken>,
) -> Vec<PortableDynamicTool> {
    handle
        .tool_descriptors()
        .into_iter()
        .map(|descriptor| {
            let handle = handle.clone();
            let qualified = descriptor.qualified_name.clone();
            let cancel = cancel.clone();
            PortableDynamicTool::new(
                descriptor.wire_name,
                descriptor.description,
                descriptor.parameters,
                move |arguments| {
                    let handle = handle.clone();
                    let qualified = qualified.clone();
                    let cancel = cancel.clone();
                    Box::pin(async move {
                        // Phase 6: race the turn's cancel so a cancelled turn
                        // stops the in-flight server call instead of parking
                        // until it finishes or times out.
                        let outcome = match &cancel {
                            Some(token) => {
                                handle
                                    .call_tool_cancellable(&qualified, &arguments, token)
                                    .await
                            }
                            None => handle.call_tool(&qualified, &arguments).await,
                        };
                        match outcome {
                            Ok(output) => Ok(mcp_tool_output(output)),
                            Err(e) => Err(failure(e.to_string())),
                        }
                    })
                },
            )
        })
        .collect()
}

/// Map an MCP tool's output into a tool result, preserving the server's
/// block order: text parts as text, images as vision content parts (same
/// shape `view_image` emits) interleaved where they appeared.
fn mcp_tool_output(output: crate::mcp::McpToolOutput) -> ToolOutput {
    use crate::mcp::session::McpPart;

    if !output.has_images() {
        return ToolOutput::text(output.joined_text());
    }
    let mut content = Vec::new();
    for part in output.parts {
        match part {
            McpPart::Text(text) => content.push(RigToolResultContent::text(text)),
            McpPart::Image(image) => match image_media_type(&image.mime) {
                Some(media) => content.push(RigToolResultContent::image_base64(
                    image.data,
                    Some(media),
                    None,
                )),
                // Rig only carries known media types; note the drop instead of
                // losing the image silently.
                None => content.push(RigToolResultContent::text(format!(
                    "[unsupported image mime: {}]",
                    image.mime
                ))),
            },
        }
    }
    let text = content
        .iter()
        .filter_map(|c| match c {
            RigToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    ToolOutput::content(content).unwrap_or_else(|_| ToolOutput::text(text))
}

fn image_media_type(mime: &str) -> Option<ImageMediaType> {
    match mime.to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => Some(ImageMediaType::JPEG),
        "image/png" => Some(ImageMediaType::PNG),
        "image/gif" => Some(ImageMediaType::GIF),
        "image/webp" => Some(ImageMediaType::WEBP),
        "image/heic" => Some(ImageMediaType::HEIC),
        "image/heif" => Some(ImageMediaType::HEIF),
        "image/svg+xml" => Some(ImageMediaType::SVG),
        _ => None,
    }
}

/// Adapt a typed portable tool into the erased tool the dispatcher executes.
fn dynamic<T>(tool: T) -> PortableDynamicTool
where
    T: PortableTool + Clone + Send + Sync + 'static,
    T::Args: serde::de::DeserializeOwned + Send + Sync + 'static,
    T::Output: rig_core::tool::IntoToolOutput,
{
    PortableDynamicTool::new(
        T::NAME,
        tool.description(),
        tool.parameters(),
        move |arguments| {
            let tool = tool.clone();
            Box::pin(async move {
                let args = serde_json::from_value(arguments)
                    .map_err(|error| invalid(format!("invalid arguments: {error}")))?;
                match tool.call(args).await {
                    Ok(output) => rig_core::tool::IntoToolOutput::into_tool_output(output),
                    Err(error) => Err(tool.map_error(error)),
                }
            })
        },
    )
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

    #[test]
    fn plan_file_write_outside_workspace_still_works() {
        let (_dir, _state, workspace, plan) = plan_workspace();
        workspace.set_plan_path(Some(plan.clone()));
        let requested = plan.to_string_lossy().into_owned();
        let target = workspace.target(&requested).unwrap();
        assert_eq!(target, plan);
        fs::write(&target, "the plan").unwrap();
        assert_eq!(workspace.resolve(&requested).unwrap(), plan);
        assert_eq!(fs::read_to_string(&plan).unwrap(), "the plan");
    }

    #[test]
    fn plan_file_write_creates_missing_parent_dirs() {
        let (_dir, _state, workspace, plan) = plan_workspace();
        let nested = plan.parent().unwrap().join("nested/deep/PLAN.md");
        workspace.set_plan_path(Some(nested.clone()));
        let target = workspace.target(&nested.to_string_lossy()).unwrap();
        assert_eq!(target, nested);
        fs::write(&target, "plan").unwrap();
        assert_eq!(fs::read_to_string(&nested).unwrap(), "plan");
    }

    #[test]
    fn plan_path_with_parent_component_is_rejected() {
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
    #[test]
    fn plan_path_through_symlinked_directory_is_rejected() {
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

    #[test]
    fn plans_dir_reads_outside_workspace_are_exempt() {
        let (_root, _state, state, workspace) = plans_workspace();
        let earlier = state.join("plans/old-plan.md");
        fs::create_dir_all(earlier.parent().unwrap()).unwrap();
        fs::write(&earlier, "# earlier session's plan").unwrap();
        let requested = earlier.to_string_lossy().into_owned();
        assert_eq!(workspace.resolve(&requested).unwrap(), earlier);
        assert_eq!(workspace.file(&requested).unwrap(), earlier);
    }

    #[test]
    fn plans_dir_writes_outside_workspace_are_refused() {
        let (_root, _state, state, workspace) = plans_workspace();
        let earlier = state.join("plans/old-plan.md");
        fs::create_dir_all(earlier.parent().unwrap()).unwrap();
        let requested = earlier.to_string_lossy().into_owned();
        assert!(workspace.target(&requested).is_err());
    }

    #[test]
    fn state_dir_paths_outside_plans_are_still_refused() {
        let (_root, _state, state, workspace) = plans_workspace();
        let sessions = state.join("sessions/session.json");
        let requested = sessions.to_string_lossy().into_owned();
        assert!(workspace.resolve(&requested).is_err());
        assert!(workspace.target(&requested).is_err());
    }

    /// The implement flow: after clear-context-and-implement the turn runs
    /// in Build mode, which installs no plan path — the session's plan file
    /// must keep its exemption so the agent can read (and update) it.
    #[test]
    fn plan_file_exemption_survives_the_build_mode_implement_turn() {
        let (_dir, _state, workspace, plan) = plan_workspace();
        workspace.set_plan_path(Some(plan.clone()));
        let requested = plan.to_string_lossy().into_owned();
        fs::write(workspace.target(&requested).unwrap(), "# the plan").unwrap();
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
