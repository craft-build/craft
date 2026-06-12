use std::collections::{HashMap, HashSet};

use std::ops::Range;

use craft_providers::{ContentBlock, Message, Role};
use tracing::info;

use super::history::History;

use super::compression_store::SharedCompressionStore;

const STALE_MARKER_PREFIX: &str = "[Stale read: ";
const SUPERSEDED_MARKER_PREFIX: &str = "[Superseded read: ";

/// Number of most-recent assistant messages whose edit targets form the "working set".
/// Reads of working-set files are not marked Stale — the model is still actively editing them.
const WORKING_SET_LOOKBACK: usize = 4;

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
///
/// Files in the active "working set" (edited in the last `WORKING_SET_LOOKBACK` assistant
/// messages) are never marked Stale — the model is still actively editing them and
/// stale-marking would destroy the context it needs for the next edit.
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

    // Build the working set: files edited in the last WORKING_SET_LOOKBACK assistant messages.
    let assistant_msg_indices: Vec<usize> = operations
        .iter()
        .map(|op| op.msg_index)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let recent_assistant_indices: HashSet<usize> = {
        let mut sorted = assistant_msg_indices;
        sorted.sort_unstable();
        sorted
            .into_iter()
            .rev()
            .take(WORKING_SET_LOOKBACK)
            .collect()
    };
    let working_set_files: HashSet<&str> = operations
        .iter()
        .filter(|op| {
            matches!(op.op_kind, OpKind::Edit | OpKind::Write)
                && recent_assistant_indices.contains(&op.msg_index)
        })
        .map(|op| op.file_path.as_str())
        .collect();

    let mut classifications = Vec::new();

    let last_assistant_msg = operations.iter().map(|op| op.msg_index).max();

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

        // Working set protection: if the file is actively being edited, don't mark stale.
        // The model still needs the read content for subsequent edits.
        if has_later_edit && !working_set_files.contains(read_op.file_path.as_str()) {
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

        // Don't supersede reads from the most recent turn — the LLM is still
        // actively using them and premature compaction causes re-read feedback loops.
        let is_most_recent = last_assistant_msg.is_some_and(|last| read_op.msg_index == last);

        if has_later_superseding_read && !is_most_recent {
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
pub(super) fn apply_lifecycle(
    history: &mut [Message],
    classifications: &[ReadClassification],
    compression_store: Option<&SharedCompressionStore>,
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
                let original_lines = content.lines().count();

                let hash = compression_store.and_then(|store| {
                    let mut guard = store.lock().ok()?;
                    Some(guard.put(content))
                });

                let mut marker = match state {
                    ReadState::Stale => {
                        // Actionable stale marker: tell the model to re-read before editing.
                        // No retrieval marker — retrieving old content is misleading;
                        // the model needs current content, not historical.
                        if old_len > STALE_MARKER_PREFIX.len() + 60 {
                            format!("{STALE_MARKER_PREFIX}{file_path} was modified after this read. Re-read the file with the read tool before editing. {old_len} chars removed]")
                        } else {
                            format!("{STALE_MARKER_PREFIX}{file_path} was modified after this read. Re-read the file with the read tool before editing.]")
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

                // Only attach retrieval marker for superseded reads — the retrieved
                // content is still valid (a newer read exists with the same data).
                // For stale reads, retrieval returns outdated content which misleads
                // the model into editing against the wrong file state.
                if matches!(state, ReadState::Superseded)
                    && let Some(ref h) = hash
                {
                    marker.push_str(&super::compression_store::retrieval_marker(
                        original_lines,
                        1,
                        h,
                    ));
                }

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
/// When the `onnx` feature is enabled and a scorer is provided, semantically
/// stale reads may be downgraded back to Fresh if the edit content is still
/// similar enough to the original read content.
#[cfg(feature = "onnx")]
pub(super) async fn run_lifecycle(
    history: &mut History,
    scorer: Option<&super::semantic::RelevanceScorer>,
    compression_store: Option<&SharedCompressionStore>,
) -> usize {
    let messages = history.as_slice();
    let mut classifications = classify_reads(messages);

    if let Some(scorer) = scorer {
        let stale: Vec<(String, String)> = classifications
            .iter()
            .filter(|c| c.state != ReadState::Fresh)
            .map(|c| (c.tool_call_id.clone(), c.file_path.clone()))
            .collect();
        if !stale.is_empty() {
            let semantic_results =
                super::semantic::classify_reads_semantic(history.as_slice(), scorer, &stale)
                    .await;
            for (tool_call_id, _, is_still_stale) in semantic_results {
                if !is_still_stale
                    && let Some(c) = classifications
                        .iter_mut()
                        .find(|c| c.tool_call_id == tool_call_id)
                {
                    info!(file = %c.file_path, "semantic stale override: Stale -> Fresh");
                    c.state = ReadState::Fresh;
                }
            }
        }
    }

    let msgs = history.as_mut_slice();
    apply_lifecycle(msgs, &classifications, compression_store)
}

#[cfg(not(feature = "onnx"))]
pub(super) async fn run_lifecycle(
    history: &mut History,
    compression_store: Option<&SharedCompressionStore>,
) -> usize {
    let messages = history.as_slice();
    let classifications = classify_reads(messages);
    let msgs = history.as_mut_slice();
    apply_lifecycle(msgs, &classifications, compression_store)
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

    /// Generate gap assistant turns (reads of unrelated files) to push earlier edits
    /// outside the working set lookback window.
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

    fn find_by_id<'a>(classifications: &'a [ReadClassification], id: &str) -> &'a ReadClassification {
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
            tool_use_msg("t2", "edit", json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"})),
            tool_result_msg("t2", "ok"),
        ];
        // Push the edit outside the working set lookback so the read becomes stale.
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let cls = classify_reads(&messages);
        let c = find_by_id(&cls, "t1");
        assert_eq!(c.state, ReadState::Stale);
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
            tool_use_msg("t2", "edit", json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"})),
            tool_result_msg("t2", "ok"),
            user_msg("read again"),
            tool_use_msg("t3", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t3", "new content"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let classifications = classify_reads(&messages);
        let t1 = find_by_id(&classifications, "t1");
        let t3 = find_by_id(&classifications, "t3");
        assert_eq!(t1.state, ReadState::Stale, "t1: stale (edited after)");
        assert_eq!(t3.state, ReadState::Fresh, "t3: fresh (latest read)");
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
            tool_use_msg("t3", "edit", json!({"path": "/src/a.rs", "old_string": "x", "new_string": "y"})),
            tool_result_msg("t3", "ok"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let classifications = classify_reads(&messages);
        let t1 = find_by_id(&classifications, "t1");
        let t2 = find_by_id(&classifications, "t2");
        assert_eq!(t1.state, ReadState::Stale, "a.rs was edited");
        assert_eq!(t2.state, ReadState::Fresh, "b.rs was not touched");
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
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let classifications = classify_reads(&messages);
        let t1 = find_by_id(&classifications, "t1");
        assert_eq!(t1.state, ReadState::Stale);
        let removed = apply_lifecycle(&mut messages, &classifications, None);
        assert!(removed > 0);
        match &messages[2].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert!(content.starts_with(STALE_MARKER_PREFIX));
                assert!(content.contains("/src/main.rs"));
                assert!(content.contains("was modified after this read"));
                assert!(content.contains("Re-read the file"));
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
        let removed = apply_lifecycle(&mut messages, &classifications, None);
        assert_eq!(removed, 0);
        match &messages[2].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, "fresh content");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_lifecycle_on_history() {
        let mut msgs = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "a substantial amount of content that would typically appear in a file read operation, spanning multiple lines and containing various code constructs that make it significantly longer than the compact marker which will replace it"),
            user_msg("edit"),
            tool_use_msg("t2", "write", json!({"path": "/src/main.rs", "content": "new"})),
            tool_result_msg("t2", "ok"),
        ];
        msgs.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let mut history = History::new(msgs);
        #[cfg(feature = "onnx")]
        let removed = run_lifecycle(&mut history, None, None).await;
        #[cfg(not(feature = "onnx"))]
        let removed = run_lifecycle(&mut history, None).await;
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
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "content"),
            user_msg("write"),
            tool_use_msg("t2", "write", json!({"path": "/src/main.rs", "content": "new"})),
            tool_result_msg("t2", "ok"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let cls = classify_reads(&messages);
        let c = find_by_id(&cls, "t1");
        assert_eq!(c.state, ReadState::Stale);
    }

    #[test]
    fn multiedit_makes_read_stale() {
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "content"),
            user_msg("edit"),
            tool_use_msg("t2", "multiedit", json!({"path": "/src/main.rs", "edits": []})),
            tool_result_msg("t2", "ok"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let cls = classify_reads(&messages);
        let c = find_by_id(&cls, "t1");
        assert_eq!(c.state, ReadState::Stale);
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
    fn working_set_protects_recently_edited_file() {
        // read→edit with no gap: the edit is in the most recent assistant message,
        // so the file is in the working set and the read stays Fresh.
        let messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "content"),
            user_msg("edit"),
            tool_use_msg("t2", "edit", json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"})),
            tool_result_msg("t2", "ok"),
        ];
        let cls = classify_reads(&messages);
        let c = find_by_id(&cls, "t1");
        assert_eq!(c.state, ReadState::Fresh, "read protected by working set");
    }

    #[test]
    fn working_set_expires_after_lookback() {
        // read→edit, then gap turns to push the edit outside the working set.
        // With 2 original assistant msgs, we need gap turns such that the edit
        // stays in / falls out of the last WORKING_SET_LOOKBACK.
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", "content"),
            user_msg("edit"),
            tool_use_msg("t2", "edit", json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"})),
            tool_result_msg("t2", "ok"),
        ];
        // 2 original + (LOOKBACK-2) gap = LOOKBACK total assistant msgs → edit is in the set.
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK - 2));
        let cls = classify_reads(&messages);
        let c = find_by_id(&cls, "t1");
        assert_eq!(c.state, ReadState::Fresh, "edit still in working set");

        // One more gap turn pushes the edit outside.
        messages.extend(gap_assistant_turns(2));
        let cls = classify_reads(&messages);
        let c = find_by_id(&cls, "t1");
        assert_eq!(c.state, ReadState::Stale, "edit expired from working set");
    }

    #[test]
    fn stale_marker_has_re_read_instruction() {
        let content = "a long line of content that should be replaced with a stale marker with re-read instruction that is significantly shorter than this original content to ensure the marker fits inside the original tool result";
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", content),
            user_msg("edit"),
            tool_use_msg("t2", "edit", json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"})),
            tool_result_msg("t2", "ok"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let classifications = classify_reads(&messages);
        let removed = apply_lifecycle(&mut messages, &classifications, None);
        assert!(removed > 0, "marker should be shorter than content");
        match &messages[2].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert!(content.contains("Re-read the file with the read tool before editing"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn stale_read_has_no_retrieval_marker() {
        let content = "a long line of content that should be replaced with a stale marker without retrieval that is significantly shorter than this original content to ensure the marker fits inside the original tool result";
        let mut messages = vec![
            user_msg("read"),
            tool_use_msg("t1", "read", json!({"path": "/src/main.rs"})),
            tool_result_msg("t1", content),
            user_msg("edit"),
            tool_use_msg("t2", "edit", json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"})),
            tool_result_msg("t2", "ok"),
        ];
        messages.extend(gap_assistant_turns(WORKING_SET_LOOKBACK + 1));
        let classifications = classify_reads(&messages);
        let store = super::super::compression_store::shared_store();
        apply_lifecycle(&mut messages, &classifications, Some(&store));
        match &messages[2].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert!(!content.contains("Retrieve original"), "stale reads should not have retrieval markers");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
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
