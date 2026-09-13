use std::time::SystemTime;

use ignore::{WalkBuilder, overrides::OverrideBuilder};
use rig_core::tool::{IntoToolOutput, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{MAX_OUTPUT_BYTES, Result, Workspace, impl_tool, invalid};

const MAX_RESULTS: usize = 100;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobArgs {
    /// Glob pattern, e.g. `**/*.rs` or `src/**/*.ts`. Gitignore-style.
    pub pattern: String,
    /// Directory to search in, relative to the workspace.
    #[serde(default = "default_path")]
    pub path: String,
}

fn default_path() -> String {
    ".".into()
}

#[derive(Debug)]
pub struct GlobOutput {
    pub paths: Vec<String>,
    /// The result cap or output budget was hit; more files may exist.
    pub truncated: bool,
}

impl IntoToolOutput for GlobOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        if self.paths.is_empty() {
            return Ok(ToolOutput::text("No files found"));
        }
        let mut text = self.paths.join("\n");
        if self.truncated {
            text.push_str("\n\n[Truncated: more files may exist. Narrow the path or pattern.]");
        }
        Ok(ToolOutput::text(text))
    }
}

#[derive(Clone)]
pub struct Glob(pub Workspace);

impl Glob {
    fn execute(workspace: &Workspace, args: GlobArgs) -> Result<GlobOutput> {
        if args.pattern.is_empty() || args.pattern.starts_with('!') {
            return Err(invalid("pattern must be a nonempty include glob"));
        }
        let root = workspace.resolve(&args.path)?;
        if !root.is_dir() {
            return Err(invalid("glob path must be a directory"));
        }
        let mut overrides = OverrideBuilder::new(workspace.root());
        overrides
            .add(&args.pattern)
            .map_err(|error| invalid(format!("invalid glob: {error}")))?;
        let includes = overrides
            .build()
            .map_err(|error| invalid(error.to_string()))?;
        let mut walk = WalkBuilder::new(workspace.root());
        walk.follow_links(false)
            .hidden(true)
            .parents(false)
            .git_global(false)
            .require_git(false)
            .filter_entry(move |entry| {
                !entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(".git")
                    && (entry.path().starts_with(&root) || root.starts_with(entry.path()))
            });
        let mut found: Vec<(SystemTime, String)> = Vec::new();
        let mut truncated = false;
        for entry in walk.build().flatten() {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if !includes.matched(entry.path(), false).is_whitelist() {
                continue;
            }
            if found.len() >= MAX_RESULTS {
                truncated = true;
                break;
            }
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            found.push((modified, workspace.display(entry.path())));
        }
        found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let mut paths: Vec<String> = found.into_iter().map(|(_, path)| path).collect();
        let mut bytes = 0;
        for (index, path) in paths.iter().enumerate() {
            if bytes + path.len() > MAX_OUTPUT_BYTES {
                paths.truncate(index);
                truncated = true;
                break;
            }
            bytes += path.len();
        }
        Ok(GlobOutput { paths, truncated })
    }
}

impl_tool!(
    Glob,
    GlobArgs,
    GlobOutput,
    "glob",
    "Find files by gitignore-style glob pattern. Respects .gitignore. Returns workspace-relative paths sorted by modification time (newest first), capped at 100 results; prefer speculative parallel searches over sequential rounds of glob+grep."
);
