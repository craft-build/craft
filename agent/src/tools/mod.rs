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
mod inspect;
mod list;
mod list_tools;
pub(crate) mod move_file;
mod multiedit;
mod read;
mod retrieve;
mod todo_write;
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
pub use inspect::{Inspect, InspectArgs, InspectOutput};
pub use list::{List, ListArgs, ListOutput};
pub use list_tools::{ListTools, ListToolsArgs, ListToolsOutput};
pub use move_file::{MoveFile, MoveFileArgs, MoveFileOutput};
pub use multiedit::{EditEntry, MultiEdit, MultiEditArgs, MultiEditOutput};
pub use read::{Read, ReadArgs, ReadLine, ReadOutput};
pub use retrieve::{Retrieve, RetrieveArgs, RetrieveOutput};
pub use todo_write::{Todo, TodoWrite, TodoWriteArgs, TodoWriteOutput};
pub use write::{Write, WriteArgs, WriteOutput};

use std::{
    fs::{self, File},
    io::{self, Read as _},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use rig_core::tool::{PortableDynamicTool, PortableTool, ToolErrorKind, ToolExecutionError};

pub(crate) type Result<T> = std::result::Result<T, ToolExecutionError>;
pub(crate) const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_LINE_BYTES: usize = 2048;

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
        })
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

    /// The session-shared reversible-compression store: request-build
    /// markers and the `retrieve` tool both use this instance.
    pub fn compression_store(&self) -> &crate::compression::store::SharedCompressionStore {
        &self.compression_store
    }

    /// The session-shared snapshot manager backing `/undo`.
    pub fn snapshots(&self) -> &crate::snapshot::SnapshotManager {
        &self.snapshots
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
        let batch = Batch(std::sync::Arc::new(std::sync::OnceLock::new()));
        let mut tools: Vec<PortableDynamicTool> = vec![
            dynamic(Read(self.clone())),
            dynamic(Grep(self.clone())),
            dynamic(Glob(self.clone())),
            dynamic(List(self.clone())),
            dynamic(Edit(self.clone())),
            dynamic(EditLines(self.clone())),
            dynamic(InsertLines(self.clone())),
            dynamic(MultiEdit(self.clone())),
            dynamic(ApplyPatch(self.clone())),
            dynamic(Write(self.clone())),
            dynamic(Delete(self.clone())),
            dynamic(MoveFile(self.clone())),
            dynamic(Bash(self.clone())),
            dynamic(BashStatus(self.clone())),
            dynamic(BashWatch(self.clone())),
            dynamic(BashKill(self.clone())),
            dynamic(Inspect(self.clone())),
            dynamic(TodoWrite(self.clone())),
            dynamic(Retrieve(self.compression_store.clone())),
            dynamic(batch.clone()),
        ];
        // Introspection snapshot of every other registered tool.
        let definitions = tools.iter().map(PortableDynamicTool::definition).collect();
        tools.push(dynamic(ListTools(Arc::new(definitions))));
        let dispatch = crate::run::ToolDispatch::new(tools)
            .with_compression_store(self.compression_store.clone())
            .with_snapshots(self.snapshots.clone());
        let _ = batch.0.set(dispatch.clone());
        dispatch
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
        let requested = Path::new(requested);
        let relative = if requested.is_absolute() {
            requested
                .strip_prefix(self.root())
                .map_err(|_| denied("path is outside the workspace"))?
        } else {
            requested
        };
        let mut components = relative.components().peekable();
        let mut path = self.root().to_path_buf();
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

pub(crate) fn invalid(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::new(ToolErrorKind::InvalidArgs, message)
}

pub(crate) fn denied(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::refused(message)
}

pub(crate) fn failure(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::new(ToolErrorKind::Other, message)
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
                serde_json::to_value(schemars::schema_for!($args))
                    .expect("JSON Schema is serializable")
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
