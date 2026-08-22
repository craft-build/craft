use agent_client_protocol_schema::v1::{
    Content, ContentBlock, ContentChunk, Cost, Diff, ImageContent, SessionUpdate, StopReason,
    TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use craft_agent::DoneReason;
use craft_agent::FlowProgress;
use craft_agent::tools::ToolRegistry;
use craft_agent::types::{
    BatchProgressEvent, ToolDoneEvent, ToolOutput, ToolStartEvent, TurnCompleteEvent,
};
use craft_providers::{
    ContentBlock as MsgBlock, ImageMediaType, ImageSource, Message, Role as MsgRole,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MIN_FENCE_LEN: usize = 3;
/// Model pricing is quoted in US dollars, so that is the reported currency.
const CURRENCY: &str = "USD";
pub const SUBAGENT_BREADCRUMB_ARROW: &str = "▸ ";
pub const SUBAGENT_FAILURE_MARKER: &str = "(failed)";

fn fenced(text: &str) -> String {
    let longest_backtick_run = text
        .split(|c: char| c != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(MIN_FENCE_LEN.max(longest_backtick_run + 1));
    format!("{fence}\n{text}\n{fence}")
}

pub fn tool_kind(name: &str) -> ToolKind {
    if let Some(kind_str) = ToolRegistry::native()
        .get(name)
        .and_then(|e| e.tool.tool_kind().map(str::to_owned))
    {
        return parse_tool_kind(&kind_str);
    }
    match name {
        "bash" | "bash_status" | "bash_watch" | "bash_kill" => ToolKind::Execute,
        "grep" | "glob" => ToolKind::Search,
        "webfetch" | "websearch" => ToolKind::Fetch,
        "skill" => ToolKind::Read,
        _ => ToolKind::Other,
    }
}

fn parse_tool_kind(s: &str) -> ToolKind {
    match s {
        "read" => ToolKind::Read,
        "edit" => ToolKind::Edit,
        "delete" => ToolKind::Delete,
        "move" => ToolKind::Move,
        "search" => ToolKind::Search,
        "execute" => ToolKind::Execute,
        "think" => ToolKind::Think,
        "fetch" => ToolKind::Fetch,
        "switch_mode" => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

pub fn text_delta(text: &str) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text.to_string(),
    ))))
}

pub fn thinking_delta(text: &str) -> SessionUpdate {
    SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text.to_string(),
    ))))
}

/// Render a Flow mode progress event as a thought breadcrumb for ACP clients.
/// Returns `None` for terminal events (`GoalReady`/`Done`/`NeedsReview`/
/// `Failed`/`Cancelled`) because the headless Flow driver already surfaces
/// those as agent messages / done / error events, so a duplicate thought
/// would be noise. Structural events (turn-type entered, thread spawn/exit)
/// carry the live thread-tree signal that has no other channel.
pub fn flow_progress(progress: &FlowProgress) -> Option<SessionUpdate> {
    let text = match progress {
        FlowProgress::TurnTypeEntered {
            thread_id,
            turn_type,
        } => format!(
            "flow ▸ entered {} in thread {thread_id}",
            turn_type.as_str()
        ),
        FlowProgress::ThreadSpawn {
            thread_id,
            parent_id,
            turn_type,
        } => format!(
            "flow ▸ spawned {} thread {thread_id} under {parent_id}",
            turn_type.as_str()
        ),
        FlowProgress::ThreadExit {
            thread_id,
            returning_to,
        } => format!("flow ▸ thread {thread_id} done -> {returning_to}"),
        FlowProgress::AdvisorNote {
            thread_id,
            addressed_to,
            severity,
            message,
        } => format!(
            "flow ▸ advisor {severity:?} for thread {thread_id} -> {addressed_to}: {message}"
        ),
        FlowProgress::GoalReady { .. }
        | FlowProgress::Done { .. }
        | FlowProgress::NeedsReview { .. }
        | FlowProgress::Failed { .. }
        | FlowProgress::Cancelled => return None,
    };
    Some(thinking_delta(&text))
}

/// Render the always-on advisor's note as a thought chunk, mirroring the
/// terminal's `[advisor:{severity}] {message}` display.
pub fn advisor_note(severity: &str, message: &str) -> SessionUpdate {
    thinking_delta(&format!("advisor ▸ {severity}: {message}"))
}

/// Surface an agent info message (e.g. advisor lifecycle notices) as a
/// thought chunk. Clients otherwise never see these; the terminal renders
/// them inline.
pub fn info(message: &str) -> SessionUpdate {
    thinking_delta(message)
}

pub fn user_message_chunk(text: &str) -> SessionUpdate {
    SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text.to_string(),
    ))))
}

/// Echo an image the client attached to a prompt (or that replay surfaces in
/// a user message) so the client transcript can render it.
pub fn user_image_chunk(source: &ImageSource) -> SessionUpdate {
    SessionUpdate::UserMessageChunk(ContentChunk::new(image_content_block(source)))
}

fn image_content_block(source: &ImageSource) -> ContentBlock {
    ContentBlock::Image(ImageContent::new(
        source.data.to_string(),
        mime_type(&source.media_type),
    ))
}

fn image_tool_content(source: &ImageSource) -> ToolCallContent {
    ToolCallContent::Content(Content::new(image_content_block(source)))
}

pub fn subagent_breadcrumb(label: &str) -> String {
    format!("\n{SUBAGENT_BREADCRUMB_ARROW}{label}\n")
}

pub fn subagent_content_update(parent_id: &str, content: &str) -> SessionUpdate {
    let fields = ToolCallUpdateFields::new().content(vec![ToolCallContent::Content(Content::new(
        ContentBlock::Text(TextContent::new(content.to_string())),
    ))]);
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(parent_id.to_string()),
        fields,
    ))
}

pub fn tool_pending(id: &str, name: &str) -> SessionUpdate {
    let kind = tool_kind(name);
    SessionUpdate::ToolCall(
        ToolCall::new(ToolCallId::from(id.to_string()), name.to_string())
            .kind(kind)
            .status(ToolCallStatus::Pending),
    )
}

pub fn tool_start(event: &ToolStartEvent, cwd: &Path, home: Option<&Path>) -> SessionUpdate {
    let mut fields = ToolCallUpdateFields::new()
        .status(ToolCallStatus::InProgress)
        .title(event.summary.clone());

    if let Some(raw) = &event.raw_input {
        fields = fields.raw_input(raw.clone());
    }

    let locations = tool_locations(&event.tool, event.raw_input.as_ref(), cwd, home);
    if !locations.is_empty() {
        fields = fields.locations(locations);
    }

    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(event.id.clone()),
        fields,
    ))
}

/// File-level tools report a location so the client can follow along. Directory
/// scoped tools (glob, grep, list) and commands (bash) target no single file.
const FILE_TOOLS: &[&str] = &[
    "read",
    "write",
    "edit",
    "multiedit",
    "edit_lines",
    "insert_lines",
    "index",
    "view_image",
];

/// File locations the tool call touches, per ACP "Following the Agent". The
/// client expects absolute paths, so `~` and relative paths are resolved
/// against the session's home and cwd.
fn tool_locations(
    tool: &str,
    raw_input: Option<&serde_json::Value>,
    cwd: &Path,
    home: Option<&Path>,
) -> Vec<ToolCallLocation> {
    if !FILE_TOOLS.contains(&tool) {
        return Vec::new();
    }
    let Some(raw) = raw_input else {
        return Vec::new();
    };
    let Some(path) = input_path(raw) else {
        return Vec::new();
    };
    let Some(resolved) = resolve_path(path, cwd, home) else {
        return Vec::new();
    };
    vec![location(resolved, input_line(tool, raw))]
}

/// The target file: `path`, or its schema alias `file_path` when the model
/// used that spelling.
fn input_path(raw_input: &serde_json::Value) -> Option<&str> {
    raw_input
        .get("path")
        .or_else(|| raw_input.get("file_path"))?
        .as_str()
        .filter(|s| !s.is_empty())
}

/// `~`, `~/x`, and relative paths become absolute; other `~` spellings
/// (`~user`) have no expansion. ACP clients require absolute paths in
/// locations, so anything unresolvable yields None instead of a bogus path.
fn resolve_path(raw: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if raw.starts_with('~') {
        if raw == "~" {
            return home.map(Path::to_path_buf);
        }
        let rest = raw.strip_prefix("~/")?;
        return home.map(|h| h.join(rest));
    }
    let p = Path::new(raw);
    Some(if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    })
}

/// The line a tool call focuses on, when its input says so. ACP `line` is
/// 0-based (Zed uses it as a buffer row), but tool inputs are 1-based.
fn input_line(tool: &str, raw_input: &serde_json::Value) -> Option<u32> {
    let key = match tool {
        "read" => "offset",
        "edit_lines" => "start",
        // insert_lines writes *after* `line`, so the new text's 0-based row
        // is the raw value itself, and 0 (insert at the top) is valid.
        "insert_lines" => return raw_input.get("line").and_then(as_number),
        _ => return None,
    };
    raw_input.get(key).and_then(as_line).map(|l| l - 1)
}

fn as_line(v: &serde_json::Value) -> Option<u32> {
    as_number(v).filter(|&l| l >= 1)
}

/// raw_input is pre-validation, so models sometimes send numbers as strings.
fn as_number(v: &serde_json::Value) -> Option<u32> {
    let n = v
        .as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))?;
    u32::try_from(n).ok()
}

fn location(path: PathBuf, line: Option<u32>) -> ToolCallLocation {
    let mut loc = ToolCallLocation::new(path);
    if let Some(l) = line {
        loc = loc.line(l);
    }
    loc
}

/// A finished call's location comes from what it wrote. File tools already
/// reported that path at start, sometimes with a line, and an update replaces
/// locations instead of merging them, so re-reporting would drop the line the
/// client is following. Paths are already absolute (the tools abspath them),
/// so resolve_path is a no-op here; it only guards against a relative slip.
fn done_locations(event: &ToolDoneEvent, cwd: &Path, home: Option<&Path>) -> Vec<ToolCallLocation> {
    if event.is_error || FILE_TOOLS.contains(&&*event.tool) {
        return Vec::new();
    }
    let Some(path) = event.written_path.as_deref() else {
        return Vec::new();
    };
    let Some(resolved) = resolve_path(path, cwd, home) else {
        return Vec::new();
    };
    vec![location(resolved, None)]
}

pub fn batch_inner_start(event: &BatchProgressEvent) -> SessionUpdate {
    let title = event.summary.clone().unwrap_or_else(|| event.tool.clone());
    SessionUpdate::ToolCall(
        ToolCall::new(
            ToolCallId::from(format!("{}__{}", event.batch_id, event.index)),
            title,
        )
        .kind(tool_kind(&event.tool))
        .status(ToolCallStatus::InProgress),
    )
}

const AUTO_REVIEW_IN_PROGRESS: &str = "auto-review in progress\u{2026}";

fn verdict_label(verdict: &str) -> &str {
    match verdict.trim().to_ascii_lowercase().as_str() {
        "allow" | "approved" | "approve" | "yes" | "true" => "allowed",
        _ => "denied",
    }
}

pub fn auto_review_start(id: &str) -> SessionUpdate {
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(id.to_string()),
        ToolCallUpdateFields::new().title(AUTO_REVIEW_IN_PROGRESS.to_string()),
    ))
}

pub fn auto_review_decision(id: &str, verdict: &str, risk: &str, rationale: &str) -> SessionUpdate {
    let label = verdict_label(verdict);
    let title = format!("auto-review {label} (risk: {risk}): {rationale}");
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(id.to_string()),
        ToolCallUpdateFields::new().title(title),
    ))
}

pub fn tool_output(id: &str, content: &str) -> SessionUpdate {
    let fields = ToolCallUpdateFields::new().content(vec![ToolCallContent::Content(Content::new(
        ContentBlock::Text(TextContent::new(fenced(content))),
    ))]);
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(id.to_string()),
        fields,
    ))
}

pub fn tool_done(event: &ToolDoneEvent, cwd: &Path, home: Option<&Path>) -> SessionUpdate {
    let status = if event.is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };

    let content = match &event.output {
        ToolOutput::Diff {
            path,
            before,
            after,
            ..
        } => {
            let diff = if before.is_empty() {
                Diff::new(path.as_str(), after.clone())
            } else {
                Diff::new(path.as_str(), after.clone()).old_text(before.clone())
            };
            vec![ToolCallContent::Diff(diff)]
        }
        _ => {
            let text = event.output.as_text();
            let mut content = Vec::new();
            if !text.is_empty() {
                content.push(ToolCallContent::Content(Content::new(ContentBlock::Text(
                    TextContent::new(fenced(&text)),
                ))));
            }
            if let Some(source) = event.output.image_source() {
                content.push(image_tool_content(source));
            }
            content
        }
    };

    let raw_text = event.output.as_text();
    let mut fields = ToolCallUpdateFields::new().status(status).content(content);
    if !raw_text.is_empty() {
        fields = fields.raw_output(serde_json::Value::String(raw_text));
    }

    let locations = done_locations(event, cwd, home);
    if !locations.is_empty() {
        fields = fields.locations(locations);
    }

    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(event.id.clone()),
        fields,
    ))
}

pub fn map_done_reason(reason: DoneReason) -> StopReason {
    match reason {
        DoneReason::EndTurn => StopReason::EndTurn,
        DoneReason::MaxTokens => StopReason::MaxTokens,
        DoneReason::MaxTurns => StopReason::MaxTurnRequests,
        DoneReason::Cancelled => StopReason::Cancelled,
        // The approval gate is a Craft-only pause; ACP has no equivalent, so
        // surface it as EndTurn (the ACP session simply ends from its view).
        DoneReason::AwaitingGoalApproval => StopReason::EndTurn,
    }
}

/// Per ACP "Session Usage Updates": the current context gauge plus the
/// session's cumulative cost. Each turn's event only carries its own turn's
/// share, so the caller tracks the running total across turns.
pub fn usage_update(event: &TurnCompleteEvent, cost_total: Option<f64>) -> SessionUpdate {
    let used = event
        .context_size
        .unwrap_or_else(|| event.usage.context_tokens()) as u64;
    let mut update = UsageUpdate::new(used, u64::from(event.context_window));
    if let Some(cost) = cost_total {
        update = update.cost(Cost::new(cost, CURRENCY));
    }
    SessionUpdate::UsageUpdate(update)
}

pub fn replay_history(messages: &[Message], cwd: &Path, home: Option<&Path>) -> Vec<SessionUpdate> {
    let tool_inputs: HashMap<String, &serde_json::Value> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            MsgBlock::ToolUse { id, input, .. } => Some((id.clone(), input)),
            _ => None,
        })
        .collect();

    let mut updates = Vec::new();
    for msg in messages {
        match msg.role {
            MsgRole::User => replay_user(msg, &mut updates, &tool_inputs),
            MsgRole::Assistant => replay_assistant(msg, &mut updates, cwd, home),
        }
    }
    updates
}

fn replay_user(
    msg: &Message,
    updates: &mut Vec<SessionUpdate>,
    tool_inputs: &HashMap<String, &serde_json::Value>,
) {
    if msg.is_observation() {
        return;
    }
    if let Some(text) = msg.user_text() {
        updates.push(SessionUpdate::UserMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text.to_string())),
        )));
    }
    for block in &msg.content {
        match block {
            MsgBlock::ToolResult {
                tool_use_id,
                content,
                images,
                is_error,
            } => updates.push(replay_tool_result(
                tool_use_id,
                content,
                images,
                *is_error,
                tool_inputs.get(tool_use_id).copied(),
            )),
            MsgBlock::Image { source } => {
                updates.push(user_image_chunk(source));
            }
            _ => {}
        }
    }
}

fn replay_assistant(
    msg: &Message,
    updates: &mut Vec<SessionUpdate>,
    cwd: &Path,
    home: Option<&Path>,
) {
    for block in &msg.content {
        match block {
            MsgBlock::Text { text } => updates.push(text_delta(text)),
            MsgBlock::Thinking { thinking, .. } => updates.push(thinking_delta(thinking)),
            MsgBlock::ToolUse {
                id, name, input, ..
            } => {
                updates.push(replay_tool_call(id, name, input, cwd, home));
            }
            _ => {}
        }
    }
}

fn replay_tool_call(
    id: &str,
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
    home: Option<&Path>,
) -> SessionUpdate {
    SessionUpdate::ToolCall(
        ToolCall::new(ToolCallId::from(id.to_string()), name.to_string())
            .kind(tool_kind(name))
            .status(ToolCallStatus::Pending)
            .raw_input(input.clone())
            .locations(tool_locations(name, Some(input), cwd, home)),
    )
}

fn replay_tool_result(
    id: &str,
    content: &str,
    images: &[ImageSource],
    is_error: bool,
    input: Option<&serde_json::Value>,
) -> SessionUpdate {
    let status = if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };
    let mut fields = ToolCallUpdateFields::new().status(status);

    if let Some(diff) = input.and_then(reconstruct_diff) {
        fields = fields.content(vec![ToolCallContent::Diff(diff)]);
    } else {
        let mut tool_content = Vec::new();
        if !content.is_empty() {
            tool_content.push(ToolCallContent::Content(Content::new(ContentBlock::Text(
                TextContent::new(fenced(content)),
            ))));
        }
        tool_content.extend(images.iter().map(image_tool_content));
        fields = fields.content(tool_content);
    }

    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(id.to_string()),
        fields,
    ))
}

fn reconstruct_diff(input: &serde_json::Value) -> Option<Diff> {
    let path = input.get("path").and_then(|v| v.as_str())?;
    let edits: Vec<(String, String)> =
        if let Some(edits_arr) = input.get("edits").and_then(|v| v.as_array()) {
            edits_arr
                .iter()
                .filter_map(|e| {
                    Some((
                        e.get("old_string")?.as_str()?.to_string(),
                        e.get("new_string")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        } else {
            let old = input
                .get("old_string")
                .and_then(|v| v.as_str())?
                .to_string();
            let new = input
                .get("new_string")
                .and_then(|v| v.as_str())?
                .to_string();
            vec![(old, new)]
        };
    if edits.is_empty() {
        return None;
    }
    let old_text = edits
        .iter()
        .map(|(o, _)| o.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let new_text = edits
        .iter()
        .map(|(_, n)| n.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(Diff::new(path.to_string(), new_text).old_text(old_text))
}

fn mime_type(media: &ImageMediaType) -> &'static str {
    match media {
        ImageMediaType::Png => "image/png",
        ImageMediaType::Jpeg => "image/jpeg",
        ImageMediaType::Gif => "image/gif",
        ImageMediaType::Webp => "image/webp",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use craft_providers::ImageSource;
    use craft_providers::TokenUsage;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const CWD: &str = "/home/user/project";
    const HOME: &str = "/home/user";

    #[test_case("1: mod render\n2: mod segment", "```\n1: mod render\n2: mod segment\n```" ; "plain_text_gets_default_fence")]
    #[test_case("has ```rust\ncode\n``` inside", "````\nhas ```rust\ncode\n``` inside\n````" ; "fence_longer_than_inner_backticks")]
    fn fenced_wraps_in_code_block(input: &str, expected: &str) {
        assert_eq!(fenced(input), expected);
    }

    #[test]
    fn breadcrumb_wraps_label_with_arrow() {
        assert_eq!(subagent_breadcrumb("grep: x"), "\n\u{25b8} grep: x\n");
    }

    #[test]
    fn breadcrumb_failure_marker_is_text() {
        assert_eq!(SUBAGENT_FAILURE_MARKER, "(failed)");
    }

    #[test]
    fn subagent_content_update_targets_parent_id() {
        let json = serde_json::to_value(subagent_content_update("parent_tu", "narration")).unwrap();
        assert_eq!(json["sessionUpdate"], "tool_call_update");
        assert_eq!(json["toolCallId"], "parent_tu");
        assert_eq!(json["content"][0]["content"]["text"], "narration");
        let text = json["content"][0]["content"]["text"].as_str().unwrap();
        assert!(!text.starts_with('`'), "text must not be a code fence");
    }

    #[test]
    fn auto_review_start_marks_tool_title_in_progress() {
        let json = serde_json::to_value(auto_review_start("tu-9")).unwrap();
        assert_eq!(json["sessionUpdate"], "tool_call_update");
        assert_eq!(json["toolCallId"], "tu-9");
        assert_eq!(json["title"], "auto-review in progress\u{2026}");
    }

    #[test_case("allow", "low", "safe read", "auto-review allowed (risk: low): safe read"; "allow_decision")]
    #[test_case("deny", "high", "destructive write", "auto-review denied (risk: high): destructive write"; "deny_decision")]
    #[test_case("approved", "medium", "x", "auto-review allowed (risk: medium): x"; "approved_alias_maps_to_allowed")]
    fn auto_review_decision_matches_tui_text(
        verdict: &str,
        risk: &str,
        rationale: &str,
        expected_title: &str,
    ) {
        let json =
            serde_json::to_value(auto_review_decision("tu-1", verdict, risk, rationale)).unwrap();
        assert_eq!(json["sessionUpdate"], "tool_call_update");
        assert_eq!(json["toolCallId"], "tu-1");
        assert_eq!(json["title"], expected_title);
    }

    #[test_case(Some("import"), "import"; "summary_becomes_title")]
    #[test_case(None, "grep"; "title_falls_back_to_tool_name")]
    fn batch_inner_start_composite_id_and_title(summary: Option<&str>, title: &str) {
        let event = BatchProgressEvent {
            batch_id: "b1".into(),
            index: 2,
            tool: "grep".into(),
            status: craft_agent::BatchToolStatus::InProgress,
            output: None,
            summary: summary.map(Into::into),
        };
        let json = serde_json::to_value(batch_inner_start(&event)).unwrap();
        assert_eq!(json["toolCallId"], "b1__2");
        assert_eq!(json["title"], title);
    }

    /// The only pair whose names disagree, and the one ACP clients read to tell
    /// "the model stopped" from "the agent ran out of turns".
    #[test]
    fn max_turns_maps_to_max_turn_requests() {
        assert_eq!(
            map_done_reason(DoneReason::MaxTurns),
            StopReason::MaxTurnRequests
        );
    }

    fn assistant(content: Vec<MsgBlock>) -> Message {
        Message {
            role: MsgRole::Assistant,
            content,
            display_text: None,
            ..Default::default()
        }
    }

    fn updates_json(messages: &[Message]) -> Vec<serde_json::Value> {
        replay_history(messages, Path::new(CWD), Some(Path::new(HOME)))
            .iter()
            .map(|u| serde_json::to_value(u).unwrap())
            .collect()
    }

    #[test]
    fn replay_full_conversation_in_order() {
        let messages = vec![
            Message::user("hello".into()),
            assistant(vec![
                MsgBlock::Thinking {
                    thinking: "hmm".into(),
                    signature: None,
                },
                MsgBlock::Text {
                    text: "let me check".into(),
                },
                MsgBlock::tool_use("tu-1", "bash", serde_json::json!({"command": "ls"})),
            ]),
            Message {
                role: MsgRole::User,
                content: vec![MsgBlock::ToolResult {
                    tool_use_id: "tu-1".into(),
                    content: "file.rs".into(),
                    images: vec![],
                    is_error: false,
                }],
                display_text: None,
                ..Default::default()
            },
            assistant(vec![MsgBlock::Text {
                text: "done".into(),
            }]),
        ];

        let json = updates_json(&messages);
        assert_eq!(json.len(), 6);
        assert_eq!(json[0]["sessionUpdate"], "user_message_chunk");
        assert_eq!(json[0]["content"]["text"], "hello");
        assert_eq!(json[1]["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(json[1]["content"]["text"], "hmm");
        assert_eq!(json[2]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(json[2]["content"]["text"], "let me check");
        assert_eq!(json[3]["sessionUpdate"], "tool_call");
        assert_eq!(json[3]["toolCallId"], "tu-1");
        assert_eq!(json[3]["kind"], "execute");
        assert_eq!(json[3]["rawInput"]["command"], "ls");
        assert_eq!(json[4]["sessionUpdate"], "tool_call_update");
        assert_eq!(json[4]["toolCallId"], "tu-1");
        assert_eq!(json[4]["status"], "completed");
        assert_eq!(
            json[4]["content"][0]["content"]["text"],
            "```\nfile.rs\n```"
        );
        assert_eq!(json[5]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(json[5]["content"]["text"], "done");
    }

    #[test]
    fn replay_prefers_display_text_over_model_text() {
        let msg = Message::user_display("expanded with context".into(), "what user typed".into());
        let json = updates_json(&[msg]);
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["content"]["text"], "what user typed");
    }

    #[test]
    fn replay_hides_synthetic_messages() {
        assert!(updates_json(&[Message::synthetic("injected".into())]).is_empty());
    }

    #[test]
    fn replay_never_speaks_an_observation_as_the_user() {
        let obs = Message::observation("[monitor] build failed".into());
        assert!(updates_json(&[obs]).is_empty());
    }

    #[test]
    fn replay_failed_tool_result_maps_to_failed_status() {
        let msg = Message {
            role: MsgRole::User,
            content: vec![MsgBlock::ToolResult {
                tool_use_id: "tu-err".into(),
                content: "boom".into(),
                images: vec![],
                is_error: true,
            }],
            display_text: None,
            ..Default::default()
        };
        let json = updates_json(&[msg]);
        assert_eq!(json[0]["sessionUpdate"], "tool_call_update");
        assert_eq!(json[0]["status"], "failed");
    }

    #[test]
    fn replay_edit_produces_diff_content() {
        let messages = vec![
            assistant(vec![MsgBlock::ToolUse {
                id: "tu-edit".into(),
                name: "edit".into(),
                input: serde_json::json!({
                    "path": "src/main.rs",
                    "old_string": "fn old() {}",
                    "new_string": "fn new() {}"
                }),
                thought_signature: None,
            }]),
            Message {
                role: MsgRole::User,
                content: vec![MsgBlock::ToolResult {
                    tool_use_id: "tu-edit".into(),
                    content: "edited src/main.rs".into(),
                    images: vec![],
                    is_error: false,
                }],
                display_text: None,
                ..Default::default()
            },
        ];
        let json = updates_json(&messages);
        let result_update = &json[1];
        assert_eq!(result_update["sessionUpdate"], "tool_call_update");
        assert_eq!(result_update["content"][0]["type"], "diff");
        assert_eq!(result_update["content"][0]["path"], "src/main.rs");
        assert_eq!(result_update["content"][0]["oldText"], "fn old() {}");
        assert_eq!(result_update["content"][0]["newText"], "fn new() {}");
    }

    #[test]
    fn replay_multiedit_merges_edits_into_diff() {
        let messages = vec![
            assistant(vec![MsgBlock::ToolUse {
                id: "tu-multi".into(),
                name: "multiedit".into(),
                input: serde_json::json!({
                    "path": "lib.rs",
                    "edits": [
                        {"old_string": "a", "new_string": "b"},
                        {"old_string": "c", "new_string": "d"}
                    ]
                }),
                thought_signature: None,
            }]),
            Message {
                role: MsgRole::User,
                content: vec![MsgBlock::ToolResult {
                    tool_use_id: "tu-multi".into(),
                    content: "edited lib.rs".into(),
                    images: vec![],
                    is_error: false,
                }],
                display_text: None,
                ..Default::default()
            },
        ];
        let json = updates_json(&messages);
        let result_update = &json[1];
        assert_eq!(result_update["content"][0]["type"], "diff");
        assert_eq!(result_update["content"][0]["oldText"], "a\n\nc");
        assert_eq!(result_update["content"][0]["newText"], "b\n\nd");
    }

    #[test]
    fn replay_non_edit_tool_still_uses_text() {
        let messages = vec![
            assistant(vec![MsgBlock::tool_use(
                "tu-bash",
                "bash",
                serde_json::json!({"command": "echo hi"}),
            )]),
            Message {
                role: MsgRole::User,
                content: vec![MsgBlock::ToolResult {
                    tool_use_id: "tu-bash".into(),
                    content: "hi".into(),
                    images: vec![],
                    is_error: false,
                }],
                display_text: None,
                ..Default::default()
            },
        ];
        let json = updates_json(&messages);
        let result_update = &json[1];
        assert_eq!(result_update["content"][0]["type"], "content");
        assert_eq!(
            result_update["content"][0]["content"]["text"],
            "```\nhi\n```"
        );
    }

    #[test]
    fn replay_user_image_keeps_mime_type() {
        let msg = Message::user_with_images(
            String::new(),
            vec![ImageSource {
                media_type: ImageMediaType::Png,
                data: std::sync::Arc::from("b64data"),
            }],
        );
        let json = updates_json(&[msg]);
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["sessionUpdate"], "user_message_chunk");
        assert_eq!(json[0]["content"]["type"], "image");
        assert_eq!(json[0]["content"]["mimeType"], "image/png");
        assert_eq!(json[0]["content"]["data"], "b64data");
    }

    #[test]
    fn replay_tool_result_includes_image_content() {
        let messages = vec![
            assistant(vec![MsgBlock::tool_use(
                "tu-view",
                "view_image",
                serde_json::json!({"path": "shot.png"}),
            )]),
            Message {
                role: MsgRole::User,
                content: vec![MsgBlock::ToolResult {
                    tool_use_id: "tu-view".into(),
                    content: "[image shot.png]".into(),
                    images: vec![ImageSource {
                        media_type: ImageMediaType::Png,
                        data: std::sync::Arc::from("b64data"),
                    }],
                    is_error: false,
                }],
                display_text: None,
                ..Default::default()
            },
        ];
        let json = updates_json(&messages);
        let result_update = &json[1];
        assert_eq!(result_update["content"][0]["type"], "content");
        assert_eq!(result_update["content"][1]["content"]["type"], "image");
        assert_eq!(result_update["content"][1]["content"]["data"], "b64data");
        assert_eq!(
            result_update["content"][1]["content"]["mimeType"],
            "image/png"
        );
    }

    #[test]
    fn tool_done_emits_image_content_for_image_output() {
        let event = ToolDoneEvent {
            id: "t-img".into(),
            tool: Arc::from("browser"),
            output: ToolOutput::Image {
                caption: "[screenshot of https://a.test]".into(),
                source: ImageSource {
                    media_type: ImageMediaType::Png,
                    data: std::sync::Arc::from("b64shot"),
                },
            },
            is_error: false,
            annotation: None,
            written_path: None,
        };
        let json =
            serde_json::to_value(tool_done(&event, Path::new(CWD), Some(Path::new(HOME)))).unwrap();
        assert_eq!(json["content"][0]["content"]["type"], "text");
        assert_eq!(json["content"][1]["content"]["type"], "image");
        assert_eq!(json["content"][1]["content"]["data"], "b64shot");
        assert_eq!(json["content"][1]["content"]["mimeType"], "image/png");
    }

    #[test_case("read", ToolKind::Read ; "read")]
    #[test_case("edit", ToolKind::Edit ; "edit")]
    #[test_case("delete", ToolKind::Delete ; "delete")]
    #[test_case("move", ToolKind::Move ; "move_kind")]
    #[test_case("search", ToolKind::Search ; "search")]
    #[test_case("execute", ToolKind::Execute ; "execute")]
    #[test_case("think", ToolKind::Think ; "think")]
    #[test_case("fetch", ToolKind::Fetch ; "fetch")]
    #[test_case("switch_mode", ToolKind::SwitchMode ; "switch_mode")]
    #[test_case("other", ToolKind::Other ; "other")]
    #[test_case("bogus", ToolKind::Other ; "unknown_maps_to_other")]
    fn parse_tool_kind_maps_wire_strings(input: &str, expected: ToolKind) {
        assert_eq!(parse_tool_kind(input), expected);
    }

    #[test_case("bash", ToolKind::Execute ; "bash_lua_name_fallback")]
    #[test_case("grep", ToolKind::Search ; "grep_lua_name_fallback")]
    #[test_case("websearch", ToolKind::Fetch ; "websearch_lua_name_fallback")]
    #[test_case("nonexistent_plugin_tool", ToolKind::Other ; "unknown_tool_is_other")]
    fn tool_kind_from_registry_or_fallback(name: &str, expected: ToolKind) {
        assert_eq!(tool_kind(name), expected);
    }

    #[test]
    fn flow_progress_renders_structural_events_as_thoughts() {
        let entered = FlowProgress::TurnTypeEntered {
            thread_id: "root".into(),
            turn_type: craft_agent::TurnType::Scout,
        };
        let json = serde_json::to_value(flow_progress(&entered).unwrap()).unwrap();
        assert_eq!(json["sessionUpdate"], "agent_thought_chunk");
        let text = json["content"]["text"].as_str().unwrap();
        assert!(text.contains("entered scout"), "got {text}");

        let spawn = FlowProgress::ThreadSpawn {
            thread_id: "c1".into(),
            parent_id: "root".into(),
            turn_type: craft_agent::TurnType::Req,
        };
        let json = serde_json::to_value(flow_progress(&spawn).unwrap()).unwrap();
        assert!(
            json["content"]["text"]
                .as_str()
                .unwrap()
                .contains("spawned req")
        );
    }

    #[test]
    fn flow_progress_drops_terminal_events_to_avoid_duplication() {
        assert!(
            flow_progress(&FlowProgress::GoalReady {
                goal_doc: "g".into()
            })
            .is_none()
        );
        assert!(
            flow_progress(&FlowProgress::Done {
                verdict: "v".into()
            })
            .is_none()
        );
        assert!(flow_progress(&FlowProgress::NeedsReview { report: "r".into() }).is_none());
        assert!(
            flow_progress(&FlowProgress::Failed {
                stage: craft_agent::TurnType::Plan,
                reason: "x".into(),
            })
            .is_none()
        );
        assert!(flow_progress(&FlowProgress::Cancelled).is_none());
    }

    #[test]
    fn flow_progress_renders_advisor_note_as_thought() {
        let note = FlowProgress::AdvisorNote {
            thread_id: "c2".into(),
            addressed_to: "root".into(),
            severity: craft_agent::AdvisorSeverity::Blocker,
            message: "stale child: upstream changed".into(),
        };
        let json = serde_json::to_value(flow_progress(&note).unwrap()).unwrap();
        assert_eq!(json["sessionUpdate"], "agent_thought_chunk");
        let text = json["content"]["text"].as_str().unwrap();
        assert!(text.contains("advisor"), "got {text}");
        assert!(text.contains("stale child"), "got {text}");
    }

    #[test]
    fn advisor_note_renders_as_thought() {
        let json = serde_json::to_value(advisor_note("concern", "missing error handling")).unwrap();
        assert_eq!(json["sessionUpdate"], "agent_thought_chunk");
        let text = json["content"]["text"].as_str().unwrap();
        assert!(text.contains("advisor"), "got {text}");
        assert!(text.contains("concern"), "got {text}");
        assert!(text.contains("missing error handling"), "got {text}");
    }

    #[test]
    fn info_renders_as_thought() {
        let json = serde_json::to_value(info("advisor reviewing recent activity…")).unwrap();
        assert_eq!(json["sessionUpdate"], "agent_thought_chunk");
        let text = json["content"]["text"].as_str().unwrap();
        assert!(text.contains("advisor reviewing"), "got {text}");
    }

    fn start_event(tool: &str, raw_input: Option<serde_json::Value>) -> ToolStartEvent {
        ToolStartEvent {
            id: "t-1".into(),
            tool: Arc::from(tool),
            summary: String::new(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input,
            output: None,
        }
    }

    fn start_locations(
        tool: &str,
        raw_input: Option<serde_json::Value>,
    ) -> Option<serde_json::Value> {
        let update = tool_start(
            &start_event(tool, raw_input),
            Path::new(CWD),
            Some(Path::new(HOME)),
        );
        serde_json::to_value(update)
            .unwrap()
            .get("locations")
            .cloned()
    }

    #[test_case("read", Some(json!({"path": "/a/b.rs", "offset": 42, "limit": 5})), Some(json!([{"path": "/a/b.rs", "line": 41}])) ; "read_absolute_with_offset")]
    #[test_case("read", Some(json!({"path": "/a/b.rs", "offset": 1, "limit": 5})), Some(json!([{"path": "/a/b.rs", "line": 0}])) ; "offset_one_reports_line_zero")]
    #[test_case("read", Some(json!({"path": "src/lib.rs", "offset": 1, "limit": 0})), Some(json!([{"path": "/home/user/project/src/lib.rs", "line": 0}])) ; "read_relative_resolved_against_cwd")]
    #[test_case("read", Some(json!({"path": "~/notes.md", "offset": 1, "limit": 0})), Some(json!([{"path": "/home/user/notes.md", "line": 0}])) ; "read_tilde_expanded")]
    #[test_case("read", Some(json!({"path": "~", "offset": 1, "limit": 0})), Some(json!([{"path": "/home/user", "line": 0}])) ; "read_bare_tilde_expands_to_home")]
    #[test_case("read", Some(json!({"path": "~other/x", "offset": 1, "limit": 0})), None ; "read_tilde_user_prefix_unresolvable")]
    #[test_case("read", Some(json!({"file_path": "/a/b.rs", "offset": 7, "limit": 5})), Some(json!([{"path": "/a/b.rs", "line": 6}])) ; "read_file_path_alias")]
    #[test_case("read", Some(json!({"path": "/a/b.rs", "offset": "42", "limit": 5})), Some(json!([{"path": "/a/b.rs", "line": 41}])) ; "read_string_offset_coerced")]
    #[test_case("write", Some(json!({"path": "/a/b.rs", "content": "x"})), Some(json!([{"path": "/a/b.rs"}])) ; "write_no_line")]
    #[test_case("edit", Some(json!({"path": "c.rs", "old_string": "a", "new_string": "b"})), Some(json!([{"path": "/home/user/project/c.rs"}])) ; "edit_relative_no_line")]
    #[test_case("multiedit", Some(json!({"path": "c.rs", "edits": []})), Some(json!([{"path": "/home/user/project/c.rs"}])) ; "multiedit_relative_no_line")]
    #[test_case("edit_lines", Some(json!({"path": "/a", "start": 3, "end": 9, "new_string": "n"})), Some(json!([{"path": "/a", "line": 2}])) ; "edit_lines_start_becomes_line")]
    #[test_case("insert_lines", Some(json!({"path": "/a", "line": 5, "new_string": "n"})), Some(json!([{"path": "/a", "line": 5}])) ; "insert_lines_reports_the_inserted_row")]
    #[test_case("insert_lines", Some(json!({"path": "/a", "line": 0, "new_string": "n"})), Some(json!([{"path": "/a", "line": 0}])) ; "insert_lines_at_top")]
    #[test_case("index", Some(json!({"path": "/a/b.rs"})), Some(json!([{"path": "/a/b.rs"}])) ; "index_file_path")]
    #[test_case("view_image", Some(json!({"path": "img.png"})), Some(json!([{"path": "/home/user/project/img.png"}])) ; "view_image_relative")]
    #[test_case("glob", Some(json!({"pattern": "*.rs", "path": "src"})), None ; "glob_directory_path_ignored")]
    #[test_case("grep", Some(json!({"pattern": "x", "path": "src"})), None ; "grep_directory_path_ignored")]
    #[test_case("bash", Some(json!({"command": "ls", "workdir": "/tmp"})), None ; "bash_workdir_ignored")]
    #[test_case("read", None, None ; "missing_input_no_locations")]
    #[test_case("read", Some(json!({"offset": 1, "limit": 0})), None ; "missing_path_no_locations")]
    #[test_case("read", Some(json!({"path": "", "offset": 1, "limit": 0})), None ; "empty_path_no_locations")]
    fn tool_start_locations(
        tool: &str,
        input: Option<serde_json::Value>,
        expected: Option<serde_json::Value>,
    ) {
        assert_eq!(start_locations(tool, input), expected);
    }

    #[test]
    fn tool_start_with_no_raw_input_has_no_locations_field() {
        let update = tool_start(
            &start_event("read", None),
            Path::new(CWD),
            Some(Path::new(HOME)),
        );
        let json = serde_json::to_value(update).unwrap();
        assert!(
            json.get("locations").is_none(),
            "empty locations must be omitted: {json}"
        );
    }

    fn done_event(
        tool: &str,
        output: ToolOutput,
        is_error: bool,
        written: Option<&str>,
    ) -> ToolDoneEvent {
        ToolDoneEvent {
            id: "t-1".into(),
            tool: Arc::from(tool),
            output,
            is_error,
            annotation: None,
            written_path: written.map(str::to_owned),
        }
    }

    fn done_locations_of(event: &ToolDoneEvent) -> Option<serde_json::Value> {
        let update = tool_done(event, Path::new(CWD), Some(Path::new(HOME)));
        serde_json::to_value(update)
            .unwrap()
            .get("locations")
            .cloned()
    }

    #[test]
    fn done_written_path_reports_location_without_line() {
        let event = done_event(
            "memory",
            ToolOutput::Plain("wrote 3 bytes".into()),
            false,
            Some("/home/user/project/a.rs"),
        );
        assert_eq!(
            done_locations_of(&event),
            Some(json!([{"path": "/home/user/project/a.rs"}]))
        );
    }

    /// A file tool's start event already reported the path, with the line the
    /// client is following. Re-reporting it here would replace that line.
    #[test_case("edit_lines" ; "edit_lines_keeps_start_line")]
    #[test_case("insert_lines" ; "insert_lines_keeps_start_line")]
    #[test_case("write" ; "write_keeps_start_location")]
    fn done_file_tool_omits_written_path_location(tool: &str) {
        let event = done_event(
            tool,
            ToolOutput::Plain("wrote 3 bytes".into()),
            false,
            Some("/home/user/project/a.rs"),
        );
        assert_eq!(done_locations_of(&event), None);
    }

    #[test]
    fn done_error_suppresses_written_path_location() {
        let event = done_event(
            "memory",
            ToolOutput::Plain("write error: permission denied".into()),
            true,
            Some("/home/user/project/a.rs"),
        );
        assert_eq!(done_locations_of(&event), None);
    }

    #[test]
    fn done_without_file_output_has_no_locations() {
        let event = done_event("memory", ToolOutput::Plain("done".into()), false, None);
        assert_eq!(done_locations_of(&event), None);
    }

    #[test]
    fn replay_tool_use_reports_locations() {
        let msg = assistant(vec![MsgBlock::tool_use(
            "tu-1",
            "read",
            json!({"path": "src/lib.rs", "offset": 10, "limit": 0}),
        )]);
        let json = updates_json(&[msg]);
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["sessionUpdate"], "tool_call");
        assert_eq!(json[0]["toolCallId"], "tu-1");
        assert_eq!(
            json[0]["locations"],
            json!([{"path": "/home/user/project/src/lib.rs", "line": 9}])
        );
    }

    #[test]
    fn replay_non_file_tool_has_no_locations() {
        let msg = assistant(vec![MsgBlock::tool_use(
            "tu-1",
            "bash",
            json!({"command": "ls"}),
        )]);
        let json = updates_json(&[msg]);
        assert!(json[0].get("locations").is_none(), "{:?}", json[0]);
    }

    fn turn_event(
        context_size: Option<u32>,
        context_window: u32,
        cost: Option<f64>,
    ) -> TurnCompleteEvent {
        TurnCompleteEvent {
            message: Message::default(),
            usage: TokenUsage {
                input: 1_000,
                output: 200,
                cache_creation: 0,
                cache_read: 50_000,
            },
            model: "test-model".into(),
            cost,
            context_size,
            context_window,
        }
    }

    #[test]
    fn usage_update_reports_gauge_and_cumulative_cost() {
        let event = turn_event(Some(60_000), 200_000, Some(0.05));
        let json = serde_json::to_value(usage_update(&event, Some(0.125))).unwrap();
        assert_eq!(json["sessionUpdate"], "usage_update");
        assert_eq!(json["used"], 60_000);
        assert_eq!(json["size"], 200_000);
        assert_eq!(json["cost"]["amount"], 0.125);
        assert_eq!(json["cost"]["currency"], CURRENCY);
    }

    #[test]
    fn usage_update_without_cost_omits_cost() {
        let event = turn_event(Some(60_000), 200_000, None);
        let json = serde_json::to_value(usage_update(&event, None)).unwrap();
        assert_eq!(json["used"], 60_000);
        assert_eq!(json["size"], 200_000);
        assert!(json.get("cost").is_none(), "{json}");
    }

    #[test]
    fn usage_update_falls_back_to_usage_when_context_size_missing() {
        let event = turn_event(None, 200_000, None);
        let json = serde_json::to_value(usage_update(&event, None)).unwrap();
        assert_eq!(json["used"], 51_200);
    }
}
