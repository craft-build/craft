//! Durable alteration passes: per-message revisions of the latest active
//! content, applied between runs, with the original and earlier revisions
//! kept in the entry. Two passes exist:
//!
//! - **Read lifecycle**: `classify_reads` over the active view decides which
//!   `read` results are Stale or Superseded; those results are revised into
//!   today's marker text, tagged `stale`/`superseded`. It runs first: the
//!   classifier parses `move` results, which compression would shrink.
//! - **Output compression**: the `compression/` compressors applied to
//!   tool-result text once a result leaves the recent window
//!   (`protect_recent_tool_outputs` entries), tagged `craft.alter` carrying
//!   `compressed`.
//!
//! `Alteration::call` sees one message at a time, so both passes compute
//! their global context from a snapshot first (the recent window, the
//! call-name table, the read classifications) and then look up per-block
//! decisions by `tool_use_id`.
//!
//! Idempotence: the tag value is a sorted comma-joined set of every kind
//! that has touched the entry (`compressed`, `stale`, `superseded`) — one
//! step's observation entry can hold a compressible result and a read
//! result side by side, so neither pass may untag the other. Per block, a
//! result already carrying a lifecycle marker prefix is never re-marked,
//! and compression skips tagged entries.

use std::collections::{HashMap, HashSet};

use animist::history::{Alteration, ContentBlock, History, StoredMessage, ToolContents};

use crate::compression::{self, CompressionConfig};
use crate::harness::read_lifecycle::{ReadState, STALE_MARKER_PREFIX, SUPERSEDED_MARKER_PREFIX};
use crate::history::ext;

/// Tag recording which durable alteration kinds have revised an entry.
pub const ALTER_TAG: &str = "craft.alter";
/// The compression pass's kind.
pub const ALTER_COMPRESSED: &str = "compressed";
/// The read-lifecycle pass's kinds.
pub const ALTER_STALE: &str = "stale";
pub const ALTER_SUPERSEDED: &str = "superseded";

/// Add `kind` to the message's alteration tag set, keeping the other kinds.
fn add_alter_tag(stored: &mut StoredMessage, kind: &str) {
    let mut kinds: Vec<String> = stored
        .tags()
        .get(ALTER_TAG)
        .map(|value| value.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    if !kinds.iter().any(|existing| existing == kind) {
        kinds.push(kind.to_owned());
    }
    kinds.sort();
    stored.tags_mut().insert(
        ALTER_TAG.to_owned(),
        kinds.into_iter().collect::<Vec<_>>().join(","),
    );
}

/// Whether the message's alteration tag set contains `kind`.
fn has_alter_tag(stored: &StoredMessage, kind: &str) -> bool {
    stored
        .tags()
        .get(ALTER_TAG)
        .is_some_and(|value| value.split(',').any(|existing| existing == kind))
}

/// A result text already replaced by a read-lifecycle marker.
fn is_lifecycle_marker(text: &str) -> bool {
    text.starts_with(STALE_MARKER_PREFIX) || text.starts_with(SUPERSEDED_MARKER_PREFIX)
}

/// The `tool_use_id` → tool-name table for one snapshot: results written
/// before name metadata (or with it stripped) borrow their call's name.
fn names_by_id(entries: &[animist::history::HistoryEntry]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for stored in entries.iter().filter_map(|entry| entry.current()) {
        for block in ext::tool_calls(stored.message()) {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                names.insert(id.clone(), name.clone());
            }
        }
    }
    names
}

/// The output-compression pass. Results inside the recent window stay raw so
/// the model keeps full text for the calls it is still working from; the
/// compressed bytes come from the same `compress_for_llm` the request view
/// produces, so the model-visible request does not change when a result
/// crosses the window — only where the rewrite lives.
pub(crate) struct CompressAlteration<'a> {
    config: &'a CompressionConfig,
    /// Tool results inside the recent window, keyed by call id.
    recent: HashSet<String>,
    /// Call-name table for results written without name metadata.
    names: HashMap<String, String>,
}

impl<'a> CompressAlteration<'a> {
    fn new(config: &'a CompressionConfig, snapshot: &animist::history::HistorySnapshot) -> Self {
        let mut result_entries = 0_usize;
        let mut recent = HashSet::new();
        // Walk backwards so the window is the *most recent* result entries.
        for entry in snapshot.entries().iter().rev() {
            let Some(stored) = entry.current() else {
                continue;
            };
            let ids: Vec<String> = ext::tool_results(stored.message())
                .filter_map(|block| match block {
                    ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                    _ => None,
                })
                .collect();
            if ids.is_empty() {
                continue;
            }
            result_entries += 1;
            if result_entries <= config.protect_recent_tool_outputs {
                recent.extend(ids);
            }
        }
        Self {
            config,
            recent,
            names: names_by_id(snapshot.entries()),
        }
    }
}

#[async_trait::async_trait]
impl Alteration for CompressAlteration<'_> {
    async fn call(&self, message: &StoredMessage) -> Result<Option<StoredMessage>, animist::Error> {
        if !self.config.enabled || has_alter_tag(message, ALTER_COMPRESSED) {
            return Ok(None);
        }
        let mut revised = None;
        for block in ext::tool_results(message.message()) {
            let ContentBlock::ToolResult {
                tool_use_id,
                name,
                content,
                ..
            } = block
            else {
                continue;
            };
            if self.recent.contains(tool_use_id) {
                continue;
            }
            let name = name
                .as_deref()
                .or_else(|| self.names.get(tool_use_id).map(String::as_str));
            let Some(name) = name else {
                continue;
            };
            if !compression::should_compress_tool(name) {
                continue;
            }
            let compressed: Vec<_> = content
                .0
                .iter()
                .map(|item| match item {
                    animist::history::ToolContent::Text { text } => {
                        animist::history::ToolContent::Text {
                            text: compression::compress_for_llm(text, self.config),
                        }
                    }
                    other => other.clone(),
                })
                .collect();
            if compressed == content.0 {
                continue;
            }
            let revised = revised.get_or_insert_with(|| message.clone());
            for block in revised.message_mut().content.iter_mut() {
                if let ContentBlock::ToolResult {
                    tool_use_id: id,
                    content,
                    ..
                } = block
                    && id == tool_use_id
                {
                    *content = ToolContents::from(compressed.clone());
                }
            }
        }
        match revised {
            None => Ok(None),
            Some(mut revised) => {
                add_alter_tag(&mut revised, ALTER_COMPRESSED);
                Ok(Some(revised))
            }
        }
    }
}

/// The read-lifecycle pass. The classifications are computed from a snapshot
/// of the active view (they need the whole conversation's file operations),
/// then the alteration revises the stale and superseded results into today's
/// marker text. Superseded markers keep the retrieval suffix naming the hash
/// of the original content, which `retrieve` resolves against entry sources.
pub(crate) struct ReadLifecycleAlteration {
    /// Read classifications that are not Fresh, keyed by call id.
    decisions: HashMap<String, (ReadState, String)>,
}

impl ReadLifecycleAlteration {
    fn new(root: Option<&str>, snapshot: &animist::history::HistorySnapshot) -> Self {
        let view: Vec<_> = snapshot
            .entries()
            .iter()
            .filter_map(|entry| entry.current())
            .map(|stored| stored.message().clone())
            .collect();
        let decisions = crate::harness::read_lifecycle::classify_reads(&view, root)
            .into_iter()
            .filter(|classification| classification.state != ReadState::Fresh)
            .map(|classification| {
                (
                    classification.tool_call_id,
                    (classification.state, classification.file_path),
                )
            })
            .collect();
        Self { decisions }
    }
}

#[async_trait::async_trait]
impl Alteration for ReadLifecycleAlteration {
    async fn call(&self, message: &StoredMessage) -> Result<Option<StoredMessage>, animist::Error> {
        let mut revised = None;
        let mut applied: Vec<&str> = Vec::new();
        for block in ext::tool_results(message.message()) {
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
            let Some((state, file_path)) = self.decisions.get(tool_use_id) else {
                continue;
            };
            let original = ext::tool_content_text(content);
            if is_lifecycle_marker(&original) {
                // Already marked: a later pass never re-marks a marker, even
                // when the entry's tag names a different kind.
                continue;
            }
            let old_len = original.len();
            let original_lines = original.lines().count();
            let state = *state;
            let mut marker = match state {
                ReadState::Stale => {
                    // Actionable stale marker: retrieving old content would
                    // be misleading; the model needs current content.
                    if old_len > STALE_MARKER_PREFIX.len() + 60 {
                        format!(
                            "{STALE_MARKER_PREFIX}{file_path} was modified after this read. \
                             Re-read the file with the read tool before editing. {old_len} chars \
                             removed]"
                        )
                    } else {
                        format!(
                            "{STALE_MARKER_PREFIX}{file_path} was modified after this read. \
                             Re-read the file with the read tool before editing.]"
                        )
                    }
                }
                ReadState::Superseded => {
                    if old_len > SUPERSEDED_MARKER_PREFIX.len() + 60 {
                        format!(
                            "{SUPERSEDED_MARKER_PREFIX}{file_path} was re-read later. {old_len} \
                             chars removed]"
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
                // The marker is one line, so today's request-view shape said
                // `1 lines compressed`; the durable revision keeps those
                // bytes.
                marker.push_str(&crate::compression::retrieval_marker(
                    original_lines,
                    1,
                    &crate::compression::content_hash(&original),
                ));
            }
            applied.push(match state {
                ReadState::Stale => ALTER_STALE,
                _ => ALTER_SUPERSEDED,
            });
            let revised = revised.get_or_insert_with(|| message.clone());
            for block in revised.message_mut().content.iter_mut() {
                if let ContentBlock::ToolResult {
                    tool_use_id: id,
                    content,
                    ..
                } = block
                    && id == tool_use_id
                {
                    *content = ToolContents::from(marker.clone());
                }
            }
        }
        match revised {
            None => Ok(None),
            Some(mut revised) => {
                for kind in applied {
                    add_alter_tag(&mut revised, kind);
                }
                Ok(Some(revised))
            }
        }
    }
}

/// Run every durable alteration pass over the history, read lifecycle
/// first — the classifier reads uncompressed `move` results (their text
/// names the files a move rewrote). Each pass commits atomically; a
/// concurrent mutation returns `HistoryError::Conflict` with nothing
/// committed, and the caller (the driver, which owns the between-runs
/// sequencing) retries once.
#[expect(
    clippy::result_large_err,
    reason = "animist's HistoryError is the operation's public error type"
)]
pub async fn run_passes(
    history: &History,
    config: &CompressionConfig,
    root: Option<&str>,
) -> Result<(), animist::history::HistoryError> {
    let snapshot = history.snapshot().await;
    history
        .alter(ReadLifecycleAlteration::new(root, &snapshot))
        .await?;
    let snapshot = history.snapshot().await;
    history
        .alter(CompressAlteration::new(config, &snapshot))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::test_support as ts;
    use animist::history::History;

    /// A bash round whose result compresses under the default config
    /// (numbered code, well past `MIN_COMPRESS_LEN`).
    fn bash_round(id: &str, lines: usize) -> Vec<StoredMessage> {
        let output: String = (1..=lines)
            .map(|i| format!("{i}: let x = {i};\n"))
            .collect();
        vec![
            ts::stored_assistant_tool_args(
                id,
                "bash",
                serde_json::json!({"command": "cargo test"}),
            ),
            ts::stored_tool_result_of(id, &output),
        ]
    }

    /// A read→edit→gap pattern whose first read ends Stale outside the
    /// working-set lookback.
    fn stale_read_history() -> Vec<StoredMessage> {
        let mut messages = vec![
            ts::stored_user("read"),
            ts::stored_assistant_tool_args(
                "r1",
                "read",
                serde_json::json!({"path": "/src/main.rs"}),
            ),
            ts::stored_tool_result_of("r1", "line 1\nline 2\nline 3"),
            ts::stored_user("edit"),
            ts::stored_assistant_tool_args(
                "e1",
                "edit",
                serde_json::json!({"path": "/src/main.rs", "old_string": "x", "new_string": "y"}),
            ),
            ts::stored_tool_result_of("e1", "ok"),
        ];
        // Enough later read rounds to push the edit outside the lookback.
        for gap in 0..crate::harness::read_lifecycle::WORKING_SET_LOOKBACK + 1 {
            let id = format!("gap{gap}");
            messages.push(ts::stored_assistant_tool_args(
                &id,
                "read",
                serde_json::json!({"path": format!("/other/{gap}.rs")}),
            ));
            messages.push(ts::stored_tool_result_of(&id, "gap content"));
        }
        messages
    }

    async fn seeded(messages: &[StoredMessage]) -> History {
        let history = History::default();
        for message in messages {
            history.add_stored(message.clone()).await;
        }
        history
    }

    async fn active(history: &History) -> Vec<StoredMessage> {
        history
            .snapshot()
            .await
            .entries()
            .iter()
            .filter_map(|entry| entry.current())
            .cloned()
            .collect()
    }

    const CONFIG: CompressionConfig = CompressionConfig {
        enabled: true,
        code_compression_rate: 0.3,
        max_log_lines: 50,
        max_search_files: 20,
        max_matches_per_file: 5,
        max_diff_lines: 100,
        max_json_items: 15,
        json_first_keep: 5,
        json_last_keep: 3,
        protect_recent_tool_outputs: 2,
    };

    #[tokio::test]
    async fn compression_pass_compresses_old_results_and_keeps_recent_raw() {
        let mut messages = Vec::new();
        for i in 0..4 {
            messages.extend(bash_round(&format!("t{i}"), 60));
        }
        let history = seeded(&messages).await;
        run_passes(&history, &CONFIG, None).await.expect("passes");
        let view = active(&history).await;
        // The four rounds live in four result entries; the recent window
        // protects the last two.
        let results: Vec<String> = view
            .iter()
            .filter(|stored| {
                crate::history::ext::tool_results(stored.message())
                    .next()
                    .is_some()
            })
            .map(|stored| {
                let block = crate::history::ext::tool_results(stored.message())
                    .next()
                    .unwrap();
                match block {
                    ContentBlock::ToolResult { content, .. } => ext::tool_content_text(content),
                    _ => unreachable!(),
                }
            })
            .collect();
        assert_eq!(results.len(), 4);
        assert!(
            results[0].contains("lines omitted"),
            "results outside the window compress"
        );
        assert!(
            results[3].contains("60: let x = 60"),
            "recent results stay raw"
        );
    }

    #[tokio::test]
    async fn passes_keep_replay_valid_and_never_split_exchanges() {
        let mut messages = stale_read_history();
        messages.extend(bash_round("b1", 60));
        let history = seeded(&messages).await;
        run_passes(&history, &CONFIG, None).await.expect("passes");
        let view = active(&history).await;
        let wire: Vec<_> = view.iter().map(StoredMessage::message).cloned().collect();
        animist::protocol::validate_replay(&wire, &[])
            .expect("the altered view stays replay-valid");
        // Every ToolResult block keeps its identity, and thinking payloads
        // ride untouched (the passes only rewrite tool-result text).
        for (index, stored) in view.iter().enumerate() {
            for block in &stored.message().content {
                match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        name,
                        ..
                    } => {
                        assert!(!tool_use_id.is_empty());
                        let _ = is_error;
                        let _ = name;
                    }
                    ContentBlock::Thinking { signature, .. } => {
                        assert!(
                            signature.is_some(),
                            "thinking signatures survive (#{index})"
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    #[tokio::test]
    async fn originals_are_recoverable_from_the_snapshot() {
        // Three rounds: the first result sits outside the recent window,
        // so the pass compresses it.
        let mut messages = bash_round("t0", 60);
        messages.extend(bash_round("t1", 60));
        messages.extend(bash_round("t2", 60));
        let history = seeded(&messages).await;
        run_passes(&history, &CONFIG, None).await.expect("passes");
        let snapshot = history.snapshot().await;
        // The latest revision is compressed; the revision under it (and the
        // tag-free history the pass built from) holds the original.
        let entry = snapshot
            .entries()
            .iter()
            .find(|entry| {
                crate::history::ext::tool_results(entry.latest().message())
                    .next()
                    .is_some_and(|block| {
                        matches!(block, ContentBlock::ToolResult { content, .. }
                            if ext::tool_content_text(content).contains("lines omitted"))
                    })
            })
            .expect("a compressed result entry");
        let latest = entry.latest();
        assert!(
            ext::message_text(latest).contains("lines omitted")
                || matches!(
                    crate::history::ext::tool_results(latest.message()).next(),
                    Some(ContentBlock::ToolResult { content, .. })
                        if ext::tool_content_text(content).contains("lines omitted")
                )
        );
        let original = entry
            .revisions()
            .iter()
            .rev()
            .find(|stored| !stored.tags().contains_key(ALTER_TAG))
            .unwrap_or_else(|| entry.verbatim());
        let block = crate::history::ext::tool_results(original.message())
            .next()
            .unwrap();
        match block {
            ContentBlock::ToolResult { content, .. } => {
                let text = ext::tool_content_text(content);
                assert!(text.contains("60: let x = 60"), "the original survives");
                // The same hash the retrieval markers name (the store's
                // `{:08x}` shape: at least 8 hex chars, never truncated).
                let hash = crate::compression::content_hash(&text);
                assert!(hash.len() >= 8 && hash.chars().all(|c| c.is_ascii_hexdigit()));
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn tags_prevent_double_application() {
        let mut messages = stale_read_history();
        messages.extend(bash_round("b1", 60));
        let history = seeded(&messages).await;
        run_passes(&history, &CONFIG, None).await.expect("passes");
        let first = history.snapshot().await;
        // The second run must find nothing to do: compression skips tagged
        // entries, and lifecycle markers are never re-marked.
        run_passes(&history, &CONFIG, None).await.expect("passes");
        let second = history.snapshot().await;
        assert_eq!(
            first.entries(),
            second.entries(),
            "a settled history is idempotent under the passes"
        );
    }

    #[tokio::test]
    async fn stale_reads_get_marker_revisions_tagged_by_state() {
        let messages = stale_read_history();
        let history = seeded(&messages).await;
        run_passes(&history, &CONFIG, None).await.expect("passes");
        let view = active(&history).await;
        let stale = view
            .iter()
            .find(|stored| {
                crate::history::ext::tool_results(stored.message()).any(|block| {
                    matches!(block, ContentBlock::ToolResult { content, .. }
                        if ext::tool_content_text(content).starts_with("[Stale read: "))
                })
            })
            .expect("the stale read is durably marked");
        assert_eq!(
            stale.tags().get(ALTER_TAG).map(String::as_str),
            Some(ALTER_STALE)
        );
    }

    /// Both passes can revise one entry (a parallel wave holding a read
    /// result and a compressible bash result in one observation): the
    /// kinds must coexist in the tag set and neither block may lose its
    /// rewrite. This is the interaction the set-valued `craft.alter` tag
    /// exists for; the pass order itself (lifecycle first) is not
    /// byte-observable today — no compressor matches the `move`-result
    /// shape whose classification motivated it, so the hazard is latent —
    /// which is why the order is documented rather than pinned here.
    #[tokio::test]
    async fn both_passes_revise_one_entry_without_untagging_each_other() {
        // A read that a later edit stales, and a compressible bash result,
        // committed in one observation message.
        let read_result = ext::tool_result("r1", Some("read"), "line 1\nline 2\nline 3", false);
        let bash_output: String = (1..=60).map(|i| format!("{i}: let x = {i};\n")).collect();
        let bash_result = ext::tool_result("b1", Some("bash"), bash_output, false);
        let mut messages = vec![
            crate::history::Message {
                role: animist::history::Role::Assistant,
                content: vec![crate::history::ContentBlock::ToolUse {
                    id: "r1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "/src/main.rs"}),
                    thought_signature: None,
                }],
                display_text: None,
                kind: animist::history::MessageKind::Turn,
            },
            ext::observation(vec![read_result, bash_result]),
            crate::history::Message {
                role: animist::history::Role::Assistant,
                content: vec![crate::history::ContentBlock::ToolUse {
                    id: "e1".into(),
                    name: "edit".into(),
                    input: serde_json::json!({"path": "/src/main.rs"}),
                    thought_signature: None,
                }],
                display_text: None,
                kind: animist::history::MessageKind::Turn,
            },
            ext::observation(vec![ext::tool_result("e1", Some("edit"), "ok", false)]),
        ];
        // Push the edit outside the working-set lookback so the read stales.
        for gap in 0..crate::harness::read_lifecycle::WORKING_SET_LOOKBACK + 1 {
            let id = format!("g{gap}");
            messages.push(crate::history::Message {
                role: animist::history::Role::Assistant,
                content: vec![crate::history::ContentBlock::ToolUse {
                    id: id.clone(),
                    name: "read".into(),
                    input: serde_json::json!({"path": format!("/other/{gap}.rs")}),
                    thought_signature: None,
                }],
                display_text: None,
                kind: animist::history::MessageKind::Turn,
            });
            messages.push(ext::observation(vec![ext::tool_result(
                &id,
                Some("read"),
                "gap",
                false,
            )]));
        }
        // Wrap the wire messages as stored entries the history accepts.
        let stored: Vec<StoredMessage> = messages.into_iter().map(ext::stored).collect();
        let history = seeded(&stored).await;
        run_passes(&history, &CONFIG, None).await.expect("passes");
        let view = active(&history).await;
        let revised = view
            .iter()
            .find(|stored| {
                stored
                    .tags()
                    .get(ALTER_TAG)
                    .is_some_and(|value| value.contains(ALTER_COMPRESSED))
            })
            .expect("the mixed entry carries the compression tag");
        let kinds = revised.tags().get(ALTER_TAG).map(String::as_str);
        assert!(
            kinds == Some("compressed,stale") || kinds == Some("compressed,superseded"),
            "both kinds coexist in the set: {kinds:?}"
        );
        let read_text = revised
            .message()
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == "r1" => Some(ext::tool_content_text(content)),
                _ => None,
            })
            .expect("the read block survived");
        assert!(
            read_text.starts_with("[Stale read: ") || read_text.starts_with("[Superseded read: "),
            "the read block is durably marked: {read_text}"
        );
        let bash_text = revised
            .message()
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == "b1" => Some(ext::tool_content_text(content)),
                _ => None,
            })
            .expect("the bash block survived");
        assert!(
            bash_text.contains("lines omitted"),
            "the bash block is compressed: {bash_text}"
        );
        // And a second run stays settled.
        run_passes(&history, &CONFIG, None).await.expect("passes");
        assert!(history.snapshot().await.entries().iter().any(|entry| {
            entry.current().is_some_and(|stored| {
                stored.tags().get(ALTER_TAG).is_some_and(|value| {
                    value == "compressed,stale" || value == "compressed,superseded"
                })
            })
        }));
    }

    /// A pass whose commit races a concurrent append conflicts and commits
    /// nothing — the animist contract the driver's retry relies on. The
    /// racing writer appends inside the first callback, between the
    /// manager's snapshot and its commit.
    #[tokio::test]
    async fn alteration_racing_a_concurrent_add_conflicts_with_nothing_committed() {
        struct AppendInside(History);
        #[async_trait::async_trait]
        impl Alteration for AppendInside {
            async fn call(
                &self,
                _message: &StoredMessage,
            ) -> Result<Option<StoredMessage>, animist::Error> {
                self.0.add_stored(ts::stored_user("racing")).await;
                Ok(None)
            }
        }
        let history = seeded(&[ts::stored_user("first")]).await;
        match history.alter(AppendInside(history.clone())).await {
            Err(animist::history::HistoryError::Conflict { .. }) => {}
            other => panic!("expected Conflict, got {other:?}"),
        }
        // Nothing from the pass committed: the racing append is the only
        // change.
        let view = active(&history).await;
        assert_eq!(view.len(), 2);
        assert_eq!(ext::message_text(&view[0]), "first");
        assert_eq!(ext::message_text(&view[1]), "racing");
    }
}
