//! VCC compaction on animist's `VccCompactor`, with craft's harness
//! knowledge in a [`VccExtractor`]: tool-specific breadcrumbs
//! (compressed bash commands, file paths, patch targets, move endpoints),
//! noise-tool suppression, and XML-wrapper stripping. The cut,
//! budget, and prior-summary merge rules are animist's.

mod cut;
mod util;

use std::collections::HashSet;
use std::sync::LazyLock;

use animist::history::{
    CompactionEdit, CompactionPlan, Compactor, ContentBlock, HistorySnapshot, StoredMessage,
    VccCompactor, VccExtraction, VccExtractor,
};
use async_trait::async_trait;

use crate::history::{ext, marker};

pub(crate) use cut::find_cut;
use util::{clip, compress_bash, extract_path, first_line};

use super::llm::COMPACTION_TAG;

/// The VCC kind value for the `craft.compaction` tag.
pub(crate) const COMPACTION_VCC: &str = "vcc";

/// animist's default recent-tail protection (8 messages), kept as the stage
/// configuration so the number is named where it is chosen.
const KEEP_RECENT_MESSAGES: usize = 8;

/// animist's default per-breadcrumb excerpt cap.
const MAX_EXCERPT_CHARS: usize = 512;

/// animist's default whole-summary budget in Unicode scalar values.
const MAX_SUMMARY_CHARS: usize = 16_000;

/// Tools whose calls and results are noise in a summary; the lowercase
/// spellings are craft's own tools.
static NOISE_TOOLS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "TodoWrite",
        "TodoRead",
        "todo_write",
        "ToolSearch",
        "WebSearch",
        "websearch",
        "AskUser",
        "ask",
        "ExitSpecMode",
        "GenerateDroid",
    ]
    .into_iter()
    .collect()
});

/// Craft tools whose *call* breadcrumb already says what happened: their
/// successful results are omitted — a tool result as a breadcrumb is
/// nearly as big as the exchange it replaces, and an
/// output-heavy round would compact to almost nothing. Unknown tools (MCP
/// and friends) keep animist's generic result excerpts.
static ONE_LINER_TOOLS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "Read",
        "read",
        "Edit",
        "edit",
        "Write",
        "write",
        "MultiEdit",
        "multiedit",
        "Glob",
        "glob",
        "Grep",
        "grep",
        "bash",
        "apply_patch",
        "move",
        "retrieve",
    ]
    .into_iter()
    .collect()
});

const NOISE_STRINGS: &[&str] = &[
    "Continue from where you left off.",
    "No response requested.",
    "IMPORTANT: TodoWrite was not called yet.",
];

const XML_WRAPPERS: &[&str] = &[
    "system-reminder",
    "ide_opened_file",
    "command-message",
    "context-window-usage",
];

/// Remove known wrapper elements a harness injects around real content.
fn strip_xml_wrappers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find("<") {
        let after = &remaining[start..];
        let mut matched = false;
        for tag in XML_WRAPPERS {
            let open = format!("<{tag}");
            if after.starts_with(&open) {
                let close = format!("</{tag}>");
                if let Some(end) = after.find(&close) {
                    out.push_str(&remaining[..start]);
                    remaining = &after[end + close.len()..];
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            out.push_str(&remaining[..start + 1]);
            remaining = &remaining[start + 1..];
        }
    }
    out.push_str(remaining);
    out
}

fn is_noise_user_text(text: &str) -> bool {
    let trimmed = text.trim();
    if NOISE_STRINGS.iter().any(|s| trimmed.contains(s)) {
        return true;
    }
    strip_xml_wrappers(trimmed).trim().is_empty()
}

/// Craft's harness knowledge for VCC breadcrumbs. The extractor sees the
/// latest stored message, so altered results arrive as their compressed or
/// marker revision — the right input for a breadcrumb. Entries whose
/// content is protected by the engine's carry anchor (unanswered input)
/// are omitted entirely: the summary must not describe what stays verbatim.
pub(crate) struct CraftVccExtractor {
    /// Serialized content of the protected messages, omitted from the
    /// summary (carry protection; empty between turns).
    protected: HashSet<String>,
}

impl CraftVccExtractor {
    pub(crate) fn new(protected: HashSet<String>) -> Self {
        Self { protected }
    }

    fn is_protected(&self, message: &StoredMessage) -> bool {
        serde_json::to_string(message.message())
            .is_ok_and(|encoded| self.protected.contains(&encoded))
    }
}

/// The one-line breadcrumb shape for a tool call: the file/pattern/query
/// argument names the target.
fn tool_call_breadcrumbs(name: &str, input: &serde_json::Value) -> Vec<String> {
    let field = match name {
        "Read" | "read" | "Edit" | "edit" | "Write" | "write" | "Glob" | "glob" | "Grep"
        | "grep" => input
            .get("file_path")
            .or_else(|| input.get("pattern"))
            .and_then(|value| value.as_str()),
        _ => None,
    };
    if let Some(field) = field {
        return vec![format!("* {name} \"{field}\"")];
    }
    if name.eq_ignore_ascii_case("bash") {
        // Craft's semantic command compression: first meaningful line, cd
        // prefix and pipe tails stripped, capped.
        let command = input
            .get("command")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        return vec![format!("* bash \"{}\"", compress_bash(command))];
    }
    if let Some(path) = extract_path(input) {
        return vec![format!("* {name} \"{path}\"")];
    }
    if let Some(query) = input.get("query").and_then(|value| value.as_str()) {
        return vec![format!("* {name} \"{}\"", clip(query, 60))];
    }
    if name == "apply_patch"
        && let Some(patch) = input.get("patch_text").and_then(|value| value.as_str())
    {
        // Every file a patch touches earns its own breadcrumb.
        return crate::tools::patch_paths(patch)
            .into_iter()
            .map(|path| format!("* apply_patch \"{path}\""))
            .collect();
    }
    if name == "move" {
        let source = input.get("source").and_then(|value| value.as_str());
        let destination = input.get("destination").and_then(|value| value.as_str());
        if let (Some(source), Some(destination)) = (source, destination) {
            return vec![format!("* move \"{source}\" -> \"{destination}\"")];
        }
    }
    vec![format!("* {name}")]
}

#[async_trait]
impl VccExtractor for CraftVccExtractor {
    async fn extract(
        &self,
        message: &StoredMessage,
        block: &ContentBlock,
    ) -> Result<VccExtraction, animist::Error> {
        // Marker observations (cancel, end, failure) are harness bookkeeping,
        // not conversation content.
        if marker::of(message).is_some() || self.is_protected(message) {
            return Ok(VccExtraction::Omit);
        }
        match block {
            ContentBlock::ToolUse { name, input, .. } => {
                if NOISE_TOOLS.contains(name.as_str()) {
                    return Ok(VccExtraction::Omit);
                }
                Ok(VccExtraction::Breadcrumbs(tool_call_breadcrumbs(
                    name, input,
                )))
            }
            ContentBlock::ToolResult {
                name,
                is_error,
                content,
                ..
            } => {
                let name = name.as_deref().unwrap_or("tool");
                if NOISE_TOOLS.contains(name) {
                    return Ok(VccExtraction::Omit);
                }
                if *is_error {
                    // The failure is signal even when the tool's success
                    // output would be noise.
                    let text = ext::tool_content_text(content);
                    return Ok(VccExtraction::Breadcrumbs(vec![format!(
                        "! {name} failed: {}",
                        first_line(text.trim(), 120)
                    )]));
                }
                // Craft's own tools carry their outcome in the call's
                // breadcrumb; unknown tools keep animist's generic excerpt.
                if ONE_LINER_TOOLS.contains(name) {
                    Ok(VccExtraction::Omit)
                } else {
                    Ok(VccExtraction::Fallback)
                }
            }
            ContentBlock::Text { text } => {
                if ext::is_user(message.message()) && is_noise_user_text(text) {
                    return Ok(VccExtraction::Omit);
                }
                Ok(VccExtraction::Fallback)
            }
            _ => Ok(VccExtraction::Fallback),
        }
    }
}

/// Build the VCC stage's plan for `snapshot`: animist's cut and breadcrumb
/// budget, craft's extraction, carry protection clamping the cut, and the
/// `craft.compaction = vcc` tag on the generated summary. `summarize_end`
/// is the first protected position in the active view (`usize::MAX` when
/// nothing is protected). Returns `None` when the stage declines (no safe
/// boundary, empty prefix, everything protected).
///
/// The summary budget is a character budget of its own: mapping the
/// overhead-subtracted token target onto a character budget collapses to
/// zero for large schemas — "a total budget too small for any breadcrumb
/// produces no edits".
pub(crate) async fn build_vcc_plan(
    snapshot: &HistorySnapshot,
    summarize_end: usize,
) -> Option<CompactionPlan> {
    let compactor = VccCompactor::builder()
        .keep_recent_messages(KEEP_RECENT_MESSAGES)
        .max_summary_chars(MAX_SUMMARY_CHARS)
        .max_excerpt_chars(MAX_EXCERPT_CHARS)
        .build()
        .with_extractor(CraftVccExtractor::new(protected_content(
            snapshot,
            summarize_end,
        )));
    let plan = compactor.call(snapshot).await.ok()?;
    clamp_plan(plan, snapshot, summarize_end)
}

/// The serialized content of every active entry at or after the protected
/// boundary, which the extractor omits from the summary.
fn protected_content(snapshot: &HistorySnapshot, summarize_end: usize) -> HashSet<String> {
    let mut protected = HashSet::new();
    if summarize_end == usize::MAX {
        return protected;
    }
    snapshot
        .entries()
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| entry.current().map(|stored| (index, stored)))
        .skip(summarize_end)
        .for_each(|(_, stored)| {
            if let Ok(encoded) = serde_json::to_string(stored.message()) {
                protected.insert(encoded);
            }
        });
    protected
}

/// Clamp a VCC plan to the carry protection: no protected entry is removed,
/// and the summary lands before the first kept entry. Protected content was
/// already omitted from the summary by the extractor.
fn clamp_plan(
    mut plan: CompactionPlan,
    snapshot: &HistorySnapshot,
    summarize_end: usize,
) -> Option<CompactionPlan> {
    if summarize_end == usize::MAX {
        return (!plan.edits.is_empty()).then_some(tag_vcc_summary(plan));
    }
    let active: Vec<usize> = snapshot
        .entries()
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| entry.current().is_some().then_some(index))
        .collect();
    let first_protected = active.get(summarize_end).copied();
    plan.edits.retain(|edit| match edit {
        CompactionEdit::Remove { index } => Some(*index) < first_protected,
        _ => true,
    });
    let removes: HashSet<usize> = plan
        .edits
        .iter()
        .filter_map(|edit| match edit {
            CompactionEdit::Remove { index } => Some(*index),
            _ => None,
        })
        .collect();
    if removes.is_empty() {
        return None;
    }
    // Re-anchor the summary before the first kept entry.
    let before = active
        .iter()
        .find(|index| !removes.contains(index))
        .copied()
        .unwrap_or(snapshot.entries().len());
    for edit in &mut plan.edits {
        if let CompactionEdit::Insert { before: at, .. } = edit {
            *at = before;
        }
    }
    Some(tag_vcc_summary(plan))
}

/// Add the craft tag to the generated summary message, keeping animist's
/// own `animist.compaction = vcc` tag intact.
fn tag_vcc_summary(mut plan: CompactionPlan) -> CompactionPlan {
    for edit in &mut plan.edits {
        if let CompactionEdit::Insert { message, .. } = edit {
            message
                .tags_mut()
                .insert(COMPACTION_TAG.to_owned(), COMPACTION_VCC.to_owned());
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::test_support::{
        stored_assistant_tool_args, stored_tool_result_of, stored_user,
    };

    fn history_with_tail() -> Vec<StoredMessage> {
        let mut msgs = Vec::new();
        for i in 0..8 {
            msgs.push(stored_user(&format!(
                "do task {i} with a fairly long instruction"
            )));
            msgs.push(stored_assistant_tool_args(
                &format!("t{i}"),
                "bash",
                serde_json::json!({"command": format!("echo step{i}")}),
            ));
            msgs.push(stored_tool_result_of(
                &format!("t{i}"),
                &format!("output {i}"),
            ));
        }
        msgs.push(stored_user("second task begins here"));
        for i in 8..16 {
            msgs.push(stored_user(&format!(
                "do subtask {i} with a fairly long instruction"
            )));
            msgs.push(stored_assistant_tool_args(
                &format!("t{i}"),
                "bash",
                serde_json::json!({"command": format!("echo step{i}")}),
            ));
            msgs.push(stored_tool_result_of(
                &format!("t{i}"),
                &format!("output {i}"),
            ));
        }
        msgs
    }

    async fn snapshot(messages: &[StoredMessage]) -> animist::history::HistorySnapshot {
        animist::history::History::from_entries(ext::entries(messages.to_vec()))
            .snapshot()
            .await
    }

    #[tokio::test]
    async fn vcc_plan_compacts_a_long_history_with_craft_breadcrumbs() {
        let messages = history_with_tail();
        let snap = snapshot(&messages).await;
        let plan = build_vcc_plan(&snap, usize::MAX).await.expect("plan");
        let (removes, insert) = split(&plan);
        assert!(!removes.is_empty(), "a long history has a compactable head");
        assert!(removes.len() < messages.len(), "a tail is kept");
        let summary = ext::message_text(&insert);
        // animist's preamble leads; craft's bash breadcrumbs ride inside.
        assert!(summary.contains("Earlier conversation"));
        assert!(
            summary.contains("* bash \"echo step"),
            "craft's compressed command breadcrumbs: {summary}"
        );
        // Users keep animist's generic prompt breadcrumbs.
        assert!(summary.contains("do task 0"));
        assert_eq!(
            insert.tags().get(COMPACTION_TAG).map(String::as_str),
            Some(COMPACTION_VCC)
        );
        assert_eq!(
            insert.tags().get("animist.compaction").map(String::as_str),
            Some("vcc")
        );
    }

    #[tokio::test]
    async fn vcc_plan_declines_a_short_history() {
        let messages = vec![stored_user("hi"), stored_user("there")];
        let snap = snapshot(&messages).await;
        assert!(build_vcc_plan(&snap, usize::MAX).await.is_none());
    }

    #[tokio::test]
    async fn vcc_plan_declines_when_everything_is_protected() {
        let messages = history_with_tail();
        let snap = snapshot(&messages).await;
        assert!(
            build_vcc_plan(&snap, 0).await.is_none(),
            "no removable head when the whole view is protected"
        );
    }

    #[tokio::test]
    async fn carry_protection_keeps_protected_entries_verbatim_and_out_of_the_summary() {
        let messages = history_with_tail();
        let snap = snapshot(&messages).await;
        // Protect the final prompt and its exchange (the last three
        // messages: prompt, call, result).
        let protect_at = messages.len() - 3;
        let plan = build_vcc_plan(&snap, protect_at).await.expect("plan");
        let (removes, insert) = split(&plan);
        assert!(
            removes.iter().all(|index| *index < protect_at),
            "protected entries are never removed"
        );
        let summary = ext::message_text(&insert);
        assert!(
            !summary.contains("do subtask 15"),
            "protected content must not be summarized: {summary}"
        );
    }

    /// Whole-tool-exchange safety comes from animist's cut; pin that no plan
    /// splits a call from its result.
    #[tokio::test]
    async fn vcc_plan_never_splits_a_tool_exchange() {
        let messages = history_with_tail();
        let snap = snapshot(&messages).await;
        let plan = build_vcc_plan(&snap, usize::MAX).await.expect("plan");
        let (removes, _) = split(&plan);
        let exchange_id = |block: &ContentBlock| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        };
        let ids = |removed: bool| -> HashSet<String> {
            messages
                .iter()
                .enumerate()
                .filter(|(index, _)| removes.contains(index) == removed)
                .flat_map(|(_, message)| message.message().content.iter().filter_map(&exchange_id))
                .collect()
        };
        let removed_ids = ids(true);
        let kept_ids = ids(false);
        assert!(
            removed_ids.is_disjoint(&kept_ids),
            "every tool exchange must be removed whole or kept whole"
        );
    }

    fn split(plan: &CompactionPlan) -> (HashSet<usize>, StoredMessage) {
        let removes: HashSet<usize> = plan
            .edits
            .iter()
            .filter_map(|edit| match edit {
                CompactionEdit::Remove { index } => Some(*index),
                _ => None,
            })
            .collect();
        let insert = plan
            .edits
            .iter()
            .find_map(|edit| match edit {
                CompactionEdit::Insert { message, .. } => Some(message.clone()),
                _ => None,
            })
            .expect("summary insert");
        (removes, insert)
    }

    #[tokio::test]
    async fn extractor_omits_noise_tools_and_markers() {
        let extractor = CraftVccExtractor::new(HashSet::new());
        let todo_call = ext::stored(crate::history::ext::assistant_text("calling"));
        let block = ContentBlock::ToolUse {
            id: "t1".into(),
            name: "todo_write".into(),
            input: serde_json::json!({}),
            thought_signature: None,
        };
        assert!(matches!(
            extractor.extract(&todo_call, &block).await.unwrap(),
            VccExtraction::Omit
        ));
        let marker_message = crate::history::marker::observation(
            crate::history::marker::CANCEL,
            "[Cancelled by user]",
        );
        let text = ContentBlock::Text {
            text: "[Cancelled by user]".into(),
        };
        assert!(matches!(
            extractor.extract(&marker_message, &text).await.unwrap(),
            VccExtraction::Omit
        ));
    }

    #[tokio::test]
    async fn extractor_omits_wrapper_only_user_text() {
        let extractor = CraftVccExtractor::new(HashSet::new());
        let user = ext::stored_user_text("<system-reminder>noise</system-reminder>");
        let block = ContentBlock::Text {
            text: "<system-reminder>noise</system-reminder>".into(),
        };
        assert!(matches!(
            extractor.extract(&user, &block).await.unwrap(),
            VccExtraction::Omit
        ));
    }

    #[tokio::test]
    async fn extractor_emits_craft_tool_breadcrumbs() {
        let extractor = CraftVccExtractor::new(HashSet::new());
        let assistant = ext::stored(crate::history::ext::assistant_text("calling"));
        let bash = ContentBlock::ToolUse {
            id: "t1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "cd /tmp && cargo test --lib | tail -5"}),
            thought_signature: None,
        };
        match extractor.extract(&assistant, &bash).await.unwrap() {
            VccExtraction::Breadcrumbs(lines) => {
                assert_eq!(lines.len(), 1);
                assert!(
                    lines[0].contains("cargo test --lib"),
                    "cd prefix and pipe tail stripped: {}",
                    lines[0]
                );
                assert!(lines[0].starts_with("* bash \""));
            }
            other => panic!("bash breadcrumb, got {other:?}"),
        }
        let read = ContentBlock::ToolUse {
            id: "t2".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "src/main.rs"}),
            thought_signature: None,
        };
        match extractor.extract(&assistant, &read).await.unwrap() {
            VccExtraction::Breadcrumbs(lines) => {
                assert_eq!(lines[0], "* read \"src/main.rs\"");
            }
            other => panic!("read breadcrumb, got {other:?}"),
        }
        let failed = ContentBlock::ToolResult {
            tool_use_id: "t3".into(),
            name: Some("bash".into()),
            content: "error: could not compile\nnote: see logs".into(),
            is_error: true,
            loaded_tools: Vec::new(),
        };
        match extractor.extract(&assistant, &failed).await.unwrap() {
            VccExtraction::Breadcrumbs(lines) => {
                assert_eq!(lines[0], "! bash failed: error: could not compile");
            }
            other => panic!("error breadcrumb, got {other:?}"),
        }
    }
}
