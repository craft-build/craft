use agent_client_protocol_schema::v1::{
    Content, ContentBlock, ContentChunk, Diff, ImageContent, SessionUpdate, StopReason,
    TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use craft_agent::DoneReason;
use craft_agent::FlowProgress;
use craft_agent::tools::ToolRegistry;
use craft_agent::types::{BatchProgressEvent, ToolDoneEvent, ToolOutput, ToolStartEvent};
use craft_providers::{ContentBlock as MsgBlock, ImageMediaType, Message, Role as MsgRole};
use std::collections::HashMap;

const MIN_FENCE_LEN: usize = 3;
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

pub fn user_message_chunk(text: &str) -> SessionUpdate {
    SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text.to_string(),
    ))))
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

pub fn tool_start(event: &ToolStartEvent) -> SessionUpdate {
    let mut fields = ToolCallUpdateFields::new()
        .status(ToolCallStatus::InProgress)
        .title(event.summary.clone());

    if let Some(raw) = &event.raw_input {
        fields = fields.raw_input(raw.clone());
    }

    let mut locations = Vec::new();
    if event.input.is_some()
        && let Some(path) = input_path(event.raw_input.as_ref())
    {
        locations.push(ToolCallLocation::new(path));
    }
    if !locations.is_empty() {
        fields = fields.locations(locations);
    }

    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(event.id.clone()),
        fields,
    ))
}

fn input_path(raw_input: Option<&serde_json::Value>) -> Option<std::path::PathBuf> {
    raw_input
        .and_then(|v| v.get("path"))
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
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

pub fn tool_done(event: &ToolDoneEvent) -> SessionUpdate {
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
            if text.is_empty() {
                vec![]
            } else {
                vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
                    TextContent::new(fenced(&text)),
                )))]
            }
        }
    };

    let raw_text = event.output.as_text();
    let mut fields = ToolCallUpdateFields::new().status(status).content(content);
    if !raw_text.is_empty() {
        fields = fields.raw_output(serde_json::Value::String(raw_text));
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

pub fn replay_history(messages: &[Message]) -> Vec<SessionUpdate> {
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
            MsgRole::Assistant => replay_assistant(msg, &mut updates),
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
                is_error,
                ..
            } => updates.push(replay_tool_result(
                tool_use_id,
                content,
                *is_error,
                tool_inputs.get(tool_use_id).copied(),
            )),
            MsgBlock::Image { source } => {
                updates.push(SessionUpdate::UserMessageChunk(ContentChunk::new(
                    ContentBlock::Image(ImageContent::new(
                        source.data.to_string(),
                        mime_type(&source.media_type),
                    )),
                )));
            }
            _ => {}
        }
    }
}

fn replay_assistant(msg: &Message, updates: &mut Vec<SessionUpdate>) {
    for block in &msg.content {
        match block {
            MsgBlock::Text { text } => updates.push(text_delta(text)),
            MsgBlock::Thinking { thinking, .. } => updates.push(thinking_delta(thinking)),
            MsgBlock::ToolUse {
                id, name, input, ..
            } => {
                updates.push(replay_tool_call(id, name, input));
            }
            _ => {}
        }
    }
}

fn replay_tool_call(id: &str, name: &str, input: &serde_json::Value) -> SessionUpdate {
    SessionUpdate::ToolCall(
        ToolCall::new(ToolCallId::from(id.to_string()), name.to_string())
            .kind(tool_kind(name))
            .status(ToolCallStatus::Pending)
            .raw_input(input.clone()),
    )
}

fn replay_tool_result(
    id: &str,
    content: &str,
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
    } else if !content.is_empty() {
        fields = fields.content(vec![ToolCallContent::Content(Content::new(
            ContentBlock::Text(TextContent::new(fenced(content))),
        ))]);
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
    use craft_providers::ImageSource;
    use test_case::test_case;

    use super::*;

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
        replay_history(messages)
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
}
