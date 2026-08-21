#![allow(dead_code)]

use craft_providers::{ContentBlock, Message, Role, StopReason, StreamResponse, TokenUsage};
use test_case::test_case;

use super::super::Agent;
use super::super::test_support::*;
use super::{
    ADVISOR_REVIEWING_INFO, AdvisorTurnAction, SHIFT_OUT_TO_GENERAL_PROMPT,
    advisor_continuation_info, advisor_followup_message, advisor_turn_action, parse_shift_output,
};
use crate::agent::flow_loop::FlowProgress;
use crate::agent::history::History;
use crate::agent::turn_type::TurnType;
use crate::{AdvisorSeverity, AgentEvent, DoneReason};

#[test]
fn parse_shift_output_round_trips() {
    let original = crate::types::ToolOutput::ShiftTurnType {
        target: TurnType::Plan,
        rationale: "goal approved".into(),
    };
    let text = original.as_display_text();
    let parsed = parse_shift_output(&text).expect("parse");
    match parsed {
        crate::types::ToolOutput::ShiftTurnType { target, rationale } => {
            assert_eq!(target, TurnType::Plan);
            assert_eq!(rationale, "goal approved");
        }
        other => panic!("expected ShiftTurnType, got {other:?}"),
    }
}

#[test]
fn parse_shift_output_returns_none_on_garbage() {
    assert!(parse_shift_output("not json").is_none());
    assert!(parse_shift_output("{}").is_none());
    assert!(parse_shift_output(r#"{"shift":{"target":"unknown","rationale":""}}"#).is_none());
}

#[tokio::test]
async fn flow_shift_to_scout_emits_turn_type_entered() {
    let (_tmp, store) = tmp_flow_store();
    let (ptx, prx) = flume::unbounded::<FlowProgress>();
    let mut history = History::new(Vec::new());
    let (run_params, event_rx) = make_run_params(&mut history);
    let mut params = flow_agent_params(store, ptx);
    params.provider = std::sync::Arc::new(MockProvider::new(vec![
        shift_tool_call("t1", "scout", "need a codebase map"),
        text_response(StopReason::EndTurn),
        text_response(StopReason::EndTurn),
    ]));
    let agent = Agent::new(params, run_params);
    let _ = agent.run(flow_input()).await;

    let events = drain_events(&event_rx);
    let progress: Vec<_> = prx.try_iter().collect();
    assert!(
        progress.iter().any(|p| matches!(
            p,
            FlowProgress::TurnTypeEntered {
                turn_type: TurnType::Scout,
                ..
            }
        )),
        "expected TurnTypeEntered(Scout) in progress: {progress:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, AgentEvent::Done { .. }))
    );
}

#[tokio::test]
async fn flow_no_shift_stays_general() {
    let (_tmp, store) = tmp_flow_store();
    let (ptx, prx) = flume::unbounded::<FlowProgress>();
    let mut history = History::new(Vec::new());
    let (run_params, _event_rx) = make_run_params(&mut history);
    let mut params = flow_agent_params(store, ptx);
    params.provider = std::sync::Arc::new(MockProvider::new(vec![
        text_response(StopReason::EndTurn),
        text_response(StopReason::EndTurn),
    ]));
    let agent = Agent::new(params, run_params);
    let _ = agent.run(flow_input()).await;

    let progress: Vec<_> = prx.try_iter().collect();
    assert!(
        !progress
            .iter()
            .any(|p| matches!(p, FlowProgress::TurnTypeEntered { .. })),
        "no shift should produce no TurnTypeEntered: {progress:?}"
    );
}

#[tokio::test]
async fn flow_narrow_endturn_shifts_out_to_general() {
    let (_tmp, store) = tmp_flow_store();
    let (ptx, prx) = flume::unbounded::<FlowProgress>();
    let mut history = History::new(Vec::new());
    let (run_params, _event_rx) = make_run_params(&mut history);
    let mut params = flow_agent_params(store, ptx);
    params.provider = std::sync::Arc::new(MockProvider::new(vec![
        shift_tool_call("t1", "scout", "map it"),
        text_response(StopReason::EndTurn),
        text_response(StopReason::EndTurn),
    ]));
    let agent = Agent::new(params, run_params);
    let _ = agent.run(flow_input()).await;

    let progress: Vec<_> = prx.try_iter().collect();
    let entered: Vec<_> = progress
        .iter()
        .filter_map(|p| match p {
            FlowProgress::TurnTypeEntered { turn_type, .. } => Some(*turn_type),
            _ => None,
        })
        .collect();
    assert!(
        entered == vec![TurnType::Scout, TurnType::General],
        "ShiftOut should re-enter general after the Scout EndTurn: {entered:?}"
    );
}

#[tokio::test]
async fn flow_shift_out_leaves_trailing_user_message() {
    let (_tmp, store) = tmp_flow_store();
    let (ptx, _prx) = flume::unbounded::<FlowProgress>();
    let mut history = History::new(Vec::new());
    let (run_params, _event_rx) = make_run_params(&mut history);
    let mut params = flow_agent_params(store, ptx);
    params.provider = std::sync::Arc::new(MockProvider::new(vec![
        shift_tool_call("t1", "scout", "map it"),
        text_response(StopReason::EndTurn),
        text_response(StopReason::EndTurn),
    ]));
    let agent = Agent::new(params, run_params);
    let _ = agent.run(flow_input()).await;

    let msgs = history.as_slice();
    let hand_back = msgs
        .iter()
        .enumerate()
        .find(|(_, m)| {
            m.first_text_content()
                .is_some_and(|t| t == SHIFT_OUT_TO_GENERAL_PROMPT)
        })
        .map(|(i, _)| i);
    let Some(idx) = hand_back else {
        panic!("expected the ShiftOut hand-back message: {msgs:?}");
    };
    assert!(
        idx > 0 && matches!(msgs[idx - 1].role, Role::Assistant),
        "the message before the hand-back must be the scout assistant reply",
    );
    assert!(
        matches!(msgs[idx].role, Role::User),
        "the hand-back message must be user-role"
    );
}

#[tokio::test]
async fn flow_illegal_shift_pushes_message_and_stays() {
    let (_tmp, store) = tmp_flow_store();
    let (ptx, _prx) = flume::unbounded::<FlowProgress>();
    let mut history = History::new(Vec::new());
    let (run_params, _event_rx) = make_run_params(&mut history);
    let mut params = flow_agent_params(store, ptx);
    params.provider = std::sync::Arc::new(MockProvider::new(vec![
        shift_tool_call("t1", "scout", "map it"),
        shift_tool_call("t2", "execute", "skip ahead"),
        text_response(StopReason::EndTurn),
        text_response(StopReason::EndTurn),
    ]));
    let agent = Agent::new(params, run_params);
    let _ = agent.run(flow_input()).await;

    assert!(
        history.as_slice().iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::Text { text } if text.starts_with("Illegal shift")),
            )
        }),
        "expected an Illegal shift message in history"
    );
}

#[tokio::test]
async fn flow_tpm_to_plan_emits_goal_ready_and_awaits_approval() {
    let (_tmp, store) = tmp_flow_store();
    let (ptx, prx) = flume::unbounded::<FlowProgress>();
    let mut history = History::new(Vec::new());
    let (run_params, event_rx) = make_run_params(&mut history);
    let mut params = flow_agent_params(store, ptx);
    params.provider = std::sync::Arc::new(MockProvider::new(vec![
        shift_tool_call("t1", "tpm", "shape the goal"),
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "# Goal\n\nShip login with SSO.".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "t2".into(),
                        name: "shift".into(),
                        input: serde_json::json!({
                            "target": "plan",
                            "rationale": "goal ready",
                        }),
                        thought_signature: None,
                    },
                ],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::ToolUse),
        },
        text_response(StopReason::EndTurn),
    ]));
    let agent = Agent::new(params, run_params);
    let _ = agent.run(flow_input()).await;

    let progress: Vec<_> = prx.try_iter().collect();
    assert!(
        progress
            .iter()
            .any(|p| matches!(p, FlowProgress::GoalReady { .. })),
        "expected GoalReady in progress: {progress:?}"
    );
    let events = drain_events(&event_rx);
    let awaiting = events.iter().any(|e| {
        matches!(
            e.event,
            AgentEvent::Done {
                reason: DoneReason::AwaitingGoalApproval,
                ..
            }
        )
    });
    assert!(awaiting, "expected Done(AwaitingGoalApproval)");
    assert!(
        !progress.iter().any(|p| matches!(
            p,
            FlowProgress::TurnTypeEntered {
                turn_type: TurnType::Plan,
                ..
            }
        )),
        "turn_type must stay Tpm at the gate, but Plan was entered"
    );
}

#[tokio::test]
async fn flow_resume_after_gate_enters_plan() {
    let (_tmp, store) = tmp_flow_store();
    let (ptx, prx) = flume::unbounded::<FlowProgress>();
    let mut history = History::new(Vec::new());
    let (run_params, _event_rx) = make_run_params(&mut history);
    let mut params = flow_agent_params(store.clone(), ptx);
    params.provider = std::sync::Arc::new(MockProvider::new(vec![
        shift_tool_call("t1", "tpm", "shape the goal"),
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "# Goal\n\nShip login with SSO.".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "t2".into(),
                        name: "shift".into(),
                        input: serde_json::json!({
                            "target": "plan",
                            "rationale": "goal ready",
                        }),
                        thought_signature: None,
                    },
                ],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::ToolUse),
        },
    ]));
    let agent = Agent::new(params, run_params);
    let _ = agent.run(flow_input()).await;
    let gate_progress: Vec<_> = prx.try_iter().collect();
    assert!(
        !gate_progress.iter().any(|p| matches!(
            p,
            FlowProgress::TurnTypeEntered {
                turn_type: TurnType::Plan,
                ..
            }
        )),
        "gate must not enter Plan: {gate_progress:?}"
    );

    let (ptx2, prx2) = flume::unbounded::<FlowProgress>();
    let (run_params2, _event_rx2) = make_run_params(&mut history);
    let mut params2 = flow_agent_params(store, ptx2);
    params2.provider = std::sync::Arc::new(MockProvider::new(vec![
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "# Plan\n\n1. Add SSO flow.".into(),
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::EndTurn),
        },
        text_response(StopReason::EndTurn),
    ]));
    let resume_input =
        flow_input().with_flow_resume(crate::FLOW_APPROVE_ANSWER.into(), TurnType::Plan);
    let agent2 = Agent::new(params2, run_params2);
    let _ = agent2.run(resume_input).await;

    let resume_progress: Vec<_> = prx2.try_iter().collect();
    assert!(
        resume_progress.iter().any(|p| matches!(
            p,
            FlowProgress::TurnTypeEntered {
                turn_type: TurnType::Plan,
                ..
            }
        )),
        "resume must emit TurnTypeEntered(Plan): {resume_progress:?}"
    );
}

#[tokio::test]
async fn flow_shift_into_narrow_type_pushes_stage_brief() {
    let (_tmp, store) = tmp_flow_store();
    let (ptx, _prx) = flume::unbounded::<FlowProgress>();
    let mut history = History::new(Vec::new());
    let (run_params, _event_rx) = make_run_params(&mut history);
    let mut params = flow_agent_params(store, ptx);
    params.provider = std::sync::Arc::new(MockProvider::new(vec![
        shift_tool_call("t1", "scout", "need a codebase map"),
        text_response(StopReason::EndTurn),
        text_response(StopReason::EndTurn),
    ]));
    let agent = Agent::new(params, run_params);
    let _ = agent.run(flow_input()).await;

    let brief = history.as_slice().iter().rev().find_map(|m| {
        m.content.iter().find_map(|b| match b {
            ContentBlock::Text { text }
                if text.starts_with("You are now in the `scout` turn type") =>
            {
                Some(text.clone())
            }
            _ => None,
        })
    });
    let brief = brief.expect("expected a scout stage brief in history");
    assert!(
        brief.contains("codebase_context"),
        "brief should name the write entry: {brief}"
    );
    assert!(
        brief.contains("`tpm`"),
        "brief should list the legal next shift to tpm: {brief}"
    );
}

#[tokio::test]
async fn flow_stage_brief_inlines_core_reads() {
    let (_tmp, store) = tmp_flow_store();
    let (ptx, _prx) = flume::unbounded::<FlowProgress>();
    let mut history = History::new(Vec::new());
    let (run_params, _event_rx) = make_run_params(&mut history);
    let mut params = flow_agent_params(store, ptx);
    let scout_findings = "the codebase has 3 crates keyed off craft-agent";
    params.provider = std::sync::Arc::new(MockProvider::new(vec![
        shift_tool_call("t1", "scout", "map it"),
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: scout_findings.into(),
                    },
                    ContentBlock::ToolUse {
                        id: "t2".into(),
                        name: "shift".into(),
                        input: serde_json::json!({
                            "target": "tpm",
                            "rationale": "shape the goal",
                        }),
                        thought_signature: None,
                    },
                ],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::ToolUse),
        },
        text_response(StopReason::EndTurn),
        text_response(StopReason::EndTurn),
    ]));
    let agent = Agent::new(params, run_params);
    let _ = agent.run(flow_input()).await;

    let tpm_brief = history.as_slice().iter().rev().find_map(|m| {
        m.content.iter().find_map(|b| match b {
            ContentBlock::Text { text }
                if text.starts_with("You are now in the `tpm` turn type") =>
            {
                Some(text.clone())
            }
            _ => None,
        })
    });
    let tpm_brief = tpm_brief.expect("expected a tpm stage brief");
    assert!(
        tpm_brief.contains(scout_findings),
        "tpm brief should inline the scout's committed codebase_context: {tpm_brief}"
    );
    assert!(
        tpm_brief.contains("Acceptance criteria"),
        "tpm brief should surface the goal-doc guidance: {tpm_brief}"
    );
    assert!(
        tpm_brief.contains("Context (from the typed log)"),
        "tpm brief should render a context section: {tpm_brief}"
    );
}

#[tokio::test]
async fn flow_search_returns_persisted_entries_after_resume() {
    let (_tmp, store) = tmp_flow_store();
    let (ptx, _prx) = flume::unbounded::<FlowProgress>();
    {
        let hist = std::sync::Arc::new(std::sync::Mutex::new(
            crate::agent::typed_log::ThreadHistory::open(
                std::sync::Arc::clone(&store),
                "test-project",
                "test-workstream",
            ),
        ));
        hist.lock().unwrap().append(
            crate::agent::typed_log::ThreadId::new("test-workstream"),
            crate::agent::typed_log::EntryType::Goal,
            "ship the login flow with these acceptance criteria",
        );
    }
    let mut history = History::new(Vec::new());
    let (run_params, _event_rx) = make_run_params(&mut history);
    let params = flow_agent_params(store, ptx);
    let agent = Agent::new(params, run_params);
    let ctx = agent.tool_context();
    let backend = ctx
        .flow_search
        .as_ref()
        .expect("flow_search auto-wired in Flow mode");
    let hits = backend
        .search("test-project", "test-workstream", "login goal", 5)
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.path.starts_with("goal:")),
        "expected a goal hit, got: {hits:?}"
    );
}

#[tokio::test]
async fn flow_tool_context_exposes_thread_handles() {
    let (_tmp, store) = tmp_flow_store();
    let (ptx, _prx) = flume::unbounded::<FlowProgress>();
    let mut history = History::new(Vec::new());
    let (run_params, _event_rx) = make_run_params(&mut history);
    let params = flow_agent_params(store, ptx);
    let agent = Agent::new(params, run_params);
    let ctx = agent.tool_context();
    assert!(ctx.flow_thread_manager.is_some());
    assert!(ctx.flow_thread_id.is_some());
    assert!(ctx.flow_thread_history.is_some());
    assert!(ctx.flow_progress_tx.is_some());
}

#[test_case(None,        craft_config::AdvisorAutoAct::Concern, 2, 0, false, false ; "no_note_stops")]
#[test_case(Some(AdvisorSeverity::Blocker), craft_config::AdvisorAutoAct::Off, 2, 0, false, false ; "off_threshold_stops")]
#[test_case(Some(AdvisorSeverity::Nit), craft_config::AdvisorAutoAct::Concern, 2, 0, false, false ; "below_threshold_stops")]
#[test_case(Some(AdvisorSeverity::Blocker), craft_config::AdvisorAutoAct::Concern, 2, 0, false, true  ; "blocker_above_concern_continues")]
#[test_case(Some(AdvisorSeverity::Concern), craft_config::AdvisorAutoAct::Concern, 2, 0, false, true  ; "concern_at_threshold_continues")]
#[test_case(Some(AdvisorSeverity::Blocker), craft_config::AdvisorAutoAct::Concern, 0, 0, false, false ; "zero_budget_stops")]
#[test_case(Some(AdvisorSeverity::Blocker), craft_config::AdvisorAutoAct::Concern, 2, 2, false, false ; "exhausted_budget_stops")]
#[test_case(Some(AdvisorSeverity::Blocker), craft_config::AdvisorAutoAct::Concern, 2, 1, false, true  ; "budget_remaining_continues")]
#[test_case(Some(AdvisorSeverity::Blocker), craft_config::AdvisorAutoAct::Concern, 2, 0, true,  false ; "pending_approval_stops")]
fn advisor_turn_action_decision(
    note: Option<AdvisorSeverity>,
    auto_act: craft_config::AdvisorAutoAct,
    max_act_turns: u32,
    continuations: u32,
    pending_approval: bool,
    expect_continue: bool,
) {
    let note = note.map(|s| advisor_note(s, "real bug"));
    let cfg = advisor_cfg(auto_act, max_act_turns);
    let action = advisor_turn_action(note, &cfg, pending_approval, continuations);
    assert_eq!(
        matches!(action, AdvisorTurnAction::Continue(_)),
        expect_continue,
        "continuation mismatch"
    );
}

#[test]
fn advisor_turn_action_continues_with_note() {
    let note = advisor_note(AdvisorSeverity::Blocker, "leaks secret");
    let cfg = advisor_cfg(craft_config::AdvisorAutoAct::Concern, 2);
    let action = advisor_turn_action(Some(note), &cfg, false, 0);
    let AdvisorTurnAction::Continue(returned) = action else {
        panic!("expected Continue");
    };
    assert_eq!(returned.severity, AdvisorSeverity::Blocker);
    assert_eq!(returned.message, "leaks secret");
}

#[test]
fn advisor_followup_message_carries_note_and_is_hidden() {
    let note = advisor_note(AdvisorSeverity::Concern, "missing error handling");
    let msg = advisor_followup_message(&note);
    assert!(matches!(msg.role, Role::User));
    let text = msg.first_text_content().unwrap();
    assert!(text.contains("<advisor-note>"));
    assert!(text.contains("concern"));
    assert!(text.contains("missing error handling"));
    assert_eq!(msg.display_text.as_deref(), Some(""));
}

#[test]
fn advisor_continuation_info_describes_note_and_budget() {
    let note = advisor_note(AdvisorSeverity::Concern, "missing error handling");
    assert_eq!(
        advisor_continuation_info(&note, 1, 2),
        "advisor raised a concern (missing error handling); continuing to address it (1/2)"
    );
}

#[tokio::test]
async fn advisor_run_emits_reviewing_info_note_and_continuation_info() {
    const ADVISOR_NOTE_LINE: &str = "CONCERN: missing error handling";
    const OK_LINE: &str = "OK";
    // A slash-less spec cannot parse, so `resolve_advisor` falls back to the
    // active (mock) provider and this test never touches the network.
    const BAD_MODEL_SPEC: &str = "not-a-valid-spec";

    let mut history = History::new(Vec::new());
    let (run_params, event_rx) = make_run_params(&mut history);
    let mut params = make_agent_params();
    params.config.advisor = craft_config::AdvisorConfig {
        model: Some(BAD_MODEL_SPEC.into()),
        ..advisor_cfg(craft_config::AdvisorAutoAct::Concern, 2)
    };
    params.provider = std::sync::Arc::new(MockProvider::new(vec![
        text_response(StopReason::EndTurn),
        text_reply(ADVISOR_NOTE_LINE, StopReason::EndTurn),
        text_response(StopReason::EndTurn),
        text_reply(OK_LINE, StopReason::EndTurn),
    ]));
    Agent::new(params, run_params)
        .run(default_input())
        .await
        .unwrap();

    let events = drain_events(&event_rx);
    let reviewing = events
        .iter()
        .filter(|e| matches!(&e.event, AgentEvent::Info { message } if message == ADVISOR_REVIEWING_INFO))
        .count();
    assert_eq!(reviewing, 2, "one review per done-turn");
    assert!(has_event(&events, |e| matches!(
        e,
        AgentEvent::AdvisorNote { severity, message }
            if severity == "concern" && message == "missing error handling"
    )));
    let expected = advisor_continuation_info(
        &advisor_note(AdvisorSeverity::Concern, "missing error handling"),
        1,
        2,
    );
    assert!(has_event(&events, |e| matches!(
        e,
        AgentEvent::Info { message } if *message == expected
    )));
}

#[tokio::test]
async fn disabled_advisor_emits_no_lifecycle_info() {
    let mut history = History::new(Vec::new());
    let (run_params, event_rx) = make_run_params(&mut history);
    let mut params = make_agent_params();
    params.provider =
        std::sync::Arc::new(MockProvider::new(vec![text_response(StopReason::EndTurn)]));
    Agent::new(params, run_params)
        .run(default_input())
        .await
        .unwrap();

    let events = drain_events(&event_rx);
    assert!(!has_event(&events, |e| matches!(
        e,
        AgentEvent::Info { message } if message == ADVISOR_REVIEWING_INFO
    )));
}
