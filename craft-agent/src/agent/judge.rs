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

const CRITERIA_SYSTEM: &str = "\
You are a judge evaluating whether an autonomous coding agent met each of a list of acceptance criteria. \
You are given the criteria and the tail of the agent's conversation. \
For EACH criterion, decide ONLY whether it is satisfied.\n\n\
Respond with one line per criterion, in order, each starting with MET or UNMET:\n\
- MET: the criterion is fully satisfied.\n\
- UNMET: the criterion is not yet satisfied.\n\n\
After the per-criterion verdicts, give a single concise summary line. Nothing else.";

const MET: &str = "met";
const UNMET: &str = "unmet";

#[derive(Debug)]
pub enum JudgeOutcome {
    Done,
    NotDone(String),
    /// Structured verdict over a list of acceptance criteria.
    #[allow(dead_code)]
    Criteria {
        met: Vec<String>,
        unmet: Vec<String>,
    },
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

/// Evaluate the agent's transcript against a fixed list of acceptance criteria,
/// returning a structured per-criterion verdict. Used by Flow's Verifier stage.
/// `/goal` keeps its free-text `evaluate` path; this is the structured sibling.
#[allow(dead_code)]
pub async fn evaluate_criteria(
    criteria: &[String],
    history: &[Message],
    active_provider: &Arc<dyn Provider>,
    active_model: &Model,
    judge_model_spec: Option<&str>,
    timeouts: craft_providers::Timeouts,
    session_id: Option<&str>,
) -> Result<JudgeOutcome, CrateAgentError> {
    let transcript = build_transcript(history);
    let list = criteria
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}. {c}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let user_msg = format!(
        "## Acceptance criteria\n{list}\n\n## Recent agent activity\n{transcript}\n\n\
         For each criterion, respond MET or UNMET on its own line, in order."
    );
    let messages = vec![Message::user(user_msg)];

    let verdict_text = match judge_model_spec {
        Some(spec) => match resolve_judge(spec, timeouts).await {
            Ok((model, provider)) => {
                collect_text_with(
                    provider.as_ref(),
                    &model,
                    &messages,
                    CRITERIA_SYSTEM,
                    session_id,
                )
                .await?
            }
            Err(e) => {
                warn!(error = %e, spec, "judge model resolution failed, using active model");
                collect_text_with(
                    active_provider.as_ref(),
                    active_model,
                    &messages,
                    CRITERIA_SYSTEM,
                    session_id,
                )
                .await?
            }
        },
        None => {
            collect_text_with(
                active_provider.as_ref(),
                active_model,
                &messages,
                CRITERIA_SYSTEM,
                session_id,
            )
            .await?
        }
    };

    Ok(parse_criteria_verdict(criteria, &verdict_text))
}

async fn collect_text_with(
    provider: &dyn Provider,
    model: &Model,
    messages: &[Message],
    system: &str,
    session_id: Option<&str>,
) -> Result<String, CrateAgentError> {
    let (ptx, _prx) = flume::unbounded();
    let system = system.to_string();
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

/// Parse a per-criterion MET/UNMET verdict. Lines are matched in order to the
/// criteria; a missing or ambiguous line defaults to UNMET (conservative).
pub fn parse_criteria_verdict(criteria: &[String], text: &str) -> JudgeOutcome {
    let mut met = Vec::new();
    let mut unmet = Vec::new();
    let mut verdict_lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(criteria.len());
    for criterion in criteria {
        let line = verdict_lines.next().unwrap_or("").to_ascii_lowercase();
        let is_met = line.starts_with(MET) && !line.starts_with(UNMET);
        if is_met {
            met.push(criterion.clone());
        } else {
            unmet.push(criterion.clone());
        }
    }
    info!(met = met.len(), unmet = unmet.len(), "criteria verdict");
    JudgeOutcome::Criteria { met, unmet }
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
    fn parse_criteria_verdict_classifies_each_line() {
        let criteria = vec![
            "tests pass".to_string(),
            "docs updated".to_string(),
            "no regressions".to_string(),
        ];
        let text = "MET\nUNMET\nMET\nall good";
        let JudgeOutcome::Criteria { met, unmet } = parse_criteria_verdict(&criteria, text) else {
            panic!("expected Criteria");
        };
        assert_eq!(
            met,
            vec!["tests pass".to_string(), "no regressions".to_string()]
        );
        assert_eq!(unmet, vec!["docs updated".to_string()]);
    }

    #[test]
    fn parse_criteria_verdict_defaults_missing_to_unmet() {
        let criteria = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let text = "MET\nUNMET";
        let JudgeOutcome::Criteria { met, unmet } = parse_criteria_verdict(&criteria, text) else {
            panic!("expected Criteria");
        };
        assert_eq!(met, vec!["a".to_string()]);
        assert_eq!(unmet, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn parse_criteria_verdict_tolerates_prefix_punctuation() {
        let criteria = vec!["x".to_string(), "y".to_string()];
        let text = "MET: yes\n- UNMET: no";
        let JudgeOutcome::Criteria { met, unmet } = parse_criteria_verdict(&criteria, text) else {
            panic!("expected Criteria");
        };
        assert_eq!(met.len(), 1);
        assert_eq!(unmet.len(), 1);
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
