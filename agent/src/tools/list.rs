use rig_core::tool::{IntoToolOutput, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Result, Workspace, impl_tool, invalid};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListArgs {
    /// Directory to list, relative to the workspace.
    #[serde(default = "default_path")]
    pub path: String,
}

fn default_path() -> String {
    ".".into()
}

#[derive(Debug)]
pub struct ListOutput {
    /// Entry names, directories first with a trailing `/`.
    pub entries: Vec<String>,
    /// Subdirectory instruction files discovered for this directory, as
    /// `(canonical_path, content)` pairs.
    pub instructions: Vec<(String, String)>,
}

impl IntoToolOutput for ListOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        let mut text = if self.entries.is_empty() {
            "(empty directory)".to_string()
        } else {
            self.entries.join("\n")
        };
        for (path, content) in &self.instructions {
            text.push_str(&format!("\n\n---\nInstructions from: {path}\n{content}"));
        }
        Ok(ToolOutput::text(text))
    }
}

#[derive(Clone)]
pub struct List(pub Workspace);

impl List {
    fn execute(workspace: &Workspace, args: ListArgs) -> Result<ListOutput> {
        let path = workspace.resolve(&args.path)?;
        if !path.is_dir() {
            return Err(invalid("path must identify a directory"));
        }
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&path).map_err(super::io_error)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                dirs.push(format!("{name}/"));
            } else {
                files.push(name);
            }
        }
        dirs.sort_unstable();
        files.sort_unstable();
        files.retain(|name| !crate::instructions::is_instruction_file(name));
        dirs.append(&mut files);
        let instructions = crate::instructions::find_subdirectory_instructions(
            &path,
            workspace.root(),
            &workspace.loaded_instructions,
        );
        Ok(ListOutput {
            entries: dirs,
            instructions,
        })
    }
}

impl_tool!(
    List,
    ListArgs,
    ListOutput,
    "list",
    "List directory contents. Entries are sorted alphabetically with directories first and a trailing '/'. Instruction files (AGENTS.md, CLAUDE.md, etc.) are filtered out. No symlinks, Git metadata, or paths outside the workspace."
);
