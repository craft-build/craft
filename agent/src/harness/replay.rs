//! The resumed-session transcript: one builder every surface replays a
//! persisted conversation through (TUI resume, ACP `session/load`, headless
//! `--continue`'s history restore), mirroring `history.current()` —
//! inactive entries are hidden, each entry shows its latest revision, and
//! the verbatim original stays one field away for the "show full output"
//! affordance. Compaction summaries and markers are identified by their
//! tags (`craft.compaction`, `craft.marker`), never by their text.

use animist::history::{ContentBlock, HistoryEntry, Role};

use crate::compaction::llm::COMPACTION_TAG;
use crate::history::ext;
use crate::history::marker;

/// One displayable item of a resumed session's transcript.
#[derive(Clone, Debug, PartialEq)]
pub enum ReplayItem {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    /// A tool result: `text` is what the current view shows (the altered
    /// text when an alteration revised the entry), `full` the verbatim
    /// original when it differs. `name` is the result's own name, the
    /// matching call's, or `"unknown"` when neither survives (the
    /// `ext::tool_name` convention).
    ToolResult {
        id: String,
        name: String,
        text: String,
        full: Option<String>,
        is_error: bool,
    },
    /// A compaction summary (`craft.compaction`), rendered as a card.
    Compaction {
        kind: String,
        text: String,
    },
    /// A synthetic marker (`craft.marker`): cancel, end, failure, grace,
    /// nudge or continuation.
    Marker {
        kind: String,
        text: String,
    },
}

/// Build the replay list for a persisted entry list: active entries only,
/// each as its current view, in order.
pub fn transcript(entries: &[HistoryEntry]) -> Vec<ReplayItem> {
    let mut items = Vec::new();
    // Calls seen so far, for results whose block carries no name (the
    // pre-name legacy convention `ext::tool_name` also falls back to).
    let mut calls: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for entry in entries.iter().filter(|entry| entry.is_active()) {
        let current = entry.latest();
        let message = current.message();
        if let Some(kind) = marker::of(current) {
            items.push(ReplayItem::Marker {
                kind: kind.to_owned(),
                text: ext::message_text(current),
            });
            continue;
        }
        if let Some(kind) = current.tags().get(COMPACTION_TAG) {
            items.push(ReplayItem::Compaction {
                kind: kind.clone(),
                text: ext::message_text(current),
            });
            continue;
        }
        // Text and tool-result blocks keep their in-message order; an entry
        // may carry several of each once user messages are coalesced.
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    match message.role {
                        Role::User => items.push(ReplayItem::User { text: text.clone() }),
                        Role::Assistant => items.push(ReplayItem::Assistant { text: text.clone() }),
                        // System instructions are not transcript content;
                        // craft stores none, foreign sessions replay without
                        // them.
                        Role::System => {}
                    }
                }
                ContentBlock::ToolUse { id, name, .. } => {
                    calls.insert(id, name);
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    name,
                    content,
                    is_error,
                    ..
                } => {
                    let text = ext::tool_content_text(content);
                    // The verbatim original, when an alteration or compaction
                    // revised what the entry shows.
                    let full = verbatim_result_text(entry, tool_use_id, &text);
                    let name = name
                        .as_deref()
                        .or_else(|| calls.get(tool_use_id.as_str()).copied())
                        .unwrap_or("unknown")
                        .to_owned();
                    items.push(ReplayItem::ToolResult {
                        id: tool_use_id.clone(),
                        name,
                        text,
                        full,
                        is_error: *is_error,
                    });
                }
                _ => {}
            }
        }
    }
    items
}

/// Animist's agent commits every tool-result entry with a placeholder
/// source ("Tool execution interrupted…", `is_error`) that the final
/// revision replaces; the agent squashes to exactly placeholder + final.
/// A placeholder is never the "full output" the affordance promises.
const INTERRUPT_PLACEHOLDER: &str =
    "Tool execution interrupted or not completed; external effects may have occurred";

/// The verbatim original for `id`: the richest copy among the entry's
/// source and revisions that differs from the text being shown, skipping
/// the placeholder. `None` when nothing richer than the shown text exists
/// (an unaltered result whose only other copy is the placeholder).
fn verbatim_result_text(entry: &HistoryEntry, id: &str, shown: &str) -> Option<String> {
    let candidate = |stored: &animist::history::StoredMessage| {
        stored
            .message()
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == id => Some(ext::tool_content_text(content)),
                _ => None,
            })
    };
    std::iter::once(entry.verbatim())
        .chain(entry.revisions())
        .filter_map(candidate)
        .filter(|text| text != shown && text != INTERRUPT_PLACEHOLDER)
        .max_by_key(|text| text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::alter::ALTER_TAG;
    use crate::history::ext;

    #[test]
    fn inactive_entries_are_hidden() {
        let mut entries = vec![ext::entry(ext::stored_user_text("kept"))];
        let dropped = ext::entry(ext::stored_user_text("compacted away"));
        entries.push(
            HistoryEntry::builder()
                .source(dropped.verbatim().clone())
                .active(false)
                .build(),
        );
        entries.push(ext::entry(ext::stored_user_text("after")));
        let items = transcript(&entries);
        assert_eq!(
            items,
            vec![
                ReplayItem::User {
                    text: "kept".into()
                },
                ReplayItem::User {
                    text: "after".into()
                },
            ]
        );
    }

    #[test]
    fn markers_and_compaction_render_by_tag() {
        let marker_entry = ext::entry(marker::observation(marker::CANCEL, "[Cancelled by user]"));
        let mut summary = ext::stored_user_text("summary of the early turns");
        summary
            .tags_mut()
            .insert(COMPACTION_TAG.to_owned(), "vcc".into());
        let summary_entry = ext::entry(summary);

        let items = transcript(&[
            marker_entry,
            summary_entry,
            // A user typing a marker's text stays plain user text.
            ext::entry(ext::stored_user_text("[Cancelled by user]")),
        ]);
        assert_eq!(
            items,
            vec![
                ReplayItem::Marker {
                    kind: marker::CANCEL.into(),
                    text: "[Cancelled by user]".into(),
                },
                ReplayItem::Compaction {
                    kind: "vcc".into(),
                    text: "summary of the early turns".into(),
                },
                ReplayItem::User {
                    text: "[Cancelled by user]".into()
                },
            ]
        );
    }

    #[test]
    fn altered_results_show_altered_text_with_verbatim_full() {
        let source = ext::stored(ext::observation(vec![ext::tool_result(
            "t1",
            Some("bash"),
            "the original long output",
            false,
        )]));
        let mut revised_message = source.message().clone();
        let ContentBlock::ToolResult { content, .. } = &mut revised_message.content[0] else {
            panic!("tool result block");
        };
        *content = "[4 lines compressed from 4096 | retrieve: hash=abc]".into();
        let mut revised = ext::stored(revised_message);
        revised
            .tags_mut()
            .insert(ALTER_TAG.to_owned(), "compressed".into());
        let entry = HistoryEntry::builder()
            .source(source)
            .revisions(vec![revised])
            .build();

        let items = transcript(&[entry]);
        assert_eq!(
            items,
            vec![ReplayItem::ToolResult {
                id: "t1".into(),
                name: "bash".into(),
                text: "[4 lines compressed from 4096 | retrieve: hash=abc]".into(),
                full: Some("the original long output".into()),
                is_error: false,
            }]
        );
    }

    /// The production entry shape: animist commits a placeholder source
    /// and squashes to placeholder + one final revision. An
    /// unaltered result must NOT offer the placeholder as its "full
    /// output", and an altered one must surface the pre-alteration
    /// revision, not the placeholder source.
    #[test]
    fn placeholder_backed_entries_resolve_full_output_from_revisions() {
        let placeholder = ext::stored(ext::observation(vec![ext::tool_result(
            "t9",
            Some("bash"),
            INTERRUPT_PLACEHOLDER,
            true,
        )]));
        // Unaltered: placeholder source + the real output as final revision.
        let done = HistoryEntry::builder()
            .source(placeholder.clone())
            .revisions(vec![ext::stored(ext::observation(vec![ext::tool_result(
                "t9",
                Some("bash"),
                "the finished output",
                false,
            )]))])
            .build();
        assert_eq!(
            transcript(&[done]),
            vec![ReplayItem::ToolResult {
                id: "t9".into(),
                name: "bash".into(),
                text: "the finished output".into(),
                // The placeholder is never offered as the original.
                full: None,
                is_error: false,
            }]
        );

        // Altered: placeholder source, real output, then the compression
        // revision (the durable 08 pass revises the squashed entry).
        let mut altered_stored = ext::stored(ext::observation(vec![ext::tool_result(
            "t9",
            Some("bash"),
            "[9 lines compressed from 9001 | retrieve: hash=ab12]",
            false,
        )]));
        altered_stored.tags_mut().insert(
            crate::compaction::alter::ALTER_TAG.to_owned(),
            "compressed".into(),
        );
        let altered = HistoryEntry::builder()
            .source(placeholder)
            .revisions(vec![
                ext::stored(ext::observation(vec![ext::tool_result(
                    "t9",
                    Some("bash"),
                    "the finished output",
                    false,
                )])),
                altered_stored,
            ])
            .build();
        assert_eq!(
            transcript(&[altered]),
            vec![ReplayItem::ToolResult {
                id: "t9".into(),
                name: "bash".into(),
                text: "[9 lines compressed from 9001 | retrieve: hash=ab12]".into(),
                full: Some("the finished output".into()),
                is_error: false,
            }]
        );
    }

    #[test]
    fn unaltered_results_carry_no_full_copy() {
        let entry = ext::entry(ext::stored(ext::observation(vec![ext::tool_result(
            "t2",
            Some("read"),
            "plain output",
            true,
        )])));
        let items = transcript(&[entry]);
        assert_eq!(
            items,
            vec![ReplayItem::ToolResult {
                id: "t2".into(),
                name: "read".into(),
                text: "plain output".into(),
                full: None,
                is_error: true,
            }]
        );
    }

    #[test]
    fn assistant_and_tool_results_keep_block_order() {
        let message = animist::history::Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "running it".into(),
                },
                ContentBlock::Thinking {
                    thinking: "reasoning is not transcript".into(),
                    signature: None,
                },
                ContentBlock::ToolUse {
                    id: "t3".into(),
                    name: "grep".into(),
                    input: serde_json::json!({}),
                    thought_signature: None,
                },
            ],
            display_text: None,
            kind: animist::history::MessageKind::Turn,
        };
        let items = transcript(&[
            ext::entry(ext::stored(message)),
            ext::entry(ext::stored(ext::observation(vec![ext::tool_result(
                "t3", None, "result", false,
            )]))),
        ]);
        assert_eq!(
            items,
            vec![
                ReplayItem::Assistant {
                    text: "running it".into()
                },
                ReplayItem::ToolResult {
                    // The result block carries no name; the call does.
                    id: "t3".into(),
                    name: "grep".into(),
                    text: "result".into(),
                    full: None,
                    is_error: false,
                },
            ]
        );
    }
}
