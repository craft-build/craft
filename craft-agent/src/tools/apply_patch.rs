use std::path::Path;

use crate::{AgentMode, ToolOutput};
use craft_tool_macro::Tool;
use serde::Deserialize;
use similar::{ChangeTag, TextDiff};

use super::relative_path;
use crate::tools::PLAN_WRITE_RESTRICTED;

const DIFF_MAX_LINES: usize = 30;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct ApplyPatch {
    #[param(description = "Codex-style patch text with *** Begin Patch / *** End Patch markers")]
    patch_text: String,
}

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
    AddFile { path: String, contents: String },
    DeleteFile { path: String },
    UpdateFile { path: String, chunks: Vec<UpdateFileChunk> },
}

impl ApplyPatch {
    pub const NAME: &str = "apply_patch";
    pub const DESCRIPTION: &str = include_str!("apply_patch.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[{"patch_text": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n fn main() {\n-    println!(\"old\");\n+    println!(\"new\");\n }\n*** End Patch"}]"#,
    );

    pub fn extract_paths(&self) -> Vec<String> {
        match parse_apply_patch(&self.patch_text) {
            Ok(hunks) => hunks
                .iter()
                .map(|h| match h {
                    PatchHunk::AddFile { path, .. }
                    | PatchHunk::DeleteFile { path, .. }
                    | PatchHunk::UpdateFile { path, .. } => path.clone(),
                })
                .collect(),
            Err(_) => vec![],
        }
    }

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let hunks = parse_apply_patch(&self.patch_text).map_err(|e| e.to_string())?;

        if let AgentMode::Plan(plan_path) = &ctx.mode {
            for hunk in &hunks {
                let hunk_path = match hunk {
                    PatchHunk::AddFile { path, .. }
                    | PatchHunk::DeleteFile { path, .. }
                    | PatchHunk::UpdateFile { path, .. } => path,
                };
                let resolved = super::resolve_path(hunk_path)?;
                if Path::new(&resolved) != plan_path.as_path() {
                    return Err(PLAN_WRITE_RESTRICTED.into());
                }
            }
        }

        let mut results = Vec::new();
        let mut touched_paths = Vec::new();
        let mut last_before = String::new();
        let mut last_after = String::new();

        for hunk in &hunks {
            match hunk {
                PatchHunk::AddFile { path, contents } => {
                    let resolved = super::resolve_path(path)?;
                    let p = Path::new(&resolved);
                    if let Some(parent) = p.parent() {
                        tokio::fs::create_dir_all(parent)
                            .await
                            .map_err(|e| format!("mkdir error for {path}: {e}"))?;
                    }
                    let diff = generate_diff_summary("", contents);
                    ctx.fs
                        .write_text_file(p, contents)
                        .await
                        .map_err(|e| format!("write error for {path}: {e}"))?;
                    ctx.file_tracker.record_read(p);

                    touched_paths.push(path.clone());
                    last_before = String::new();
                    last_after = contents.clone();
                    if diff.is_empty() {
                        results.push(format!("{path}: created"));
                    } else {
                        results.push(format!("{path}: created\n{diff}"));
                    }
                }
                PatchHunk::DeleteFile { path } => {
                    let resolved = super::resolve_path(path)?;
                    let p = Path::new(&resolved);
                    ctx.file_tracker.check_before_edit(p)?;
                    let old_contents = ctx.fs.read_text_file(p).await.unwrap_or_default();
                    tokio::fs::remove_file(p)
                        .await
                        .map_err(|e| format!("failed to delete {path}: {e}"))?;
                    let diff = generate_diff_summary(&old_contents, "");

                    touched_paths.push(path.clone());
                    if diff.is_empty() {
                        results.push(format!("{path}: deleted"));
                    } else {
                        results.push(format!("{path}: deleted\n{diff}"));
                    }
                }
                PatchHunk::UpdateFile { path, chunks } => {
                    let resolved = super::resolve_path(path)?;
                    let p = Path::new(&resolved);
                    let chunk_count = chunks.len();
                    ctx.file_tracker.check_before_edit(p)?;
                    let original = ctx.fs.read_text_file(p).await?;
                    let new_contents = apply_update_chunks(&original, chunks, path)?;
                    let diff_text = generate_diff_summary(&original, &new_contents);
                    ctx.fs.write_text_file(p, &new_contents).await?;
                    ctx.file_tracker.record_read(p);

                    touched_paths.push(path.clone());
                    last_before = original;
                    last_after = new_contents;
                    if diff_text.is_empty() {
                        results.push(format!(
                            "{path}: modified ({chunk_count} hunks)"
                        ));
                    } else {
                        results.push(format!(
                            "{path}: modified ({chunk_count} hunks)\n{diff_text}"
                        ));
                    }
                }
            }
        }

        if results.is_empty() {
            return Ok(ToolOutput::Plain("No changes applied".into()));
        }

        let summary = if touched_paths.len() == 1 {
            let path = &touched_paths[0];
            format!("patched {}", relative_path(path))
        } else {
            format!("patched {} files", touched_paths.len())
        };

        let text = results.join("\n");

        if let Some(path) = touched_paths.first()
            && touched_paths.len() == 1
            && (!last_before.is_empty() || !last_after.is_empty())
        {
            let resolved = super::resolve_path(path)?;
            Ok(ToolOutput::Diff {
                summary,
                path: resolved,
                before: last_before,
                after: last_after,
            })
        } else {
            Ok(ToolOutput::Plain(text))
        }
    }

    pub fn start_header(&self) -> String {
        match parse_apply_patch(&self.patch_text) {
            Ok(hunks) if hunks.len() == 1 => {
                let path = match &hunks[0] {
                    PatchHunk::AddFile { path, .. }
                    | PatchHunk::DeleteFile { path, .. }
                    | PatchHunk::UpdateFile { path, .. } => path,
                };
                format!("patch {}", relative_path(path))
            }
            Ok(hunks) => format!("patch {} files", hunks.len()),
            Err(_) => "patch".into(),
        }
    }
}

super::impl_tool!(
    ApplyPatch,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::GENERAL_SUB
        | super::ToolAudience::INTERPRETER,
    kind = "edit",
);

impl super::ToolInvocation for ApplyPatch {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(ApplyPatch::start_header(self)))
    }
    fn mutable_path(&self) -> Option<&Path> {
        None
    }
    fn permission_scopes(&self) -> super::BoxFuture<'_, Option<super::PermissionScopes>> {
        let paths = self.extract_paths();
        if paths.is_empty() {
            return Box::pin(std::future::ready(None));
        }
        let ctx = crate::types::PermissionContext {
            files: paths.clone(),
            commands: vec![],
            reason: Some("apply patch".into()),
        };
        Box::pin(std::future::ready(Some(
            super::PermissionScopes::multiple_with_context(
                paths
                    .iter()
                    .map(|p| crate::permissions::canonicalize_scope_path(p))
                    .collect(),
                ctx,
            ),
        )))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { ApplyPatch::execute(&self, ctx).await })
    }
}

fn apply_update_chunks(
    original_contents: &str,
    chunks: &[UpdateFileChunk],
    path: &str,
) -> Result<String, String> {
    let mut original_lines: Vec<String> =
        original_contents.split('\n').map(String::from).collect();
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
) -> Result<Vec<(usize, usize, Vec<String>)>, String> {
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index: usize = 0;

    for chunk in chunks {
        if let Some(ctx_line) = &chunk.change_context {
            if let Some(idx) = seek_sequence(
                original_lines,
                std::slice::from_ref(ctx_line),
                line_index,
                false,
            ) {
                line_index = idx + 1;
            } else {
                return Err(format!(
                    "Failed to find context '{ctx_line}' in {path}"
                ));
            }
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

    // Pass 1: exact match
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if lines[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
    }

    // Pass 2: trim-end match
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim_end() != pat.trim_end() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }

    // Pass 3: full-trim match
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim() != pat.trim() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }

    None
}

fn generate_diff_summary(old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut output = String::new();
    let mut line_count = 0;
    let mut old_line = 1usize;
    let mut new_line = 1usize;

    for change in diff.iter_all_changes() {
        if line_count >= DIFF_MAX_LINES {
            output.push_str("... (diff truncated)\n");
            break;
        }

        let content = change.value().trim_end_matches('\n');
        let (prefix, line_num) = match change.tag() {
            ChangeTag::Delete => {
                let num = old_line;
                old_line += 1;
                if content.trim().is_empty() {
                    continue;
                }
                ("-", num)
            }
            ChangeTag::Insert => {
                let num = new_line;
                new_line += 1;
                if content.trim().is_empty() {
                    continue;
                }
                ("+", num)
            }
            ChangeTag::Equal => {
                old_line += 1;
                new_line += 1;
                continue;
            }
        };

        output.push_str(&format!("{line_num}{prefix} {content}\n"));
        line_count += 1;
    }

    output.trim_end().to_string()
}

fn parse_apply_patch(input: &str) -> Result<Vec<PatchHunk>, String> {
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
    use std::fs;

    use tempfile::TempDir;
    use test_case::test_case;

    use crate::AgentMode;
    use crate::tools::test_support::{pre_read, stub_ctx};

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
        let hunks = parse_apply_patch(patch).unwrap();
        assert_eq!(hunks.len(), 1);
        match &hunks[0] {
            PatchHunk::DeleteFile { path } => {
                assert_eq!(path, "old.txt");
            }
            _ => panic!("Expected DeleteFile"),
        }
    }

    #[test]
    fn parse_update_file_simple() {
        let patch =
            "*** Begin Patch\n*** Update File: test.py\n@@\n foo\n-bar\n+baz\n*** End Patch";
        let hunks = parse_apply_patch(patch).unwrap();
        assert_eq!(hunks.len(), 1);
        match &hunks[0] {
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
        let hunks = parse_apply_patch(patch).unwrap();
        match &hunks[0] {
            PatchHunk::UpdateFile { chunks, .. } => {
                assert_eq!(
                    chunks[0].change_context,
                    Some("def my_func():".to_string())
                );
                assert_eq!(chunks[0].old_lines, vec!["    pass"]);
                assert_eq!(chunks[0].new_lines, vec!["    return 42"]);
            }
            _ => panic!("Expected UpdateFile"),
        }
    }

    #[test]
    fn parse_multiple_chunks() {
        let patch = "*** Begin Patch\n*** Update File: test.py\n@@\n foo\n-bar\n+BAR\n@@\n baz\n-qux\n+QUX\n*** End Patch";
        let hunks = parse_apply_patch(patch).unwrap();
        match &hunks[0] {
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
        let hunks = parse_apply_patch(patch).unwrap();
        match &hunks[0] {
            PatchHunk::UpdateFile { chunks, .. } => {
                assert!(chunks[0].is_end_of_file);
            }
            _ => panic!("Expected UpdateFile"),
        }
    }

    #[test]
    fn parse_no_begin_marker_returns_error() {
        let result = parse_apply_patch("random text");
        assert!(result.is_err());
    }

    #[test]
    fn parse_heredoc_wrapper() {
        let patch = "<<'EOF'\n*** Begin Patch\n*** Add File: test.txt\n+hello\n*** End Patch\nEOF";
        let hunks = parse_apply_patch(patch).unwrap();
        assert_eq!(hunks.len(), 1);
    }

    #[test]
    fn parse_update_without_explicit_at() {
        let patch =
            "*** Begin Patch\n*** Update File: file.py\n import foo\n+bar\n*** End Patch";
        let hunks = parse_apply_patch(patch).unwrap();
        match &hunks[0] {
            PatchHunk::UpdateFile { chunks, .. } => {
                assert_eq!(chunks.len(), 1);
                assert!(chunks[0].change_context.is_none());
            }
            _ => panic!("Expected UpdateFile"),
        }
    }

    #[test]
    fn seek_sequence_exact_match() {
        let lines: Vec<String> = vec!["foo", "bar", "baz"].into_iter().map(String::from).collect();
        let pattern: Vec<String> = vec!["bar", "baz"].into_iter().map(String::from).collect();
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(1));
    }

    #[test]
    fn seek_sequence_whitespace_tolerant() {
        let lines: Vec<String> = vec!["foo   ", "bar\t"]
            .into_iter()
            .map(String::from)
            .collect();
        let pattern: Vec<String> = vec!["foo", "bar"].into_iter().map(String::from).collect();
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(0));
    }

    #[test]
    fn seek_sequence_eof_anchor() {
        let lines: Vec<String> = vec!["a", "b", "c", "d"]
            .into_iter()
            .map(String::from)
            .collect();
        let pattern: Vec<String> = vec!["c", "d"].into_iter().map(String::from).collect();
        assert_eq!(seek_sequence(&lines, &pattern, 0, true), Some(2));
    }

    #[test]
    fn diff_summary_compact_format() {
        let old = "line one\nline two\nline three\n";
        let new = "line one\nchanged two\nline three\n";
        let diff = generate_diff_summary(old, new);
        assert!(diff.contains("2- line two"));
        assert!(diff.contains("2+ changed two"));
        assert!(!diff.contains("line one"));
    }

    fn temp_file(dir: &TempDir, name: &str, content: &str) -> String {
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn apply_update_simple() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path = temp_file(&dir, "f.txt", "foo\nbar\n");
        pre_read(&ctx, &path);

        ApplyPatch {
            patch_text:
                "*** Begin Patch\n*** Update File: {path}\n@@\n foo\n-bar\n+baz\n*** End Patch"
                    .replace("{path}", &path),
        }
        .execute(&ctx)
        .await
        .unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "foo\nbaz\n");
    }

    #[tokio::test]
    async fn apply_update_multiple_chunks() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path = temp_file(&dir, "f.txt", "foo\nbar\nbaz\nqux\n");
        pre_read(&ctx, &path);

        ApplyPatch {
            patch_text: "*** Begin Patch\n*** Update File: {path}\n@@\n foo\n-bar\n+BAR\n@@\n baz\n-qux\n+QUX\n*** End Patch"
                .replace("{path}", &path),
        }
        .execute(&ctx)
        .await
        .unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "foo\nBAR\nbaz\nQUX\n"
        );
    }

    #[tokio::test]
    async fn apply_update_with_context_header() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path = temp_file(
            &dir,
            "f.py",
            "class Foo:\n    def bar(self):\n        pass\n    def baz(self):\n        pass\n",
        );
        pre_read(&ctx, &path);

        ApplyPatch {
            patch_text: "*** Begin Patch\n*** Update File: {path}\n@@ def baz(self):\n-        pass\n+        return 42\n*** End Patch"
                .replace("{path}", &path),
        }
        .execute(&ctx)
        .await
        .unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "class Foo:\n    def bar(self):\n        pass\n    def baz(self):\n        return 42\n"
        );
    }

    #[tokio::test]
    async fn apply_add_new_file() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path = dir.path().join("new.txt").to_string_lossy().to_string();

        ApplyPatch {
            patch_text: "*** Begin Patch\n*** Add File: {path}\n+Hello world\n*** End Patch"
                .replace("{path}", &path),
        }
        .execute(&ctx)
        .await
        .unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "Hello world\n");
    }

    #[tokio::test]
    async fn apply_delete_file() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path = temp_file(&dir, "old.txt", "content");

        ApplyPatch {
            patch_text: "*** Begin Patch\n*** Delete File: {path}\n*** End Patch"
                .replace("{path}", &path),
        }
        .execute(&ctx)
        .await
        .unwrap();

        assert!(!Path::new(&path).exists());
    }

    #[tokio::test]
    async fn apply_multi_file_patch() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path_a = temp_file(&dir, "a.txt", "foo\n");
        let path_b = temp_file(&dir, "b.txt", "bar\n");
        pre_read(&ctx, &path_a);
        pre_read(&ctx, &path_b);

        ApplyPatch {
            patch_text: "*** Begin Patch\n*** Update File: {a}\n@@\n-foo\n+FOO\n*** Update File: {b}\n@@\n-bar\n+BAR\n*** End Patch"
                .replace("{a}", &path_a)
                .replace("{b}", &path_b),
        }
        .execute(&ctx)
        .await
        .unwrap();

        assert_eq!(fs::read_to_string(&path_a).unwrap(), "FOO\n");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "BAR\n");
    }

    #[test_case("random text", "Patch must contain *** Begin Patch"; "no_begin_marker")]
    #[test_case("*** Begin Patch\n*** End Patch", "No valid patch directives found"; "empty_patch")]
    #[test_case("*** Begin Patch\n*** Update File: x.txt\n*** End Patch", "has no changes"; "update_no_changes")]
    fn parse_errors(patch: &str, expected: &str) {
        let err = parse_apply_patch(patch).unwrap_err();
        assert!(
            err.contains(expected),
            "expected '{expected}' in error, got '{err}'"
        );
    }

    #[tokio::test]
    async fn apply_update_append_at_eof() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path = temp_file(&dir, "f.txt", "foo\nbar\nbaz\n");
        pre_read(&ctx, &path);

        ApplyPatch {
            patch_text: "*** Begin Patch\n*** Update File: {path}\n@@\n+quux\n*** End of File\n*** End Patch"
                .replace("{path}", &path),
        }
        .execute(&ctx)
        .await
        .unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "foo\nbar\nbaz\nquux\n"
        );
    }

    #[tokio::test]
    async fn apply_fuzzy_whitespace_matching() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path = temp_file(&dir, "f.txt", "foo   \nbar\t\n");
        pre_read(&ctx, &path);

        ApplyPatch {
            patch_text: "*** Begin Patch\n*** Update File: {path}\n@@\n foo\n-bar\n+BAR\n*** End Patch"
                .replace("{path}", &path),
        }
        .execute(&ctx)
        .await
        .unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "foo\nBAR\n"
        );
    }
}
