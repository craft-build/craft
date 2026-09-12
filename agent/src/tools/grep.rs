use ignore::{WalkBuilder, overrides::OverrideBuilder};
use regex::RegexBuilder;
use rig_core::tool::{IntoToolOutput, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{
    MAX_FILE_BYTES, MAX_LINE_BYTES, MAX_OUTPUT_BYTES, Result, Workspace, clip, failure, impl_tool,
    invalid, read_bytes,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepArgs {
    pub pattern: String,
    /// File or directory to search, relative to the workspace.
    #[serde(default = "default_path")]
    pub path: String,
    /// Optional gitignore-style include glob, e.g. "*.rs" or "src/**".
    pub glob: Option<String>,
    #[serde(default = "default_true")]
    pub case_sensitive: bool,
    #[serde(default)]
    pub literal: bool,
    /// Maximum matching lines (default 100, maximum 1000).
    #[serde(default = "default_limit")]
    pub max_matches: usize,
}

fn default_path() -> String {
    ".".into()
}
fn default_true() -> bool {
    true
}
fn default_limit() -> usize {
    100
}

#[derive(Debug)]
pub struct GrepMatch {
    pub path: String,
    pub line: usize,
    /// One-based byte column of the first match on this line.
    pub column: usize,
    /// One-based byte column where the excerpt starts.
    pub text_start_column: usize,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug)]
pub struct GrepOutput {
    pub matches: Vec<GrepMatch>,
    /// Search stopped at a match, output, or scan budget; more results may exist.
    pub truncated: bool,
    /// Binary/non-UTF-8/oversized files omitted from the search.
    pub skipped_files: usize,
}

impl IntoToolOutput for GrepOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        let mut rows = Vec::new();
        let mut previous_path = None;
        for found in &self.matches {
            if previous_path != Some(&found.path) {
                if previous_path.is_some() {
                    rows.push(String::new());
                }
                rows.push(format!("{}:", found.path));
                previous_path = Some(&found.path);
            }
            rows.push(format!("  {}: {}", found.line, found.text));
            if found.truncated {
                rows.push(format!(
                    "  [line truncated; excerpt starts at byte column {}; match at byte column {}]",
                    found.text_start_column, found.column
                ));
            }
        }
        if rows.is_empty() {
            rows.push("No files found".into());
        }
        if self.truncated {
            rows.push("\n[Search truncated: more matches may exist. Narrow the path or pattern, or increase max_matches.]".into());
        }
        if self.skipped_files > 0 {
            rows.push(format!(
                "\n[Skipped {} files; results cover only the files searched.]",
                self.skipped_files
            ));
        }
        Ok(ToolOutput::text(rows.join("\n")))
    }
}

#[derive(Clone)]
pub struct Grep(pub Workspace);

impl Grep {
    fn execute(workspace: &Workspace, args: GrepArgs) -> Result<GrepOutput> {
        if args.max_matches == 0 || args.max_matches > 1000 || args.pattern.len() > 16384 {
            return Err(invalid(
                "max_matches must be 1..=1000 and pattern must be at most 16 KiB",
            ));
        }
        let pattern = if args.literal {
            regex::escape(&args.pattern)
        } else {
            args.pattern
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(!args.case_sensitive)
            .size_limit(1024 * 1024)
            .build()
            .map_err(|error| invalid(format!("invalid regex: {error}")))?;
        let root = workspace.resolve(&args.path)?;
        if !root.is_dir() && !root.is_file() {
            return Err(invalid("search path must be a file or directory"));
        }
        // Start at the workspace so its ignore rules also apply to narrowed
        // searches. Prune unrelated branches rather than reading parent rules
        // from outside the workspace.
        let mut walk = WalkBuilder::new(workspace.root());
        walk.follow_links(false)
            .hidden(true)
            .parents(false)
            .git_global(false)
            .require_git(false)
            .sort_by_file_path(|a, b| a.cmp(b));
        let includes = if let Some(glob) = args.glob {
            if glob.is_empty() || glob.starts_with('!') {
                return Err(invalid("glob must be a nonempty include pattern"));
            }
            let mut overrides = OverrideBuilder::new(workspace.root());
            overrides
                .add(&glob)
                .map_err(|error| invalid(format!("invalid glob: {error}")))?;
            Some(
                overrides
                    .build()
                    .map_err(|error| invalid(error.to_string()))?,
            )
        } else {
            None
        };
        // Do not install include globs as walker overrides: that would unignore
        // ignored/hidden files. A glob only narrows the normal search.
        walk.filter_entry(move |entry| {
            !entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(".git")
                && (entry.path().starts_with(&root) || root.starts_with(entry.path()))
        });
        let mut output = GrepOutput {
            matches: Vec::new(),
            truncated: false,
            skipped_files: 0,
        };
        let mut output_bytes = 0;
        let mut scanned_bytes = 0;
        let mut scanned_files = 0;
        'files: for entry in walk.build() {
            let entry =
                entry.map_err(|error| failure(format!("search traversal failed: {error}")))?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if scanned_files >= 10000 || scanned_bytes >= 64 * 1024 * 1024 {
                output.truncated = true;
                break;
            }
            scanned_files += 1;
            if includes
                .as_ref()
                .is_some_and(|glob| !glob.matched(entry.path(), false).is_whitelist())
            {
                continue;
            }
            let Some(relative) = entry
                .path()
                .strip_prefix(workspace.root())
                .ok()
                .and_then(|path| path.to_str())
            else {
                output.skipped_files += 1;
                continue;
            };
            let path = workspace.file(relative)?;
            let size = entry
                .metadata()
                .map_err(|error| failure(error.to_string()))?
                .len();
            if size > MAX_FILE_BYTES as u64 {
                output.skipped_files += 1;
                continue;
            }
            if scanned_bytes as u64 + size > 64 * 1024 * 1024 {
                output.truncated = true;
                break;
            }
            let bytes = read_bytes(&path)?;
            scanned_bytes += bytes.len();
            if bytes.contains(&0) {
                output.skipped_files += 1;
                continue;
            }
            let Ok(contents) = String::from_utf8(bytes) else {
                output.skipped_files += 1;
                continue;
            };
            for (index, line) in contents.lines().enumerate() {
                let Some(found) = regex.find(line) else {
                    continue;
                };
                // Include the match even when it occurs after a very long prefix.
                let mut start = found.start().saturating_sub(256);
                while !line.is_char_boundary(start) {
                    start -= 1;
                }
                let (text, clipped_end) = clip(&line[start..], MAX_LINE_BYTES);
                if output.matches.len() >= args.max_matches
                    || output_bytes + text.len() > MAX_OUTPUT_BYTES
                {
                    output.truncated = true;
                    break 'files;
                }
                output_bytes += text.len();
                output.matches.push(GrepMatch {
                    path: workspace.display(&path),
                    line: index + 1,
                    column: found.start() + 1,
                    text_start_column: start + 1,
                    text: text.into(),
                    truncated: start > 0 || clipped_end,
                });
            }
        }
        Ok(output)
    }
}

impl_tool!(
    Grep,
    GrepArgs,
    GrepOutput,
    "grep",
    "Search UTF-8 file contents with a Rust regex (line-based, no multiline), or literal text. Respects gitignore/ignore files and skips hidden files, symlinks, binary files, and files over 8 MiB. Returns paths and one-based lines, capped matches and text, with explicit truncation. An optional include glob narrows the search. Scan budget: 10000 files or 64 MiB. Paths must stay inside the workspace."
);
