use std::sync::Arc;

use serde_json::Value;
use tracing::{info, warn};

use craft_providers::provider::Provider;
use craft_providers::{AgentError, Message, Model, RequestOptions};

use crate::AgentError as CrateAgentError;

const JUDGE_SYSTEM: &str = "\
You are a judge evaluating whether an autonomous coding agent has met the user's goal. \
You are given the goal and the tail of the agent's conversation. \
Decide ONLY whether the goal has been fully achieved. Do not judge style, only completion.\n\n\
Respond with exactly one line that starts with DONE or NOT_DONE:\n\
- DONE: the goal is fully met (work completed and, where applicable, verified).\n\
- NOT_DONE: the goal is not yet met.\n\n\
On the lines after the verdict, give a single concise sentence explaining what is missing or \
confirming completion. Nothing else.";

const MAX_JUDGE_MESSAGES: usize = 12;
const MAX_TRANSCRIPT_CHARS: usize = 12_000;

#[derive(Debug)]
pub enum JudgeOutcome {
    Done,
    NotDone(String),
}

pub async fn evaluate(
    goal: &str,
    history: &[Message],
    active_provider: &Arc<dyn Provider>,
    active_model: &Model,
    judge_model_spec: Option<&str>,
    timeouts: craft_providers::Timeouts,
    session_id: Option<&str>,
) -> Result<JudgeOutcome, CrateAgentError> {
    let transcript = build_transcript(history);
    let user_msg = format!(
        "## Goal\n{goal}\n\n## Recent agent activity\n{transcript}\n\n\
         Has the agent fully met the goal? Respond with DONE or NOT_DONE and a one-line reason."
    );
    let messages = vec![Message::user(user_msg)];

    let verdict_text = match judge_model_spec {
        Some(spec) => match resolve_judge(spec, timeouts).await {
            Ok((model, provider)) => {
                collect_text(provider.as_ref(), &model, &messages, session_id).await?
            }
            Err(e) => {
                warn!(error = %e, spec, "judge model resolution failed, using active model");
                collect_text(
                    active_provider.as_ref(),
                    active_model,
                    &messages,
                    session_id,
                )
                .await?
            }
        },
        None => {
            collect_text(
                active_provider.as_ref(),
                active_model,
                &messages,
                session_id,
            )
            .await?
        }
    };

    Ok(parse_verdict(&verdict_text))
}

async fn resolve_judge(
    spec: &str,
    timeouts: craft_providers::Timeouts,
) -> Result<(Model, Box<dyn Provider>), CrateAgentError> {
    let mut model = Model::from_spec(spec).map_err(|e| AgentError::Config {
        message: format!("invalid judge_model spec: {e}"),
    })?;
    let provider = craft_providers::provider::from_model(&mut model, timeouts).await?;
    Ok((model, provider))
}

async fn collect_text(
    provider: &dyn Provider,
    model: &Model,
    messages: &[Message],
    session_id: Option<&str>,
) -> Result<String, CrateAgentError> {
    let (ptx, _prx) = flume::unbounded();
    let system = JUDGE_SYSTEM.to_string();
    let tools = Value::Array(vec![]);
    let response = provider
        .stream_message(
            model,
            messages,
            &system,
            &tools,
            &ptx,
            RequestOptions::default(),
            session_id,
        )
        .await?;
    Ok(response.message.user_text().unwrap_or_default().to_string())
}

fn build_transcript(history: &[Message]) -> String {
    let tail: Vec<&Message> = history.iter().rev().take(MAX_JUDGE_MESSAGES).collect();
    let mut out = String::new();
    for msg in tail.into_iter().rev() {
        if !out.is_empty() {
            out.push_str("\n---\n");
        }
        let role = match msg.role {
            craft_providers::Role::User => "user",
            craft_providers::Role::Assistant => "assistant",
        };
        out.push_str(&format!("[{role}] "));
        if let Some(t) = msg.user_text() {
            out.push_str(t);
        }
        if out.len() > MAX_TRANSCRIPT_CHARS {
            let cut = out.floor_char_boundary(MAX_TRANSCRIPT_CHARS);
            out.truncate(cut);
            out.push_str("\n...(truncated)");
            break;
        }
    }
    out
}

fn parse_verdict(text: &str) -> JudgeOutcome {
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let not_done = first.contains("not done")
        || first.contains("not_done")
        || first.contains("not-done")
        || first == "no";
    info!(verdict = %text.trim(), "judge verdict");
    if not_done {
        let reason = text
            .lines()
            .skip_while(|l| l.trim().is_empty())
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        JudgeOutcome::NotDone(if reason.is_empty() {
            "goal not yet met".to_string()
        } else {
            reason
        })
    } else {
        JudgeOutcome::Done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("DONE\nAll tests pass", false; "done plain")]
    #[test_case("NOT_DONE\nstill failing", true; "not_done underscore")]
    #[test_case("NOT DONE\nstill failing", true; "not done with space")]
    #[test_case("NOT-DONE\nstill failing", true; "not done with hyphen")]
    #[test_case("No", true; "bare no")]
    #[test_case("done: nothing remains", false; "done with colon")]
    #[test_case("**DONE**\ngoal met", false; "done with markdown")]
    #[test_case("", false; "empty verdict")]
    fn parse_verdict_classifies(text: &str, expects_not_done: bool) {
        match (parse_verdict(text), expects_not_done) {
            (JudgeOutcome::Done, false) | (JudgeOutcome::NotDone(_), true) => {}
            (outcome, exp) => panic!("parsed {outcome:?} but expects_not_done={exp} for {text:?}"),
        }
    }

    #[test]
    fn parse_verdict_extracts_reason() {
        let JudgeOutcome::NotDone(reason) = parse_verdict("NOT_DONE\nTests still fail\n") else {
            panic!("expected NotDone");
        };
        assert_eq!(reason, "Tests still fail");
    }

    #[test]
    fn build_transcript_truncates_at_char_boundary() {
        let mut history = Vec::new();
        let big: String = "é".repeat(MAX_TRANSCRIPT_CHARS + 100);
        history.push(Message::user(big));
        let transcript = build_transcript(&history);
        assert!(transcript.len() <= MAX_TRANSCRIPT_CHARS + 64);
        assert!(transcript.ends_with("...(truncated)"));
    }

    #[test]
    fn build_transcript_respects_message_cap() {
        let history: Vec<Message> = (0..MAX_JUDGE_MESSAGES + 5)
            .map(|i| Message::user(format!("msg{i}")))
            .collect();
        let transcript = build_transcript(&history);
        assert_eq!(transcript.matches("---").count(), MAX_JUDGE_MESSAGES - 1);
    }
}
