//! Workspace-scoped filesystem tools for Rig.
//!
//! These checks prevent accidental escapes, not hostile filesystem races. The
//! caller must supply a trusted workspace and apply any approval/sandbox policy.

mod delete;
mod edit;
mod grep;
mod read;

#[cfg(test)]
mod tests;

pub use delete::{Delete, DeleteArgs, DeleteOutput};
pub use edit::{Edit, EditArgs, EditOutput};
pub use grep::{Grep, GrepArgs, GrepMatch, GrepOutput};
pub use read::{Read, ReadArgs, ReadLine, ReadOutput};

use std::{
    fs::{self, File},
    io::{self, Read as _},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use rig::{
    agent::{AgentBuilder, WithBuilderTools},
    tool::{ToolErrorKind, ToolExecutionError},
};

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
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = fs::canonicalize(root).map_err(io_error)?;
        if !root.is_dir() {
            return Err(invalid("workspace root must be a directory"));
        }
        if root.components().any(is_git_component) {
            return Err(denied("workspace must not be inside Git metadata"));
        }
        Ok(Self {
            root: Arc::new(root),
            lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn register(&self, builder: AgentBuilder) -> AgentBuilder<WithBuilderTools> {
        builder
            .tool(Read(self.clone()))
            .tool(Grep(self.clone()))
            .tool(Edit(self.clone()))
            .tool(Delete(self.clone()))
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

    /// Existing paths only. Reject `..`, Git metadata, and every symlink
    /// component, including links whose targets are inside the workspace.
    pub(crate) fn resolve(&self, requested: &str) -> Result<PathBuf> {
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
        let mut path = self.root().to_path_buf();
        for component in relative.components() {
            match component {
                Component::CurDir => continue,
                Component::Normal(_) if is_git_component(component) => {
                    return Err(denied("access to Git metadata is not allowed"));
                }
                Component::Normal(name) => path.push(name),
                _ => {
                    return Err(denied(
                        "paths must remain inside the workspace; '..' is not allowed",
                    ));
                }
            }
            if fs::symlink_metadata(&path)
                .map_err(io_error)?
                .file_type()
                .is_symlink()
            {
                return Err(denied("symlink paths are not supported"));
            }
        }
        Ok(path)
    }

    pub(crate) fn file(&self, requested: &str) -> Result<PathBuf> {
        let path = self.resolve(requested)?;
        if !fs::symlink_metadata(&path).map_err(io_error)?.is_file() {
            return Err(invalid("path must identify a regular file"));
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

fn is_git_component(component: Component<'_>) -> bool {
    matches!(component, Component::Normal(name) if name.to_string_lossy().eq_ignore_ascii_case(".git"))
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

// Keep typed argument schemas and Rig's tool boundary identical for each tool.
macro_rules! impl_tool {
    ($tool:ident, $args:ty, $output:ty, $name:literal, $description:literal) => {
        impl rig::tool::Tool for $tool {
            const NAME: &'static str = $name;
            type Args = $args;
            type Output = $output;
            type Error = rig::tool::ToolExecutionError;

            fn description(&self) -> String {
                $description.into()
            }

            fn parameters(&self) -> serde_json::Value {
                serde_json::to_value(schemars::schema_for!($args))
                    .expect("JSON Schema is serializable")
            }

            fn map_error(&self, error: Self::Error) -> rig::tool::ToolExecutionError {
                error
            }

            async fn call(
                &self,
                _: &mut rig::tool::ToolContext,
                args: Self::Args,
            ) -> super::Result<Self::Output> {
                self.0
                    .run(move |workspace| Self::execute(workspace, args))
                    .await
            }
        }
    };
}
pub(crate) use impl_tool;
