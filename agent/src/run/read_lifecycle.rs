//! Read lifecycle: classify historical `read` results as Fresh, Stale, or
//! Superseded and replace the stale ones with compact markers, so old file
//! snapshots stop occupying context.
//!
//! - **Stale**: the file was edited/written at a later point (and is no longer
//!   in the active working set) — the model is told to re-read before editing.
//! - **Superseded**: a later read fully contains the same file range; the old
//!   snapshot is redundant.
//!
//! Since 08 the durable home for these markers is a history alteration pass
//! (`compaction::alter`); what remains here is the in-run half, applied to
//! the request view for results the current run produced. Everything older
//! arrives pre-altered from history.
//!
//! Ported from the reference's `agent/read_lifecycle.rs` minus the semantic
//! scorer; superseded reads carry retrieval markers (D.5) naming the hash of
//! the original content, which `retrieve` resolves against history entry
//! sources.

use std::collections::{HashMap, HashSet};

use std::ops::Range;

use animist::history::{ContentBlock, ToolContents};

use crate::history::{Message, ext};

pub(crate) const STALE_MARKER_PREFIX: &str = "[Stale read: ";
pub(crate) const SUPERSEDED_MARKER_PREFIX: &str = "[Superseded read: ";

/// Number of most-recent assistant messages whose edit targets form the "working set".
/// Reads of working-set files are not marked Stale — the model is still actively editing them.
pub(crate) const WORKING_SET_LOOKBACK: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpKind {
    Read,
    Edit,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadState {
    Fresh,
    Stale,
    Superseded,
}

#[derive(Debug)]
struct FileOperation {
    msg_index: usize,
    tool_call_id: String,
    /// The argument string as the model spelled it; kept for user-visible
    /// marker text.
    file_path: String,
    /// [`crate::run::dedup::PathNormalizer`] key: `./a.rs`, `a.rs`, and the
    /// anchored absolute spelling key the same file identically, so a write
    /// under one spelling staleness-marks a read recorded under another.
    file_key: String,
    op_kind: OpKind,
    /// For reads: the 0-indexed line range [start, end). None means full file.
    line_range: Option<Range<usize>>,
}

#[derive(Debug)]
pub struct ReadClassification {
    pub tool_call_id: String,
    pub file_path: String,
    pub state: ReadState,
}

/// Scan history and classify all read operations as Fresh, Stale, or Superseded.
///
/// Files in the active "working set" (edited in the last `WORKING_SET_LOOKBACK`
/// assistant messages) are never marked Stale — the model still needs the read
/// content for its next edit. Reads from the most recent assistant message are
/// never Superseded, to avoid re-read feedback loops.
/// `root` anchors path normalization: `None` collapses only structural
/// aliases (`./a.rs` ≡ `a.rs`); the workspace root also unifies relative
/// and absolute spellings of one file.
pub fn classify_reads(history: &[Message], root: Option<&str>) -> Vec<ReadClassification> {
    let move_rewrites = collect_move_rewrites(history);
    let operations = collect_file_operations(history, &move_rewrites, root);
    let by_file = group_by_file(&operations);
    let (working_set_files, last_assistant_msg) = compute_working_set(&operations);

    let mut classifications = Vec::new();
    for op in &operations {
        if !matches!(op.op_kind, OpKind::Read) {
            continue;
        }
        classifications.push(classify_read(
            op,
            &by_file,
            &working_set_files,
            last_assistant_msg,
        ));
    }

    classifications
}

/// Files whose imports a `move` rewrote, keyed by the call it answered:
/// args alone cannot name them, only the executed result can.
fn collect_move_rewrites(history: &[Message]) -> HashMap<&str, Vec<String>> {
    history
        .iter()
        .filter(|msg| ext::is_user(msg))
        .flat_map(|msg| msg.content.iter())
        .filter_map(|block| {
            let ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                content,
                ..
            } = block
            else {
                return None;
            };
            // The name comes from the result when the agent wrote it, and
            // from the matching call for legacy results.
            if ext::tool_name(block, history) != "move" || *is_error {
                return None;
            }
            let text = ext::tool_content_text(content);
            let files = crate::tools::move_file::rewritten_files(&text);
            (!files.is_empty()).then_some((tool_use_id.as_str(), files))
        })
        .collect()
}

/// Walk assistant tool calls and record one `FileOperation` per file touched.
/// Keys are normalized through the write-root-anchored
/// [`crate::run::dedup::PathNormalizer`]: only the keying is normalized, the
/// original argument spelling is kept for display.
fn collect_file_operations(
    history: &[Message],
    move_rewrites: &HashMap<&str, Vec<String>>,
    root: Option<&str>,
) -> Vec<FileOperation> {
    let normalizer = crate::run::dedup::PathNormalizer::new(root);
    let mut operations: Vec<FileOperation> = Vec::new();

    for (msg_index, msg) in history.iter().enumerate() {
        if !matches!(msg.role, crate::history::Role::Assistant) {
            continue;
        }
        for block in &msg.content {
            let ContentBlock::ToolUse {
                id, name, input, ..
            } = block
            else {
                continue;
            };
            let (op_kind, file_path) = match name.as_str() {
                "read" | "edit" | "multiedit" | "write" => match extract_path(input) {
                    Some(p) => (
                        match name.as_str() {
                            "read" => OpKind::Read,
                            "write" => OpKind::Write,
                            _ => OpKind::Edit,
                        },
                        p,
                    ),
                    None => continue,
                },
                // Every file a patch touches counts as an edit of that file.
                "apply_patch" => {
                    let Some(patch) = input.get("patch_text").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let paths = crate::tools::patch_paths(patch);
                    for file_path in paths {
                        operations.push(FileOperation {
                            msg_index,
                            tool_call_id: id.clone(),
                            file_key: normalizer.normalize(&file_path),
                            file_path,
                            op_kind: OpKind::Edit,
                            line_range: None,
                        });
                    }
                    continue;
                }
                // A move invalidates reads of both the old path (gone) and
                // the new one (content arrived from elsewhere).
                "move" => {
                    let mut paths: Vec<String> = ["source", "destination"]
                        .iter()
                        .filter_map(|key| input.get(*key))
                        .filter_map(|v| v.as_str())
                        .map(String::from)
                        .collect();
                    paths.extend(
                        move_rewrites
                            .get(id.as_str())
                            .map(|files| files.as_slice())
                            .unwrap_or_default()
                            .iter()
                            .cloned(),
                    );
                    for file_path in paths {
                        operations.push(FileOperation {
                            msg_index,
                            tool_call_id: id.clone(),
                            file_key: normalizer.normalize(&file_path),
                            file_path,
                            op_kind: OpKind::Edit,
                            line_range: None,
                        });
                    }
                    continue;
                }
                _ => continue,
            };
            let line_range = if matches!(op_kind, OpKind::Read) {
                extract_line_range(input)
            } else {
                None
            };
            operations.push(FileOperation {
                msg_index,
                tool_call_id: id.clone(),
                file_key: normalizer.normalize(&file_path),
                file_path,
                op_kind,
                line_range,
            });
        }
    }
    operations
}

/// Group operations by normalized file key (preserving order).
fn group_by_file(operations: &[FileOperation]) -> HashMap<&str, Vec<&FileOperation>> {
    let mut map: HashMap<&str, Vec<&FileOperation>> = HashMap::new();
    for op in operations {
        map.entry(op.file_key.as_str()).or_default().push(op);
    }
    map
}

/// Working set: files edited in the last WORKING_SET_LOOKBACK assistant
/// messages that contain file operations, plus the index of the most recent
/// assistant message with file operations.
fn compute_working_set(operations: &[FileOperation]) -> (HashSet<&str>, Option<usize>) {
    let mut assistant_msg_indices: Vec<usize> = operations
        .iter()
        .map(|op| op.msg_index)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    assistant_msg_indices.sort_unstable();
    let recent_assistant_indices: HashSet<usize> = assistant_msg_indices
        .into_iter()
        .rev()
        .take(WORKING_SET_LOOKBACK)
        .collect();
    let working_set_files: HashSet<&str> = operations
        .iter()
        .filter(|op| {
            matches!(op.op_kind, OpKind::Edit | OpKind::Write)
                && recent_assistant_indices.contains(&op.msg_index)
        })
        .map(|op| op.file_key.as_str())
        .collect();

    let last_assistant_msg = operations.iter().map(|op| op.msg_index).max();
    (working_set_files, last_assistant_msg)
}

/// Classify a single read operation as Stale, Superseded, or Fresh.
fn classify_read(
    op: &FileOperation,
    by_file: &HashMap<&str, Vec<&FileOperation>>,
    working_set_files: &HashSet<&str>,
    last_assistant_msg: Option<usize>,
) -> ReadClassification {
    let file_ops = by_file
        .get(op.file_key.as_str())
        .expect("every operation is keyed in by_file");

    let has_later_edit = file_ops.iter().any(|other| {
        other.msg_index > op.msg_index && matches!(other.op_kind, OpKind::Edit | OpKind::Write)
    });

    // Working set protection: the model still needs the read content for
    // subsequent edits of actively-edited files.
    if has_later_edit && !working_set_files.contains(op.file_key.as_str()) {
        return classification(op, ReadState::Stale);
    }

    let has_later_superseding_read = file_ops.iter().any(|other| {
        if other.msg_index <= op.msg_index || !matches!(other.op_kind, OpKind::Read) {
            return false;
        }
        range_contains(other.line_range.as_ref(), op.line_range.as_ref())
    });

    // Don't supersede reads from the most recent turn — premature marking
    // causes re-read feedback loops.
    let is_most_recent = last_assistant_msg.is_some_and(|last| op.msg_index == last);

    if has_later_superseding_read && !is_most_recent {
        return classification(op, ReadState::Superseded);
    }

    classification(op, ReadState::Fresh)
}

fn classification(op: &FileOperation, state: ReadState) -> ReadClassification {
    ReadClassification {
        tool_call_id: op.tool_call_id.clone(),
        file_path: op.file_path.clone(),
        state,
    }
}

/// Replace stale/superseded tool result content with compact markers.
/// Returns total characters removed for observability. Only messages at
/// index `from` on are rewritten: classification needs the whole view, but
/// results before `from` already carry their durable alteration.
fn apply_lifecycle(
    history: &mut [Message],
    classifications: &[ReadClassification],
    from: usize,
) -> usize {
    let stale_ids: HashMap<&str, (ReadState, &str)> = classifications
        .iter()
        .filter(|c| !matches!(c.state, ReadState::Fresh))
        .map(|c| (c.tool_call_id.as_str(), (c.state, c.file_path.as_str())))
        .collect();

    if stale_ids.is_empty() {
        return 0;
    }

    let mut total_removed = 0;
    for (index, msg) in history.iter_mut().enumerate() {
        if index < from || !ext::is_user(msg) {
            continue;
        }
        for block in msg.content.iter_mut() {
            let ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                content,
                ..
            } = block
            else {
                continue;
            };
            if *is_error {
                continue;
            }
            let Some(&(state, file_path)) = stale_ids.get(tool_use_id.as_str()) else {
                continue;
            };
            let original = ext::tool_content_text(content);
            let old_len = original.len();
            let original_lines = original.lines().count();
            let mut marker = match state {
                ReadState::Stale => {
                    // Actionable stale marker: retrieving old content would be
                    // misleading; the model needs current content.
                    if old_len > STALE_MARKER_PREFIX.len() + 60 {
                        format!(
                            "{STALE_MARKER_PREFIX}{file_path} was modified after this read. Re-read the file with the read tool before editing. {old_len} chars removed]"
                        )
                    } else {
                        format!(
                            "{STALE_MARKER_PREFIX}{file_path} was modified after this read. Re-read the file with the read tool before editing.]"
                        )
                    }
                }
                ReadState::Superseded => {
                    if old_len > SUPERSEDED_MARKER_PREFIX.len() + 60 {
                        format!(
                            "{SUPERSEDED_MARKER_PREFIX}{file_path} was re-read later. {old_len} chars removed]"
                        )
                    } else {
                        format!("{SUPERSEDED_MARKER_PREFIX}{file_path} was re-read later.]")
                    }
                }
                ReadState::Fresh => continue,
            };
            // Only superseded reads get a retrieval marker: their content is
            // still valid (a newer read exists). Retrieving stale content
            // would mislead the model into editing an outdated file state.
            if matches!(state, ReadState::Superseded) {
                marker.push_str(&crate::compression::retrieval_marker(
                    original_lines,
                    1,
                    &crate::compression::content_hash(&original),
                ));
            }
            total_removed += old_len.saturating_sub(marker.len());
            *content = ToolContents::from(marker);
        }
    }

    total_removed
}

/// Run read lifecycle management over a request view, returning total chars
/// removed. Callers pass the request-only copy: committed history is
/// untouched. `root` anchors the path normalizer at the workspace write root
/// so aliased spellings key one file identically; display markers keep the
/// spelling the model used.
#[cfg(test)]
pub fn apply_to_request(history: &mut [Message], root: Option<&str>) -> usize {
    apply_to_request_from(history, root, 0)
}

/// The in-run half of [`apply_to_request`]: only results the current run
/// produced (`from` is where the run started in the view) are rewritten;
/// everything older arrived pre-altered from history.
pub fn apply_to_request_from(history: &mut [Message], root: Option<&str>, from: usize) -> usize {
    let classifications = classify_reads(history, root);
    apply_lifecycle(history, &classifications, from)
}

fn extract_path(input: &serde_json::Value) -> Option<String> {
    input.get("path").and_then(|v| v.as_str()).map(String::from)
}

/// Extract the 0-indexed line range [start, end) from read tool input.
/// Returns None if no offset specified (meaning full file read).
fn extract_line_range(input: &serde_json::Value) -> Option<Range<usize>> {
    let offset = input.get("offset").and_then(|v| v.as_u64())? as usize;
    let start = offset.saturating_sub(1); // offset is 1-indexed
    let limit = input
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|l| l as usize);
    let end = limit.map_or(usize::MAX, |l| start + l);
    Some(start..end)
}

/// Check if range `outer` fully contains range `inner`. None means full file.
/// A full-file read contains any other read; a partial read only contains
/// `inner` if it starts at or before it and ends at or after it.
fn range_contains(outer: Option<&Range<usize>>, inner: Option<&Range<usize>>) -> bool {
    match (outer, inner) {
        (None, _) => true,
        (_, None) => false,
        (Some(outer), Some(inner)) => outer.start <= inner.start && inner.end <= outer.end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::ext;
    use serde_json::json;

    fn tool_use_msg(id: &str, name: &str, input: serde_json::Value) -> Message {
        Message {
            role: crate::history::Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
                thought_signature: None,
            }],
            display_text: None,
            kind: crate::history::MessageKind::Turn,
        }
    }

    fn tool_result_msg(id: &str, content: &str) -> Message {
        tool_result_of_msg(id, "read", content)
    }

    fn tool_result_of_msg(id: &str, name: &str, content: &str) -> Message {
        ext::observation(vec![ext::tool_result(id, Some(name), content, false)])
    }

    fn user_msg(text: &str) -> Message {
        ext::user_text(text)
    }

    /// Generate gap assistant turns (reads of unrelated files) to push earlier
    /// edits outside the working set lookback window.
    fn gap_assistant_turns(n: usize) -> Vec<Message> {
        (0..n)
            .flat_map(|i| {
                vec![
                    tool_use_msg(
                        &format!("gap_{i}"),
                        "read",
                        json!({"path": format!("/other/{i}.rs")}),
                    ),
                    tool_result_msg(&format!("gap_{i}"), "gap content"),
                ]
            })
            .collect()
    }

    fn find_by_id<'a>(
        classifications: &'a [ReadClassification],
        id: &str,
    ) -> &'a ReadClassification {
        classifications
            .iter()
            .find(|c| c.tool_call_id == id)
            .unwrap_or_else(|| panic!("no classification for {id}"))
    }

    #[test]
    fn fresh_read_no_edits() {
        let messages = vec![
            user_msg("read the file"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "line 1\nline 2\nline 3"),
        ];
        let classifications = classify_reads(&messages, None);
        assert_eq!(classifications.len(), 1);
        assert_eq!(classifications[0].state, ReadState::Fresh);
        assert_eq!(classifications[0].file_path, "/src/main.rs");
    }

    #[test]
    fn move_marks_reads_of_source_destination_and_rewritten_files_stale() {
        let mut messages = vec![
            user_msg("read it"),
            tool_use_msg("t1", "read", json!({"path": "src/main.rs"})),
            tool_result_msg("t1", "line 1\nline 2\nline 3"),
            tool_use_msg("t2", "read", json!({"path": "src/other.rs"})),
            tool_result_msg("t2", "content"),
            tool_use_msg(
                "t3",
                "move",
                json!({"source": "/src/util/old.rs", "destination": "/src/util/new.rs"}),
            ),
            tool_result_of_msg(
                "t3",
                "move",
                "moved src/util/old.rs -> src/util/new.rs\nupdated imports in 1 file(s)\n  src/main.rs: 1 reference(s)",
            ),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let classifications = classify_reads(&messages, None);
        assert_eq!(find_by_id(&classifications, "t1").state, ReadState::Stale);
        assert_eq!(find_by_id(&classifications, "t2").state, ReadState::Fresh);
    }

    #[test]
    fn stale_read_after_edit() {
        let mut messages = vec![
            user_msg("read it"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "line 1\nline 2\nline 3"),
            user_msg("edit it"),
            tool_use_msg(
                "t2",
                "edit",
                json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"}),
            ),
            tool_result_msg("t2", "ok"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let cls = classify_reads(&messages, None);
        assert_eq!(find_by_id(&cls, "t1").state, ReadState::Stale);
    }

    #[test]
    fn superseded_read_after_later_read() {
        let messages = vec![
            user_msg("read it"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "old content"),
            user_msg("read it again"),
            tool_use_msg("t2", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t2", "new content"),
        ];
        let classifications = classify_reads(&messages, None);
        assert_eq!(classifications.len(), 2);
        assert_eq!(classifications[0].state, ReadState::Superseded);
        assert_eq!(classifications[0].tool_call_id, "t1");
        assert_eq!(classifications[1].state, ReadState::Fresh);
        assert_eq!(classifications[1].tool_call_id, "t2");
    }

    #[test]
    fn stale_takes_precedence_over_superseded() {
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "content"),
            user_msg("edit"),
            tool_use_msg(
                "t2",
                "edit",
                json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"}),
            ),
            tool_result_msg("t2", "ok"),
            user_msg("read again"),
            tool_use_msg("t3", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t3", "new content"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let classifications = classify_reads(&messages, None);
        assert_eq!(find_by_id(&classifications, "t1").state, ReadState::Stale);
        assert_eq!(find_by_id(&classifications, "t3").state, ReadState::Fresh);
    }

    #[test]
    fn different_files_dont_interfere() {
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/a.rs"})),
            tool_result_msg("t1", "a content"),
            tool_use_msg("t2", "read", json!({"path": "/src/b.rs"})),
            tool_result_msg("t2", "b content"),
            user_msg("edit a"),
            tool_use_msg(
                "t3",
                "edit",
                json!({"path": "/src/a.rs", "old_string": "x", "new_string": "y"}),
            ),
            tool_result_msg("t3", "ok"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let classifications = classify_reads(&messages, None);
        assert_eq!(find_by_id(&classifications, "t1").state, ReadState::Stale);
        assert_eq!(find_by_id(&classifications, "t2").state, ReadState::Fresh);
    }

    #[test]
    fn write_and_multiedit_make_read_stale() {
        for tool in ["write", "multiedit"] {
            let mut messages = vec![
                user_msg("read"),
                tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
                tool_result_msg("t1", "content"),
                user_msg("modify"),
                tool_use_msg("t2", tool, json!({"path": "/src/main.rs"})),
                tool_result_msg("t2", "ok"),
            ];
            messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
            let cls = classify_reads(&messages, None);
            assert_eq!(
                find_by_id(&cls, "t1").state,
                ReadState::Stale,
                "{tool} must stale-date an earlier read"
            );
        }
    }

    #[test]
    fn apply_patch_stales_only_the_files_it_touches() {
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/a.rs"})),
            tool_result_msg("t1", "a content"),
            tool_use_msg("t2", "read", json!({"path": "/src/b.rs"})),
            tool_result_msg("t2", "b content"),
            user_msg("patch a"),
            tool_use_msg(
                "t3",
                "apply_patch",
                json!({"patch_text": "*** Begin Patch\n*** Update File: /src/a.rs\n@@\n-x\n+y\n*** End Patch"}),
            ),
            tool_result_msg("t3", "ok"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let cls = classify_reads(&messages, None);
        assert_eq!(find_by_id(&cls, "t1").state, ReadState::Stale);
        assert_eq!(find_by_id(&cls, "t2").state, ReadState::Fresh);
    }

    #[test]
    fn apply_lifecycle_replaces_stale_content() {
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg(
                "t1",
                "a long line of content that should be replaced with something even longer to ensure the marker is shorter than the original content being replaced in the tool result",
            ),
            user_msg("edit"),
            tool_use_msg(
                "t2",
                "edit",
                json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"}),
            ),
            tool_result_msg("t2", "ok"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let removed = apply_to_request(&mut messages, None);
        assert!(removed > 0);
        let ContentBlock::ToolResult {
            content: result, ..
        } = &messages[2].content[0]
        else {
            panic!("tool result block");
        };
        let text = ext::tool_content_text(result);
        assert!(text.starts_with(STALE_MARKER_PREFIX));
        assert!(text.contains("/src/main.rs"));
        assert!(text.contains("was modified after this read"));
        assert!(text.contains("Re-read the file"));
    }

    #[test]
    fn apply_lifecycle_preserves_fresh_content() {
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "fresh content"),
        ];
        let removed = apply_to_request(&mut messages, None);
        assert_eq!(removed, 0);
        let ContentBlock::ToolResult {
            content: result, ..
        } = &messages[2].content[0]
        else {
            panic!("tool result block");
        };
        assert_eq!(ext::tool_content_text(result), "fresh content");
    }

    #[test]
    fn superseded_read_gets_retrieval_marker_naming_the_originals_hash() {
        let original_text = "a long superseded line of content that will be replaced by a much \
                             shorter marker but stays recoverable through the entry source \
                             even after several additional sentences pad the original well past \
                             any marker the lifecycle pass could ever produce";
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", original_text),
            tool_use_msg("t2", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t2", original_text),
        ];
        let removed = apply_to_request(&mut messages, None);
        assert!(removed > 0);
        let ContentBlock::ToolResult {
            content: result, ..
        } = &messages[2].content[0]
        else {
            panic!("tool result block");
        };
        let text = ext::tool_content_text(result);
        assert!(text.starts_with(SUPERSEDED_MARKER_PREFIX));
        let hash_start = text
            .find("Retrieve original: hash=")
            .expect("retrieval marker")
            + "Retrieve original: hash=".len();
        let hash = text[hash_start..].trim_end_matches(']').to_string();
        assert_eq!(
            hash,
            crate::compression::content_hash(original_text),
            "the marker names the hash of the original content, which retrieve \
             resolves against history entry sources"
        );
    }

    #[test]
    fn stale_read_gets_no_retrieval_marker() {
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg(
                "t1",
                "a long line of content that should be replaced with something even longer to \
                 trigger the stale marker path",
            ),
            user_msg("edit"),
            tool_use_msg(
                "t2",
                "edit",
                json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"}),
            ),
            tool_result_msg("t2", "ok"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        apply_to_request(&mut messages, None);
        let ContentBlock::ToolResult {
            content: result, ..
        } = &messages[2].content[0]
        else {
            panic!("tool result block");
        };
        let text = ext::tool_content_text(result);
        assert!(text.starts_with(STALE_MARKER_PREFIX));
        assert!(
            !text.contains("Retrieve original"),
            "stale content must not be retrievable"
        );
    }

    #[test]
    fn partial_reads_different_offsets_not_superseded() {
        let messages = vec![
            user_msg("read top"),
            tool_use_msg(
                "t1",
                "read",
                json!({"path": "/src/main.rs", "offset": 1, "limit": 50}),
            ),
            tool_result_msg("t1", "lines 1-50"),
            user_msg("read bottom"),
            tool_use_msg(
                "t2",
                "read",
                json!({"path": "/src/main.rs", "offset": 51, "limit": 50}),
            ),
            tool_result_msg("t2", "lines 51-100"),
        ];
        let classifications = classify_reads(&messages, None);
        assert_eq!(
            classifications[0].state,
            ReadState::Fresh,
            "non-overlapping"
        );
        assert_eq!(
            classifications[1].state,
            ReadState::Fresh,
            "non-overlapping"
        );
    }

    #[test]
    fn full_file_read_supersedes_partial() {
        let messages = vec![
            user_msg("read partial"),
            tool_use_msg(
                "t1",
                "read",
                json!({"path": "/src/main.rs", "offset": 1, "limit": 50}),
            ),
            tool_result_msg("t1", "lines 1-50"),
            user_msg("read full"),
            tool_use_msg("t2", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t2", "all content"),
        ];
        let classifications = classify_reads(&messages, None);
        assert_eq!(
            classifications[0].state,
            ReadState::Superseded,
            "covered by full file read"
        );
        assert_eq!(classifications[1].state, ReadState::Fresh);
    }

    #[test]
    fn partially_overlapping_partial_reads_not_superseded() {
        let messages = vec![
            user_msg("read"),
            tool_use_msg(
                "t1",
                "read",
                json!({"path": "/src/main.rs", "offset": 1, "limit": 100}),
            ),
            tool_result_msg("t1", "lines 1-100"),
            user_msg("read again"),
            tool_use_msg(
                "t2",
                "read",
                json!({"path": "/src/main.rs", "offset": 50, "limit": 100}),
            ),
            tool_result_msg("t2", "lines 50-150"),
        ];
        let classifications = classify_reads(&messages, None);
        assert_eq!(
            classifications[0].state,
            ReadState::Fresh,
            "t2 doesn't contain t1"
        );
        assert_eq!(classifications[1].state, ReadState::Fresh);
    }

    #[test]
    fn adjacent_partial_reads_not_superseded() {
        let messages = vec![
            user_msg("read"),
            tool_use_msg(
                "t1",
                "read",
                json!({"path": "/src/main.rs", "offset": 1, "limit": 50}),
            ),
            tool_result_msg("t1", "lines 1-50"),
            user_msg("read next"),
            tool_use_msg(
                "t2",
                "read",
                json!({"path": "/src/main.rs", "offset": 51, "limit": 50}),
            ),
            tool_result_msg("t2", "lines 51-100"),
        ];
        let classifications = classify_reads(&messages, None);
        assert_eq!(classifications[0].state, ReadState::Fresh, "adjacent");
        assert_eq!(classifications[1].state, ReadState::Fresh, "adjacent");
    }

    #[test]
    fn working_set_protects_recently_edited_file() {
        // read→edit with no gap: the edit is in the most recent assistant
        // message, so the file is in the working set and the read stays Fresh.
        let messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "content"),
            user_msg("edit"),
            tool_use_msg(
                "t2",
                "edit",
                json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"}),
            ),
            tool_result_msg("t2", "ok"),
        ];
        let cls = classify_reads(&messages, None);
        assert_eq!(
            find_by_id(&cls, "t1").state,
            ReadState::Fresh,
            "read protected by working set"
        );
    }

    #[test]
    fn working_set_expires_after_lookback() {
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "content"),
            user_msg("edit"),
            tool_use_msg(
                "t2",
                "edit",
                json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"}),
            ),
            tool_result_msg("t2", "ok"),
        ];
        // 2 original + (LOOKBACK-2) gap = LOOKBACK total assistant msgs → in the set.
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK - 2));
        let cls = classify_reads(&messages, None);
        assert_eq!(
            find_by_id(&cls, "t1").state,
            ReadState::Fresh,
            "still in working set"
        );

        messages.extend(gap_assistant_turns(2));
        let cls = classify_reads(&messages, None);
        assert_eq!(
            find_by_id(&cls, "t1").state,
            ReadState::Stale,
            "expired from working set"
        );
    }

    #[test]
    fn error_results_are_never_marked() {
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            ext::observation(vec![ext::tool_result(
                "t1",
                Some("read"),
                "a long failing read result that would otherwise be long enough for the marker path to trigger replacement here",
                true,
            )]),
            user_msg("edit"),
            tool_use_msg(
                "t2",
                "edit",
                json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"}),
            ),
            tool_result_msg("t2", "ok"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let removed = apply_to_request(&mut messages, None);
        assert_eq!(removed, 0, "error results are left untouched");
    }

    #[test]
    fn no_reads_produces_empty_classifications() {
        let messages = vec![user_msg("hello"), ext::assistant_text("hi")];
        assert!(classify_reads(&messages, None).is_empty());
    }

    #[test]
    fn path_spelling_aliases_key_the_same_file() {
        // A write under one spelling staleness-marks a read recorded under
        // another: both `./a.rs` and `a.rs` normalize to the same key.
        for (read_path, write_path) in [("./a.rs", "a.rs"), ("a.rs", "./a.rs")] {
            let mut messages = vec![
                user_msg("read"),
                tool_use_msg("t1", "read", json!({"path": read_path})),
                tool_result_msg("t1", "content"),
                user_msg("write"),
                tool_use_msg("t2", "write", json!({"path": write_path, "content": "x"})),
                tool_result_of_msg("t2", "write", "ok"),
            ];
            messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
            let cls = classify_reads(&messages, None);
            assert_eq!(
                find_by_id(&cls, "t1").state,
                ReadState::Stale,
                "{read_path} read must stale-date from the {write_path} write"
            );
        }
    }

    #[test]
    fn root_anchored_keying_unifies_relative_and_absolute_spellings() {
        // Anchored at the write root, a relative read and an absolute write
        // of the same file key identically; rootless keying cannot prove
        // them equal and stays fresh.
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "src/a.rs"})),
            tool_result_msg("t1", "content"),
            user_msg("edit"),
            tool_use_msg(
                "t2",
                "edit",
                json!({"path": "/ws/src/a.rs", "old_string": "x", "new_string": "y"}),
            ),
            tool_result_msg("t2", "ok"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let anchored = classify_reads(&messages, Some("/ws"));
        assert_eq!(find_by_id(&anchored, "t1").state, ReadState::Stale);
        let rootless = classify_reads(&messages, None);
        assert_eq!(find_by_id(&rootless, "t1").state, ReadState::Fresh);
    }

    #[test]
    fn stale_marker_keeps_the_original_path_spelling() {
        // Keying is normalized, but the user-visible marker names the path
        // the way the model spelled it when it read the file.
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "./src/a.rs"})),
            tool_result_msg(
                "t1",
                "a long read result with enough content in it to be replaced by the stale marker",
            ),
            user_msg("edit"),
            tool_use_msg(
                "t2",
                "edit",
                json!({"path": "src/a.rs", "old_string": "x", "new_string": "y"}),
            ),
            tool_result_msg("t2", "ok"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        apply_to_request(&mut messages, None);
        let ContentBlock::ToolResult {
            content: result, ..
        } = &messages[2].content[0]
        else {
            panic!("tool result block");
        };
        let text = ext::tool_content_text(result);
        assert!(text.starts_with(STALE_MARKER_PREFIX), "{text}");
        assert!(
            text.contains("./src/a.rs"),
            "original spelling kept in marker: {text}"
        );
    }
}
