//! Codex-style `apply_patch`: parse `*** Begin Patch` / `*** End Patch`
//! text and apply Add/Update/Delete file hunks with fuzzy context matching.
//!
//! Ported from the reference implementation; the pure string algorithm
//! (parser, chunk matching, replacement planning) is unchanged, while file
//! I/O goes through the workspace's safety and atomic-write primitives.

use std::io::Write as _;

use rig_core::tool::{IntoToolOutput, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::edit::persist;
use super::{
    MAX_FILE_BYTES, Result, Workspace, failure, impl_tool, invalid, io_error, read_bytes, text,
};

const DIFF_MAX_LINES: usize = 30;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchArgs {
    /// Codex-style patch text with *** Begin Patch / *** End Patch markers.
    pub patch_text: String,
}

#[derive(Debug)]
pub struct ApplyPatchOutput {
    pub applied: Vec<String>,
}

impl IntoToolOutput for ApplyPatchOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        Ok(ToolOutput::text(self.applied.join("\n")))
    }
}

#[derive(Clone)]
pub struct ApplyPatch(pub Workspace);

#[derive(Debug, Clone)]
struct UpdateFileChunk {
    change_context: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    is_end_of_file: bool,
}

#[derive(Debug)]
#[allow(clippy::enum_variant_names)]
enum PatchHunk {
    AddFile {
        path: String,
        contents: String,
    },
    DeleteFile {
        path: String,
    },
    UpdateFile {
        path: String,
        chunks: Vec<UpdateFileChunk>,
    },
}

/// Every file path a patch text touches, for dedup-cache invalidation.
pub fn patch_paths(patch_text: &str) -> Vec<String> {
    patch_text
        .lines()
        .filter_map(|line| {
            ["*** Add File: ", "*** Delete File: ", "*** Update File: "]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
                .map(str::trim)
                .map(str::to_string)
        })
        .collect()
}

impl ApplyPatch {
    fn execute(workspace: &Workspace, args: ApplyPatchArgs) -> Result<ApplyPatchOutput> {
        if args.patch_text.contains('\0') {
            return Err(invalid("patch_text must not contain NUL bytes"));
        }
        let hunks = parse_apply_patch(&args.patch_text).map_err(invalid)?;
        let mut results = Vec::new();
        for hunk in &hunks {
            match hunk {
                PatchHunk::AddFile { path, contents } => {
                    let display = Self::add_file(workspace, path, contents)?;
                    results.push(format!("{display}: created"));
                }
                PatchHunk::DeleteFile { path } => {
                    let resolved = workspace.file(path)?;
                    let before = text(read_bytes(&resolved)?)?;
                    let display = workspace.display(&resolved);
                    let summary =
                        diff_summary(&before, "", &format!("{display}: deleted"), &display);
                    workspace.note_snapshot(&resolved);
                    std::fs::remove_file(&resolved).map_err(io_error)?;
                    results.push(summary);
                }
                PatchHunk::UpdateFile { path, chunks } => {
                    let resolved = workspace.file(path)?;
                    let before = text(read_bytes(&resolved)?)?;
                    let after = apply_update_chunks(&before, chunks, path).map_err(invalid)?;
                    if after.len() > MAX_FILE_BYTES {
                        return Err(invalid(
                            "patched file would exceed the 8 MiB tool size limit",
                        ));
                    }
                    let display = workspace.display(&resolved);
                    let summary = diff_summary(
                        &before,
                        &after,
                        &format!("{display}: modified ({} hunks)", chunks.len()),
                        &display,
                    );
                    persist(workspace, path, &resolved, &before, &after)?;
                    results.push(summary);
                }
            }
        }
        if results.is_empty() {
            results.push("No changes applied".into());
        }
        Ok(ApplyPatchOutput { applied: results })
    }

    /// Create a new file through the workspace write path (staged, atomic,
    /// snapshot for `/undo`). Existing files are refused. Returns the
    /// workspace-relative display path.
    fn add_file(workspace: &Workspace, requested: &str, contents: &str) -> Result<String> {
        if contents.contains('\0') {
            return Err(invalid("added file content must not contain NUL bytes"));
        }
        if contents.len() > MAX_FILE_BYTES {
            return Err(invalid("added file would exceed the 8 MiB tool size limit"));
        }
        let path = workspace.target(requested)?;
        if path.exists() {
            return Err(invalid(format!(
                "Add File target {} already exists; use Update File",
                workspace.display(&path)
            )));
        }
        workspace.note_snapshot(&path);
        let mut staged =
            tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(io_error)?;
        staged.write_all(contents.as_bytes()).map_err(io_error)?;
        staged.as_file().sync_all().map_err(io_error)?;
        if workspace.target(requested)? != path || path.exists() {
            return Err(failure(
                "file changed while preparing the patch; read it again and retry",
            ));
        }
        staged
            .persist(&path)
            .map_err(|error| io_error(error.error))?;
        Ok(workspace.display(&path))
    }
}

impl_tool!(
    ApplyPatch,
    ApplyPatchArgs,
    ApplyPatchOutput,
    "apply_patch",
    "Apply a Codex-style patch (*** Begin Patch / *** End Patch) adding, updating, or deleting multiple workspace files at once. Update hunks use ' ', '+', '-' diff lines with optional '@@ context' markers and three-pass fuzzy matching (exact, trailing-whitespace, fully trimmed); '*** End of File' anchors a hunk at the end. Files apply sequentially: a later failure leaves earlier files patched. No symlinks, Git metadata, or paths outside the workspace."
);

fn apply_update_chunks(
    original_contents: &str,
    chunks: &[UpdateFileChunk],
    path: &str,
) -> std::result::Result<String, String> {
    let mut original_lines: Vec<String> = original_contents.split('\n').map(String::from).collect();
    let had_trailing_newline = original_lines.last().is_some_and(String::is_empty);
    if had_trailing_newline {
        original_lines.pop();
    }

    let replacements = compute_replacements(&original_lines, path, chunks)?;
    let mut new_lines = apply_replacements(original_lines, &replacements);
    if had_trailing_newline && !new_lines.last().is_some_and(String::is_empty) {
        new_lines.push(String::new());
    }
    Ok(new_lines.join("\n"))
}

fn compute_replacements(
    original_lines: &[String],
    path: &str,
    chunks: &[UpdateFileChunk],
) -> std::result::Result<Vec<(usize, usize, Vec<String>)>, String> {
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index: usize = 0;

    for chunk in chunks {
        if let Some(ctx_line) = &chunk.change_context
            && let Some(idx) = seek_sequence(
                original_lines,
                std::slice::from_ref(ctx_line),
                line_index,
                false,
            )
        {
            line_index = idx + 1;
        } else if let Some(ctx_line) = &chunk.change_context {
            return Err(format!("Failed to find context '{ctx_line}' in {path}"));
        }

        if chunk.old_lines.is_empty() {
            let insertion_idx = if original_lines.last().is_some_and(String::is_empty) {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern: &[String] = &chunk.old_lines;
        let mut found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        let mut new_slice: &[String] = &chunk.new_lines;

        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        }

        if let Some(start_idx) = found {
            replacements.push((start_idx, pattern.len(), new_slice.to_vec()));
            line_index = start_idx + pattern.len();
        } else {
            return Err(format!(
                "Failed to find expected lines in {path}:\n{}",
                chunk.old_lines.join("\n"),
            ));
        }
    }

    replacements.sort_by_key(|(a, _, _)| *a);
    for window in replacements.windows(2) {
        let (a_start, a_len, _) = &window[0];
        let (b_start, _, _) = &window[1];
        if *b_start < *a_start + *a_len {
            return Err(format!("Overlapping hunks in {path}"));
        }
    }
    Ok(replacements)
}

fn apply_replacements(
    lines: Vec<String>,
    replacements: &[(usize, usize, Vec<String>)],
) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    let mut cursor = 0;
    for (start, old_len, new_seg) in replacements {
        out.extend(lines[cursor..*start].iter().cloned());
        out.extend(new_seg.iter().cloned());
        cursor = *start + *old_len;
    }
    out.extend(lines[cursor..].iter().cloned());
    out
}

/// Locate `pattern` in `lines` at or after `start` (or at the end when
/// `eof`), trying an exact match, then trailing-whitespace-tolerant, then
/// fully trimmed.
fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }

    let search_start = if eof && lines.len() >= pattern.len() {
        lines.len() - pattern.len()
    } else {
        start
    };

    let last = lines.len() - pattern.len();
    // Pass 1: exact match.
    for i in search_start..=last {
        if lines[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
    }
    // Pass 2: trim-end match.
    for i in search_start..=last {
        if (0..pattern.len()).all(|p| lines[i + p].trim_end() == pattern[p].trim_end()) {
            return Some(i);
        }
    }
    // Pass 3: full-trim match.
    for i in search_start..=last {
        if (0..pattern.len()).all(|p| lines[i + p].trim() == pattern[p].trim()) {
            return Some(i);
        }
    }
    None
}

/// Unified diff via [`crate::diff::unified_text`], truncated to a line budget
/// so large patches cannot blow the context window.
fn diff_summary(old: &str, new: &str, summary: &str, display_path: &str) -> String {
    let text = crate::diff::unified_text(old, new, summary, display_path);
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.len() > DIFF_MAX_LINES {
        lines.truncate(DIFF_MAX_LINES);
        lines.push("... (diff truncated)");
    }
    lines.join("\n")
}

fn parse_apply_patch(input: &str) -> std::result::Result<Vec<PatchHunk>, String> {
    let lines: Vec<&str> = input.lines().collect();

    let start = lines
        .iter()
        .position(|l| l.trim() == "*** Begin Patch")
        .ok_or_else(|| "Patch must contain *** Begin Patch".to_string())?;

    let mut hunks = Vec::new();
    let mut i = start + 1;

    while i < lines.len() {
        let line = lines[i].trim_end();
        if line.trim() == "*** End Patch" {
            break;
        }

        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let path = path.trim().to_string();
            i += 1;
            let mut contents = String::new();
            while i < lines.len() {
                let current = lines[i];
                if current.starts_with("*** ") {
                    break;
                }
                if let Some(added) = current.strip_prefix('+') {
                    contents.push_str(added);
                    contents.push('\n');
                }
                i += 1;
            }
            hunks.push(PatchHunk::AddFile { path, contents });
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            hunks.push(PatchHunk::DeleteFile {
                path: path.trim().to_string(),
            });
            i += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Update File: ") {
            let path = path.trim().to_string();
            i += 1;
            let mut chunks = Vec::new();
            let mut is_first_chunk = true;

            while i < lines.len() {
                let current = lines[i].trim_end();

                if current.starts_with("*** ") && current != "*** End of File" {
                    break;
                }

                if current.trim().is_empty()
                    && !current.starts_with(' ')
                    && !current.starts_with('+')
                    && !current.starts_with('-')
                {
                    i += 1;
                    continue;
                }

                let change_context;
                if current == "@@" {
                    change_context = None;
                    i += 1;
                } else if let Some(ctx) = current.strip_prefix("@@ ") {
                    change_context = Some(ctx.to_string());
                    i += 1;
                } else if is_first_chunk {
                    change_context = None;
                } else {
                    break;
                }

                let mut old_lines = Vec::new();
                let mut new_lines = Vec::new();
                let mut is_end_of_file = false;
                let mut had_diff_lines = false;

                while i < lines.len() {
                    let cl = lines[i];

                    if cl == "*** End of File" {
                        is_end_of_file = true;
                        i += 1;
                        break;
                    }

                    if cl.starts_with("*** ") || cl.starts_with("@@") {
                        break;
                    }

                    if let Some(content) = cl.strip_prefix(' ') {
                        old_lines.push(content.to_string());
                        new_lines.push(content.to_string());
                        had_diff_lines = true;
                    } else if let Some(content) = cl.strip_prefix('+') {
                        new_lines.push(content.to_string());
                        had_diff_lines = true;
                    } else if let Some(content) = cl.strip_prefix('-') {
                        old_lines.push(content.to_string());
                        had_diff_lines = true;
                    } else if cl.is_empty() {
                        old_lines.push(String::new());
                        new_lines.push(String::new());
                        had_diff_lines = true;
                    } else {
                        if had_diff_lines {
                            break;
                        }
                        i += 1;
                        continue;
                    }

                    i += 1;
                }

                if had_diff_lines || change_context.is_some() {
                    chunks.push(UpdateFileChunk {
                        change_context,
                        old_lines,
                        new_lines,
                        is_end_of_file,
                    });
                }

                is_first_chunk = false;
            }

            if chunks.is_empty() {
                return Err(format!("Update file hunk for '{path}' has no changes"));
            }

            hunks.push(PatchHunk::UpdateFile { path, chunks });
            continue;
        }

        i += 1;
    }

    if hunks.is_empty() {
        return Err("No valid patch directives found".to_string());
    }

    Ok(hunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_add_file() {
        let patch =
            "*** Begin Patch\n*** Add File: hello.txt\n+Hello world\n+Second line\n*** End Patch";
        let hunks = parse_apply_patch(patch).unwrap();
        assert_eq!(hunks.len(), 1);
        match &hunks[0] {
            PatchHunk::AddFile { path, contents } => {
                assert_eq!(path, "hello.txt");
                assert_eq!(contents, "Hello world\nSecond line\n");
            }
            _ => panic!("Expected AddFile"),
        }
    }

    #[test]
    fn parse_delete_file() {
        let patch = "*** Begin Patch\n*** Delete File: old.txt\n*** End Patch";
        match &parse_apply_patch(patch).unwrap()[0] {
            PatchHunk::DeleteFile { path } => assert_eq!(path, "old.txt"),
            _ => panic!("Expected DeleteFile"),
        }
    }

    #[test]
    fn parse_update_file_simple() {
        let patch =
            "*** Begin Patch\n*** Update File: test.py\n@@\n foo\n-bar\n+baz\n*** End Patch";
        match &parse_apply_patch(patch).unwrap()[0] {
            PatchHunk::UpdateFile { path, chunks, .. } => {
                assert_eq!(path, "test.py");
                assert_eq!(chunks.len(), 1);
                assert_eq!(chunks[0].old_lines, vec!["foo", "bar"]);
                assert_eq!(chunks[0].new_lines, vec!["foo", "baz"]);
            }
            _ => panic!("Expected UpdateFile"),
        }
    }

    #[test]
    fn parse_update_with_context() {
        let patch = "*** Begin Patch\n*** Update File: test.py\n@@ def my_func():\n-    pass\n+    return 42\n*** End Patch";
        match &parse_apply_patch(patch).unwrap()[0] {
            PatchHunk::UpdateFile { chunks, .. } => {
                assert_eq!(chunks[0].change_context, Some("def my_func():".to_string()));
                assert_eq!(chunks[0].old_lines, vec!["    pass"]);
                assert_eq!(chunks[0].new_lines, vec!["    return 42"]);
            }
            _ => panic!("Expected UpdateFile"),
        }
    }

    #[test]
    fn parse_multiple_chunks() {
        let patch = "*** Begin Patch\n*** Update File: test.py\n@@\n foo\n-bar\n+BAR\n@@\n baz\n-qux\n+QUX\n*** End Patch";
        match &parse_apply_patch(patch).unwrap()[0] {
            PatchHunk::UpdateFile { chunks, .. } => {
                assert_eq!(chunks.len(), 2);
                assert_eq!(chunks[0].old_lines, vec!["foo", "bar"]);
                assert_eq!(chunks[0].new_lines, vec!["foo", "BAR"]);
                assert_eq!(chunks[1].old_lines, vec!["baz", "qux"]);
                assert_eq!(chunks[1].new_lines, vec!["baz", "QUX"]);
            }
            _ => panic!("Expected UpdateFile"),
        }
    }

    #[test]
    fn parse_end_of_file() {
        let patch = "*** Begin Patch\n*** Update File: test.py\n@@\n last_line\n+new_last_line\n*** End of File\n*** End Patch";
        match &parse_apply_patch(patch).unwrap()[0] {
            PatchHunk::UpdateFile { chunks, .. } => assert!(chunks[0].is_end_of_file),
            _ => panic!("Expected UpdateFile"),
        }
    }

    #[test]
    fn parse_heredoc_wrapper() {
        let patch = "<<'EOF'\n*** Begin Patch\n*** Add File: test.txt\n+hello\n*** End Patch\nEOF";
        assert_eq!(parse_apply_patch(patch).unwrap().len(), 1);
    }

    #[test]
    fn parse_update_without_explicit_at() {
        let patch = "*** Begin Patch\n*** Update File: file.py\n import foo\n+bar\n*** End Patch";
        match &parse_apply_patch(patch).unwrap()[0] {
            PatchHunk::UpdateFile { chunks, .. } => {
                assert_eq!(chunks.len(), 1);
                assert!(chunks[0].change_context.is_none());
            }
            _ => panic!("Expected UpdateFile"),
        }
    }

    #[test]
    fn parse_errors() {
        let err = parse_apply_patch("random text").unwrap_err();
        assert!(err.contains("*** Begin Patch"), "{err}");
        let err = parse_apply_patch("*** Begin Patch\n*** End Patch").unwrap_err();
        assert!(err.contains("No valid patch directives"), "{err}");
        let err = parse_apply_patch("*** Begin Patch\n*** Update File: x.txt\n*** End Patch")
            .unwrap_err();
        assert!(err.contains("has no changes"), "{err}");
    }

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn seek_sequence_exact_match() {
        let file = lines(&["foo", "bar", "baz"]);
        let pattern = lines(&["bar", "baz"]);
        assert_eq!(seek_sequence(&file, &pattern, 0, false), Some(1));
    }

    #[test]
    fn seek_sequence_whitespace_tolerant() {
        let file = lines(&["foo   ", "bar\t"]);
        let pattern = lines(&["foo", "bar"]);
        assert_eq!(seek_sequence(&file, &pattern, 0, false), Some(0));
    }

    #[test]
    fn seek_sequence_eof_anchor() {
        let file = lines(&["a", "b", "c", "d"]);
        let pattern = lines(&["c", "d"]);
        assert_eq!(seek_sequence(&file, &pattern, 0, true), Some(2));
    }

    #[test]
    fn diff_summary_unified_format() {
        let old = "line one\nline two\nline three\n";
        let new = "line one\nchanged two\nline three\n";
        let diff = diff_summary(old, new, "edited f", "f");
        assert!(diff.contains("edited f"), "{diff}");
        assert!(diff.contains("@@ -1 +1 @@"), "{diff}");
        assert!(diff.contains("- line two"), "{diff}");
        assert!(diff.contains("+ changed two"), "{diff}");
        // Context lines survive (they anchor the hunk for the TUI gutter).
        assert!(diff.contains("  line one"), "{diff}");
    }

    #[test]
    fn patch_paths_extracts_every_directive() {
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** Update File: b.txt\n@@\n-c\n+d\n*** Delete File: c.txt\n*** End Patch";
        assert_eq!(patch_paths(patch), vec!["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn update_missing_context_errors() {
        let chunks = [UpdateFileChunk {
            change_context: Some("no such line".into()),
            old_lines: vec!["x".into()],
            new_lines: vec!["y".into()],
            is_end_of_file: false,
        }];
        let err = apply_update_chunks("a\nb\n", &chunks, "f.txt").unwrap_err();
        assert!(err.contains("Failed to find context"), "{err}");
    }

    #[test]
    fn update_trailing_newline_preserved() {
        let chunks = [UpdateFileChunk {
            change_context: None,
            old_lines: vec!["a".into()],
            new_lines: vec!["A".into()],
            is_end_of_file: false,
        }];
        assert_eq!(
            apply_update_chunks("a\nb\n", &chunks, "f").unwrap(),
            "A\nb\n"
        );
    }
}
