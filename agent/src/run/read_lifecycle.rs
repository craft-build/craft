//! Read lifecycle: classify historical `read` results as Fresh, Stale, or
//! Superseded and replace the stale ones with compact markers in the request
//! view, so old file snapshots stop occupying context.
//!
//! - **Stale**: the file was edited/written at a later point (and is no longer
//!   in the active working set) — the model is told to re-read before editing.
//! - **Superseded**: a later read fully contains the same file range; the old
//!   snapshot is redundant.
//!
//! Markers ride the request only: committed history, events, and the dedup
//! cache keep the raw results, mirroring request-view compression.
//!
//! Ported from the reference's `agent/read_lifecycle.rs` minus the semantic
//! scorer; superseded reads carry compression-store retrieval markers (D.5)
//! pointing at the original content held by the shared store.

use std::collections::{HashMap, HashSet};

use std::ops::Range;

use crate::compression::store::SharedCompressionStore;
use crate::history::{AssistantContent, Message, ToolResultContent, UserContent};

const STALE_MARKER_PREFIX: &str = "[Stale read: ";
const SUPERSEDED_MARKER_PREFIX: &str = "[Superseded read: ";

/// Number of most-recent assistant messages whose edit targets form the "working set".
/// Reads of working-set files are not marked Stale — the model is still actively editing them.
const WORKING_SET_LOOKBACK: usize = 4;

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
    file_path: String,
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
pub fn classify_reads(history: &[Message]) -> Vec<ReadClassification> {
    let mut operations: Vec<FileOperation> = Vec::new();

    for (msg_index, msg) in history.iter().enumerate() {
        let Message::Assistant { content } = msg else {
            continue;
        };
        for block in content {
            let AssistantContent::ToolCall(call) = block else {
                continue;
            };
            let (op_kind, file_path) = match call.function.name.as_str() {
                "read" | "edit" | "multiedit" | "write" => {
                    match extract_path(&call.function.arguments) {
                        Some(p) => (
                            match call.function.name.as_str() {
                                "read" => OpKind::Read,
                                "write" => OpKind::Write,
                                _ => OpKind::Edit,
                            },
                            p,
                        ),
                        None => continue,
                    }
                }
                _ => continue,
            };
            let line_range = if matches!(op_kind, OpKind::Read) {
                extract_line_range(&call.function.arguments)
            } else {
                None
            };
            operations.push(FileOperation {
                msg_index,
                tool_call_id: call.id.clone(),
                file_path,
                op_kind,
                line_range,
            });
        }
    }

    let by_file: HashMap<&str, Vec<&FileOperation>> = {
        let mut map: HashMap<&str, Vec<&FileOperation>> = HashMap::new();
        for op in &operations {
            map.entry(op.file_path.as_str()).or_default().push(op);
        }
        map
    };

    // Working set: files edited in the last WORKING_SET_LOOKBACK assistant
    // messages that contain file operations.
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
        .map(|op| op.file_path.as_str())
        .collect();

    let last_assistant_msg = operations.iter().map(|op| op.msg_index).max();

    let mut classifications = Vec::new();
    for op in &operations {
        if !matches!(op.op_kind, OpKind::Read) {
            continue;
        }
        let file_ops = by_file
            .get(op.file_path.as_str())
            .expect("every operation is keyed in by_file");

        let has_later_edit = file_ops.iter().any(|other| {
            other.msg_index > op.msg_index && matches!(other.op_kind, OpKind::Edit | OpKind::Write)
        });

        // Working set protection: the model still needs the read content for
        // subsequent edits of actively-edited files.
        if has_later_edit && !working_set_files.contains(op.file_path.as_str()) {
            classifications.push(classification(op, ReadState::Stale));
            continue;
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
            classifications.push(classification(op, ReadState::Superseded));
            continue;
        }

        classifications.push(classification(op, ReadState::Fresh));
    }

    classifications
}

fn classification(op: &FileOperation, state: ReadState) -> ReadClassification {
    ReadClassification {
        tool_call_id: op.tool_call_id.clone(),
        file_path: op.file_path.clone(),
        state,
    }
}

/// Replace stale/superseded tool result content with compact markers.
/// Returns total characters removed for observability.
fn apply_lifecycle(
    history: &mut [Message],
    classifications: &[ReadClassification],
    store: Option<&SharedCompressionStore>,
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
    for msg in history.iter_mut() {
        let Message::User { content } = msg else {
            continue;
        };
        for block in content.iter_mut() {
            let UserContent::ToolResult(result) = block else {
                continue;
            };
            if result.is_error {
                continue;
            }
            let Some(&(state, file_path)) = stale_ids.get(result.call.as_str()) else {
                continue;
            };
            let original: String = result.content.iter().map(|c| c.to_text()).collect();
            let old_len = original.len();
            let original_lines = original.lines().count();
            let hash =
                store.and_then(|store| store.lock().ok().map(|mut guard| guard.put(&original)));
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
            if matches!(state, ReadState::Superseded)
                && let Some(hash) = hash
            {
                marker.push_str(&crate::compression::store::retrieval_marker(
                    original_lines,
                    1,
                    &hash,
                ));
            }
            total_removed += old_len.saturating_sub(marker.len());
            result.content = vec![ToolResultContent::text(marker)];
        }
    }

    total_removed
}

/// Run read lifecycle management over a request view, returning total chars
/// removed. Callers pass the request-only copy: committed history is untouched.
pub fn apply_to_request(history: &mut [Message], store: Option<&SharedCompressionStore>) -> usize {
    let classifications = classify_reads(history);
    apply_lifecycle(history, &classifications, store)
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
    use crate::history::{AssistantContent, ToolCall, ToolResult};
    use serde_json::json;

    fn tool_use_msg(id: &str, name: &str, input: serde_json::Value) -> Message {
        Message::Assistant {
            content: vec![AssistantContent::ToolCall(ToolCall::new(id, name, input))],
        }
    }

    fn tool_result_msg(id: &str, content: &str) -> Message {
        Message::User {
            content: vec![UserContent::ToolResult(ToolResult::text(
                id, "read", content,
            ))],
        }
    }

    fn user_msg(text: &str) -> Message {
        Message::user(text)
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
        let classifications = classify_reads(&messages);
        assert_eq!(classifications.len(), 1);
        assert_eq!(classifications[0].state, ReadState::Fresh);
        assert_eq!(classifications[0].file_path, "/src/main.rs");
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
        let cls = classify_reads(&messages);
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
        let classifications = classify_reads(&messages);
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
        let classifications = classify_reads(&messages);
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
        let classifications = classify_reads(&messages);
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
            let cls = classify_reads(&messages);
            assert_eq!(
                find_by_id(&cls, "t1").state,
                ReadState::Stale,
                "{tool} must stale-date an earlier read"
            );
        }
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
        let Message::User { content } = &messages[2] else {
            panic!("tool result message");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result block");
        };
        let text = result.content[0].to_text();
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
        let Message::User { content } = &messages[2] else {
            panic!("tool result message");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result block");
        };
        assert_eq!(result.content[0].to_text(), "fresh content");
    }

    #[test]
    fn superseded_read_gets_retrieval_marker_and_store_roundtrip() {
        let original_text = "a long superseded line of content that will be replaced by a much \
                             shorter marker but stays recoverable through the compression store \
                             even after several additional sentences pad the original well past \
                             any marker the lifecycle pass could ever produce";
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", original_text),
            tool_use_msg("t2", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t2", original_text),
        ];
        let store = crate::compression::store::shared_store();
        let removed = apply_to_request(&mut messages, Some(&store));
        assert!(removed > 0);
        let Message::User { content } = &messages[2] else {
            panic!("tool result message");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result block");
        };
        let text = result.content[0].to_text();
        assert!(text.starts_with(SUPERSEDED_MARKER_PREFIX));
        let hash_start = text
            .find("Retrieve original: hash=")
            .expect("retrieval marker")
            + "Retrieve original: hash=".len();
        let hash = text[hash_start..].trim_end_matches(']').to_string();
        assert!(
            store
                .lock()
                .unwrap()
                .get(&hash)
                .is_some_and(|r| r == original_text),
            "the stored original must round-trip through the hash in the marker"
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
        let store = crate::compression::store::shared_store();
        apply_to_request(&mut messages, Some(&store));
        let Message::User { content } = &messages[2] else {
            panic!("tool result message");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result block");
        };
        let text = result.content[0].to_text();
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
        let classifications = classify_reads(&messages);
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
        let classifications = classify_reads(&messages);
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
        let classifications = classify_reads(&messages);
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
        let classifications = classify_reads(&messages);
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
        let cls = classify_reads(&messages);
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
        let cls = classify_reads(&messages);
        assert_eq!(
            find_by_id(&cls, "t1").state,
            ReadState::Fresh,
            "still in working set"
        );

        messages.extend(gap_assistant_turns(2));
        let cls = classify_reads(&messages);
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
            Message::User {
                content: vec![UserContent::ToolResult(crate::history::ToolResult {
                    call: "t1".into(),
                    name: "read".into(),
                    content: vec![crate::history::ToolResultContent::text(
                        "a long failing read result that would otherwise be long enough for the marker path to trigger replacement here",
                    )],
                    is_error: true,
                })],
            },
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
        let messages = vec![user_msg("hello"), Message::assistant("hi")];
        assert!(classify_reads(&messages).is_empty());
    }
}
