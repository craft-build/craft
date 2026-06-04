use std::collections::HashMap;

use std::ops::Range;

use craft_providers::{ContentBlock, Message, Role};
use tracing::info;

use super::history::History;

const STALE_MARKER_PREFIX: &str = "[Stale read: ";
const SUPERSEDED_MARKER_PREFIX: &str = "[Superseded read: ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OpKind {
    Read,
    Edit,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadState {
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
pub(super) struct ReadClassification {
    pub tool_call_id: String,
    pub file_path: String,
    pub state: ReadState,
}

/// Scan history and classify all read operations as Fresh, Stale, or Superseded.
///
/// - **Stale**: a read where the same file was edited/written at a later point
/// - **Superseded**: a read where a later read fully contains the same file range
/// - **Fresh**: neither of the above
pub(super) fn classify_reads(history: &[Message]) -> Vec<ReadClassification> {
    let mut operations: Vec<FileOperation> = Vec::new();

    for (msg_index, msg) in history.iter().enumerate() {
        if matches!(msg.role, Role::Assistant) {
            for (id, name, input) in msg.tool_uses() {
                let (op_kind, file_path) = match name {
                    "read" => match extract_path(input) {
                        Some(p) => (OpKind::Read, p),
                        None => continue,
                    },
                    "edit" | "multiedit" => match extract_path(input) {
                        Some(p) => (OpKind::Edit, p),
                        None => continue,
                    },
                    "write" => match extract_path(input) {
                        Some(p) => (OpKind::Write, p),
                        None => continue,
                    },
                    _ => continue,
                };
                let line_range = if matches!(op_kind, OpKind::Read) {
                    extract_line_range(input)
                } else {
                    None
                };
                operations.push(FileOperation {
                    msg_index,
                    tool_call_id: id.to_owned(),
                    file_path,
                    op_kind,
                    line_range,
                });
            }
        }
    }

    let reads: Vec<&FileOperation> = operations
        .iter()
        .filter(|op| matches!(op.op_kind, OpKind::Read))
        .collect();

    let by_file: HashMap<&str, Vec<&FileOperation>> = {
        let mut map: HashMap<&str, Vec<&FileOperation>> = HashMap::new();
        for op in &operations {
            map.entry(op.file_path.as_str()).or_default().push(op);
        }
        map
    };

    let mut classifications = Vec::new();

    for read_op in &reads {
        let file_ops = match by_file.get(read_op.file_path.as_str()) {
            Some(ops) => ops,
            None => {
                classifications.push(ReadClassification {
                    tool_call_id: read_op.tool_call_id.clone(),
                    file_path: read_op.file_path.clone(),
                    state: ReadState::Fresh,
                });
                continue;
            }
        };

        let has_later_edit = file_ops.iter().any(|op| {
            op.msg_index > read_op.msg_index
                && matches!(op.op_kind, OpKind::Edit | OpKind::Write)
        });

        if has_later_edit {
            classifications.push(ReadClassification {
                tool_call_id: read_op.tool_call_id.clone(),
                file_path: read_op.file_path.clone(),
                state: ReadState::Stale,
            });
            continue;
        }

        let has_later_superseding_read = file_ops.iter().any(|op| {
            if op.msg_index <= read_op.msg_index || !matches!(op.op_kind, OpKind::Read) {
                return false;
            }
            range_contains(op.line_range.as_ref(), read_op.line_range.as_ref())
        });

        if has_later_superseding_read {
            classifications.push(ReadClassification {
                tool_call_id: read_op.tool_call_id.clone(),
                file_path: read_op.file_path.clone(),
                state: ReadState::Superseded,
            });
            continue;
        }

        classifications.push(ReadClassification {
            tool_call_id: read_op.tool_call_id.clone(),
            file_path: read_op.file_path.clone(),
            state: ReadState::Fresh,
        });
    }

    classifications
}

/// Replace stale/superseded tool result content blocks with compact markers.
/// Returns total characters removed for observability.
pub(super) fn apply_lifecycle(history: &mut [Message], classifications: &[ReadClassification]) -> usize {
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
        if !matches!(msg.role, Role::User) {
            continue;
        }
        for block in &mut msg.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: false,
            } = block
                && let Some((state, file_path)) = stale_ids.get(tool_use_id.as_str())
            {
                let (state, file_path) = (*state, *file_path);
                    let old_len = content.len();
                    let marker = match state {
                        ReadState::Stale => {
                            if old_len > STALE_MARKER_PREFIX.len() + 60 {
                                format!("{STALE_MARKER_PREFIX}{file_path} was modified after this read. {old_len} chars removed]")
                            } else {
                                format!("{STALE_MARKER_PREFIX}{file_path} was modified after this read.]")
                            }
                        }
                        ReadState::Superseded => {
                            if old_len > SUPERSEDED_MARKER_PREFIX.len() + 60 {
                                format!("{SUPERSEDED_MARKER_PREFIX}{file_path} was re-read later. {old_len} chars removed]")
                            } else {
                                format!("{SUPERSEDED_MARKER_PREFIX}{file_path} was re-read later.]")
                            }
                        }
                        ReadState::Fresh => unreachable!(),
                    };
                    total_removed += old_len.saturating_sub(marker.len());
                    *content = marker;
                }
            }
        }

    if total_removed > 0 {
        info!(total_chars_removed = total_removed, "read lifecycle applied");
    }

    total_removed
}

/// Run read lifecycle management on a History, returning total chars removed.
pub(super) fn run_lifecycle(history: &mut History) -> usize {
    let messages = history.as_slice();
    let classifications = classify_reads(messages);
    let msgs = history.as_mut_slice();
    apply_lifecycle(msgs, &classifications)
}

fn extract_path(input: &serde_json::Value) -> Option<String> {
    input.get("path").and_then(|v| v.as_str()).map(String::from)
}

/// Extract the 0-indexed line range [start, end) from read tool input.
/// Returns None if no offset specified (meaning full file read).
fn extract_line_range(input: &serde_json::Value) -> Option<Range<usize>> {
    let offset = input.get("offset").and_then(|v| v.as_u64())? as usize;
    let start = offset.saturating_sub(1); // offset is 1-indexed
    let limit = input.get("limit").and_then(|v| v.as_u64()).map(|l| l as usize);
    let end = limit.map_or(usize::MAX, |l| start + l);
    Some(start..end)
}

/// Check if range `b` fully contains range `a`. None means full file.
/// A full-file read contains any other read. A partial read only contains `a`
/// if it starts at or before `a` and ends at or after `a`.
fn range_contains(outer: Option<&Range<usize>>, inner: Option<&Range<usize>>) -> bool {
    match (outer, inner) {
        (None, _) => true,
        (_, None) => false,
        (Some(outer), Some(inner)) => outer.start <= inner.start && inner.end <= outer.end,
    }
}

#[cfg(test)]
mod tests {
    use craft_providers::{ContentBlock, Message, Role};
    use serde_json::json;

    use super::*;
    use crate::agent::history::History;

    fn tool_use_msg(id: &str, name: &str, input: serde_json::Value) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_owned(),
                name: name.to_owned(),
                input,
            }],
            ..Default::default()
        }
    }

    fn tool_result_msg(id: &str, content: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_owned(),
                content: content.to_owned(),
                is_error: false,
            }],
            ..Default::default()
        }
    }

    fn user_msg(text: &str) -> Message {
        Message::user(text.into())
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
        let messages = vec![
            user_msg("read it"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "line 1\nline 2\nline 3"),
            user_msg("edit it"),
            tool_use_msg("t2", "edit", json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"})),
            tool_result_msg("t2", "ok"),
        ];
        let classifications = classify_reads(&messages);
        assert_eq!(classifications.len(), 1);
        assert_eq!(classifications[0].state, ReadState::Stale);
        assert_eq!(classifications[0].tool_call_id, "t1");
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
        let messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "content"),
            user_msg("edit"),
            tool_use_msg("t2", "edit", json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"})),
            tool_result_msg("t2", "ok"),
            user_msg("read again"),
            tool_use_msg("t3", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t3", "new content"),
        ];
        let classifications = classify_reads(&messages);
        assert_eq!(classifications.len(), 2);
        assert_eq!(classifications[0].state, ReadState::Stale, "t1: stale (edited after)");
        assert_eq!(classifications[1].state, ReadState::Fresh, "t3: fresh (latest read)");
    }

    #[test]
    fn different_files_dont_interfere() {
        let messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/a.rs"})),
            tool_result_msg("t1", "a content"),
            tool_use_msg("t2", "read", json!({"path": "/src/b.rs"})),
            tool_result_msg("t2", "b content"),
            user_msg("edit a"),
            tool_use_msg("t3", "edit", json!({"path": "/src/a.rs", "old_string": "x", "new_string": "y"})),
            tool_result_msg("t3", "ok"),
        ];
        let classifications = classify_reads(&messages);
        assert_eq!(classifications.len(), 2);
        assert_eq!(classifications[0].state, ReadState::Stale, "a.rs was edited");
        assert_eq!(classifications[1].state, ReadState::Fresh, "b.rs was not touched");
    }

    #[test]
    fn apply_lifecycle_replaces_stale_content() {
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "a long line of content that should be replaced with something even longer to ensure the marker is shorter than the original content being replaced in the tool result"),
            user_msg("edit"),
            tool_use_msg("t2", "edit", json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"})),
            tool_result_msg("t2", "ok"),
        ];
        let classifications = classify_reads(&messages);
        let removed = apply_lifecycle(&mut messages, &classifications);
        assert!(removed > 0);
        let result_msg = &messages[2];
        match &result_msg.content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert!(content.starts_with(STALE_MARKER_PREFIX));
                assert!(content.contains("/src/main.rs"));
                assert!(content.contains("was modified after this read"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn apply_lifecycle_preserves_fresh_content() {
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "fresh content"),
        ];
        let classifications = classify_reads(&messages);
        let removed = apply_lifecycle(&mut messages, &classifications);
        assert_eq!(removed, 0);
        match &messages[2].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, "fresh content");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn run_lifecycle_on_history() {
        let mut history = History::new(vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "a substantial amount of content that would typically appear in a file read operation, spanning multiple lines and containing various code constructs that make it significantly longer than the compact marker which will replace it"),
            user_msg("edit"),
            tool_use_msg("t2", "write", json!({"path": "/src/main.rs", "content": "new"})),
            tool_result_msg("t2", "ok"),
        ]);
        let removed = run_lifecycle(&mut history);
        assert!(removed > 0);
        match &history.as_slice()[2].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert!(content.starts_with(STALE_MARKER_PREFIX));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn write_makes_read_stale() {
        let messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "content"),
            user_msg("write"),
            tool_use_msg("t2", "write", json!({"path": "/src/main.rs", "content": "new"})),
            tool_result_msg("t2", "ok"),
        ];
        let classifications = classify_reads(&messages);
        assert_eq!(classifications.len(), 1);
        assert_eq!(classifications[0].state, ReadState::Stale);
    }

    #[test]
    fn multiedit_makes_read_stale() {
        let messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "content"),
            user_msg("edit"),
            tool_use_msg("t2", "multiedit", json!({"path": "/src/main.rs", "edits": []})),
            tool_result_msg("t2", "ok"),
        ];
        let classifications = classify_reads(&messages);
        assert_eq!(classifications.len(), 1);
        assert_eq!(classifications[0].state, ReadState::Stale);
    }

    #[test]
    fn partial_reads_different_offsets_not_superseded() {
        let messages = vec![
            user_msg("read top"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs", "offset": 1, "limit": 50})),
            tool_result_msg("t1", "lines 1-50"),
            user_msg("read bottom"),
            tool_use_msg("t2", "read", json!({"path": "/src/main.rs", "offset": 51, "limit": 50})),
            tool_result_msg("t2", "lines 51-100"),
        ];
        let classifications = classify_reads(&messages);
        assert_eq!(classifications.len(), 2);
        assert_eq!(classifications[0].state, ReadState::Fresh, "t1: non-overlapping range");
        assert_eq!(classifications[1].state, ReadState::Fresh, "t2: non-overlapping range");
    }

    #[test]
    fn full_file_read_supersedes_partial() {
        let messages = vec![
            user_msg("read partial"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs", "offset": 1, "limit": 50})),
            tool_result_msg("t1", "lines 1-50"),
            user_msg("read full"),
            tool_use_msg("t2", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t2", "all content"),
        ];
        let classifications = classify_reads(&messages);
        assert_eq!(classifications.len(), 2);
        assert_eq!(classifications[0].state, ReadState::Superseded, "t1: covered by full file read");
        assert_eq!(classifications[1].state, ReadState::Fresh);
    }

    #[test]
    fn partially_overlapping_partial_reads_not_superseded() {
        let messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs", "offset": 1, "limit": 100})),
            tool_result_msg("t1", "lines 1-100"),
            user_msg("read again"),
            tool_use_msg("t2", "read", json!({"path": "/src/main.rs", "offset": 50, "limit": 100})),
            tool_result_msg("t2", "lines 50-150"),
        ];
        let classifications = classify_reads(&messages);
        assert_eq!(classifications.len(), 2);
        assert_eq!(classifications[0].state, ReadState::Fresh, "t1: t2 doesn't fully contain t1 (1-100 vs 50-150)");
        assert_eq!(classifications[1].state, ReadState::Fresh);
    }

    #[test]
    fn wider_read_supersedes_narrower() {
        let messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs", "offset": 20, "limit": 30})),
            tool_result_msg("t1", "lines 20-50"),
            user_msg("read wider"),
            tool_use_msg("t2", "read", json!({"path": "/src/main.rs", "offset": 1, "limit": 100})),
            tool_result_msg("t2", "lines 1-100"),
        ];
        let classifications = classify_reads(&messages);
        assert_eq!(classifications.len(), 2);
        assert_eq!(classifications[0].state, ReadState::Superseded, "t1: fully contained by t2 (20-50 vs 1-100)");
        assert_eq!(classifications[1].state, ReadState::Fresh);
    }

    #[test]
    fn adjacent_partial_reads_not_superseded() {
        let messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs", "offset": 1, "limit": 50})),
            tool_result_msg("t1", "lines 1-50"),
            user_msg("read next"),
            tool_use_msg("t2", "read", json!({"path": "/src/main.rs", "offset": 51, "limit": 50})),
            tool_result_msg("t2", "lines 51-100"),
        ];
        let classifications = classify_reads(&messages);
        assert_eq!(classifications.len(), 2);
        assert_eq!(classifications[0].state, ReadState::Fresh, "t1: adjacent (not overlapping) with t2");
        assert_eq!(classifications[1].state, ReadState::Fresh, "t2: adjacent (not overlapping) with t1");
    }

    #[test]
    fn no_reads_produces_empty_classifications() {
        let messages = vec![
            user_msg("hello"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "hi".into(),
                }],
                ..Default::default()
            },
        ];
        let classifications = classify_reads(&messages);
        assert!(classifications.is_empty());
    }
}
