use rig_core::tool::{IntoToolOutput, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{
    MAX_LINE_BYTES, MAX_OUTPUT_BYTES, Result, Workspace, clip, impl_tool, invalid, read_bytes, text,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    /// Workspace-relative path, or an absolute path inside the workspace.
    pub path: String,
    /// First line, one-based.
    #[serde(default = "default_offset")]
    pub offset: usize,
    /// Lines to return (default 200, maximum 2000; 0 means 2000).
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_offset() -> usize {
    1
}
fn default_limit() -> usize {
    200
}

#[derive(Debug)]
pub struct ReadLine {
    pub number: usize,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug)]
pub struct ReadOutput {
    pub path: String,
    pub lines: Vec<ReadLine>,
    pub total_lines: usize,
    pub next_offset: Option<usize>,
}

impl IntoToolOutput for ReadOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        let mut text = self
            .lines
            .iter()
            .map(|line| {
                format!(
                    "{}: {}{}",
                    line.number,
                    line.text,
                    if line.truncated { "..." } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(offset) = self.next_offset {
            text.push_str(&format!(
                "\n\n...\n\nTruncated lines: {offset}-{}. Use offset={offset} to read further.",
                self.total_lines
            ));
        }
        Ok(ToolOutput::text(text))
    }
}

#[derive(Clone)]
pub struct Read(pub Workspace);

impl Read {
    fn execute(workspace: &Workspace, args: ReadArgs) -> Result<ReadOutput> {
        if args.offset == 0 || args.limit > 2000 {
            return Err(invalid(
                "offset must be >= 1 and limit must be between 0 and 2000",
            ));
        }
        let path = workspace.file(&args.path)?;
        let contents = text(read_bytes(&path)?)?;
        let total_lines = contents.lines().count();
        if args.offset > total_lines.saturating_add(1) {
            return Err(invalid(format!(
                "offset exceeds end of file ({total_lines} lines)"
            )));
        }
        let limit = if args.limit == 0 { 2000 } else { args.limit };
        let mut lines = Vec::new();
        let mut bytes = 0;
        for (index, line) in contents
            .lines()
            .enumerate()
            .skip(args.offset - 1)
            .take(limit)
        {
            let (line, truncated) = clip(line, MAX_LINE_BYTES);
            if bytes + line.len() > MAX_OUTPUT_BYTES {
                break;
            }
            bytes += line.len();
            lines.push(ReadLine {
                number: index + 1,
                text: line.into(),
                truncated,
            });
        }
        let next = args.offset + lines.len();
        Ok(ReadOutput {
            path: workspace.display(&path),
            lines,
            total_lines,
            next_offset: (next <= total_lines).then_some(next),
        })
    }
}

impl_tool!(
    Read,
    ReadArgs,
    ReadOutput,
    "read",
    "Read UTF-8 text with one-based line numbers. Use offset/limit for paging and the returned truncation hint to continue. Files are capped at 8 MiB; lines longer than 2048 bytes end with '...'. No binary files, symlinks, or paths outside the workspace."
);
