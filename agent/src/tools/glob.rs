use std::time::SystemTime;

use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput, ToolScheduling};
use async_trait::async_trait;
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::error::{Error, Result, definition, finish, invalid};
use super::{MAX_OUTPUT_BYTES, Workspace, claims};

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

impl super::error::IntoToolResult for GlobOutput {
    fn into_tool_result(self) -> ToolOutput {
        if self.paths.is_empty() {
            return super::error::text_output("No files found");
        }
        let mut text = self.paths.join("\n");
        if self.truncated {
            text.push_str("\n\n[Truncated: more files may exist. Narrow the path or pattern.]");
        }
        super::error::text_output(text)
    }
}

#[derive(Clone)]
pub struct Glob(pub Workspace);

impl Glob {
    async fn execute(
        workspace: &Workspace,
        args: GlobArgs,
        cancel: &animist::agent::Cancellation,
    ) -> Result<GlobOutput> {
        if args.pattern.is_empty() || args.pattern.starts_with('!') {
            return Err(invalid("pattern must be a nonempty include glob"));
        }
        let root = workspace.resolve(&args.path)?;
        if !root.is_dir() {
            return Err(invalid("glob path must be a directory"));
        }
        // Cross-agent: exclude the workspace-wide writer for the walk.
        let _guard = workspace.read_guard(&[], cancel).await?;
        let mut overrides = OverrideBuilder::new(workspace.root());
        overrides
            .add(&args.pattern)
            .map_err(|error| invalid(format!("invalid glob: {error}")))?;
        let includes = overrides
            .build()
            .map_err(|error| invalid(error.to_string()))?;
        // The ignore walker is sync-only; the whole bounded scan runs as
        // one blocking unit.
        let walker = workspace.clone();
        let search_root = root.clone();
        let mut found: Vec<(SystemTime, String)> = tokio::task::spawn_blocking(move || {
            let mut walk = WalkBuilder::new(walker.root());
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
                        && (entry.path().starts_with(&search_root)
                            || search_root.starts_with(entry.path()))
                });
            let mut found: Vec<(SystemTime, String)> = Vec::new();
            for entry in walk.build().flatten() {
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    continue;
                }
                if !includes.matched(entry.path(), false).is_whitelist() {
                    continue;
                }
                if found.len() >= MAX_RESULTS {
                    break;
                }
                let modified = entry
                    .metadata()
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                found.push((modified, walker.display(entry.path())));
            }
            found
        })
        .await
        .map_err(|source| Error::Blocking {
            source: Box::new(source),
        })?;
        let truncated_cap = found.len() >= MAX_RESULTS;
        let mut truncated = truncated_cap;
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

#[async_trait]
impl ToolImpl for Glob {
    type Args = GlobArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<GlobArgs>(
            "glob",
            "Find files by gitignore-style glob pattern. Respects .gitignore. Returns workspace-relative paths sorted by modification time (newest first), capped at 100 results; prefer speculative parallel searches over sequential rounds of glob+grep.",
        )
    }

    fn scheduling(&self, input: &Value) -> ToolScheduling {
        // The walk claims its start directory, so writes beneath it conflict.
        let fallback = default_path();
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(fallback.as_str());
        claims::reads(self.0.root_key().as_deref(), &[path.to_owned()])
    }

    async fn call(&self, context: &ToolContext<'_>, args: GlobArgs) -> Result<ToolOutput> {
        finish(Self::execute(&self.0, args, &context.run.cancellation).await)
    }
}
