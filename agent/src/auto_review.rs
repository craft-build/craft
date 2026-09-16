//! LLM auto-review of permissions (E.7).
//!
//! When a tool call is not pre-approved and auto-review is on, a single
//! locked-down model call decides allow/deny with a risk level and a
//! rationale. The reviewer carries no tools and asks for plain JSON, so it
//! cannot itself mutate state. It fails closed on timeout, provider error,
//! empty, or unparseable output.
//!
//! Ported from the reference `craft-agent/src/agent/auto_review.rs`,
//! re-expressed for this repo's Rig edge: the reviewer call goes through
//! [`crate::edge::to_request`] + `CompletionModel::completion`, like the
//! compaction summarizer. Deviation: no reviewer-side cancel token (the
//! reference races one); the deadline alone bounds the call.

use std::time::Duration;

use rig_core::completion::{CompletionModel, message::AssistantContent};
use serde::Deserialize;
use snafu::Snafu;
use tokio::time::timeout;

use crate::edge;
use crate::history::{Message, UserContent};

const REVIEWER_DEADLINE: Duration = Duration::from_secs(30);

const REVIEWER_SYSTEM_PROMPT: &str = concat!(
    "You are a permission reviewer for an AI coding agent. ",
    "A tool is about to run that is not pre-approved. Decide whether it is safe ",
    "to let it run without asking the user.\n\n",
    "Evaluate the proposed action against these risks:\n",
    "- destructive or irreversible filesystem changes (deletes, overwrites outside the project)\n",
    "- running shell commands with broad or dangerous effects\n",
    "- network egress to unexpected hosts\n",
    "- anything that could exfiltrate data, persist malware, or brick the environment\n",
    "Inside-project edits, reads, and ordinary dev commands are usually fine.\n\n",
    "Reply with ONLY a JSON object, no prose, of this exact shape:\n",
    "{\"verdict\": \"allow\" | \"deny\", \"risk\": \"low\" | \"medium\" | \"high\" | \"critical\", ",
    "\"rationale\": \"one short sentence\"}\n",
    "When in doubt, deny."
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny,
}

impl Verdict {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow" | "approved" | "approve" | "yes" | "true" => Some(Self::Allow),
            "deny" | "denied" | "reject" | "no" | "false" => Some(Self::Deny),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Low,
    Medium,
    High,
    Critical,
}

impl Risk {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "critical" => Self::Critical,
            "high" => Self::High,
            "medium" => Self::Medium,
            _ => Self::Low,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub verdict: Verdict,
    pub risk: Risk,
    pub rationale: String,
}

#[derive(Debug, Clone, Snafu)]
pub enum ReviewError {
    #[snafu(display("auto-review timed out after {deadline:?}"))]
    Timeout { deadline: Duration },
    #[snafu(display("auto-review model call failed: {message}"))]
    Provider { message: String },
    #[snafu(display("auto-review produced no text"))]
    Empty,
    #[snafu(display("auto-review could not parse a decision: {message}"))]
    Parse { message: String },
}

#[derive(Debug, Deserialize)]
struct RawDecision {
    verdict: String,
    #[serde(default)]
    risk: Option<String>,
    #[serde(default)]
    rationale: Option<String>,
}

/// Parse the reviewer model's text into a [`Decision`]. Tolerates JSON
/// embedded in prose by extracting the outermost `{...}` block. Returns
/// `Parse` on any failure so callers fail closed.
pub fn parse_decision(text: &str) -> Result<Decision, ReviewError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(ReviewError::Empty);
    }
    let value = crate::json_repair::extract_json(trimmed)
        .map_err(|message| ReviewError::Parse { message })?;
    let raw: RawDecision = serde_json::from_value(value).map_err(|e| ReviewError::Parse {
        message: format!("invalid JSON: {e}"),
    })?;
    let verdict = Verdict::parse(&raw.verdict).ok_or_else(|| ReviewError::Parse {
        message: format!("unknown verdict: {:?}", raw.verdict),
    })?;
    let risk = raw.risk.as_deref().map(Risk::parse).unwrap_or(Risk::Low);
    let rationale = match raw.rationale {
        Some(r) if !r.trim().is_empty() => r,
        _ => "auto-reviewer gave no rationale".to_string(),
    };
    Ok(Decision {
        verdict,
        risk,
        rationale,
    })
}

fn review_message(tool: &str, scopes: &[String]) -> Message {
    let scope_list = if scopes.is_empty() {
        "(none)".to_string()
    } else {
        scopes.join("; ")
    };
    Message::User {
        content: vec![UserContent::text(format!(
            "Decide whether to allow this tool call without asking the user.\n\nTool: {tool}\nRequested scopes: {scope_list}\n\nReply with only the JSON object."
        ))],
    }
}

/// Run a single locked-down model call to review a proposed tool action.
///
/// `tool` is the human-readable tool key and `scopes` the permission scopes
/// being requested. The call carries no tools, so the reviewer cannot itself
/// mutate state. Fails closed on timeout, provider error, or unparseable
/// output.
pub async fn review<M: CompletionModel>(
    model: &M,
    tool: &str,
    scopes: &[String],
) -> Result<Decision, ReviewError> {
    review_with_deadline(model, tool, scopes, REVIEWER_DEADLINE).await
}

/// Deadline-parameterized core so tests can exercise the timeout path
/// without waiting the full 30 seconds.
pub(crate) async fn review_with_deadline<M: CompletionModel>(
    model: &M,
    tool: &str,
    scopes: &[String],
    deadline: Duration,
) -> Result<Decision, ReviewError> {
    let messages = vec![review_message(tool, scopes)];
    let request = edge::to_request(&messages, &[], Some(REVIEWER_SYSTEM_PROMPT), None, None);
    let response = match timeout(deadline, model.completion(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            return Err(ReviewError::Provider {
                message: e.to_string(),
            });
        }
        Err(_) => return Err(ReviewError::Timeout { deadline }),
    };
    let text = response
        .choice
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return Err(ReviewError::Empty);
    }
    parse_decision(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::test_utils::{MockCompletionModel, MockTurn};

    fn decision_json(verdict: &str, risk: &str, rationale: &str) -> String {
        format!(r#"{{"verdict":"{verdict}","risk":"{risk}","rationale":"{rationale}"}}"#)
    }

    #[tokio::test]
    async fn well_formed_allow_is_parsed_from_the_model() {
        let model =
            MockCompletionModel::new([MockTurn::text(decision_json("allow", "low", "read-only"))]);
        let d = review(&model, "write", &["/tmp/x".into()]).await.unwrap();
        assert_eq!(d.verdict, Verdict::Allow);
        assert_eq!(d.risk, Risk::Low);
        assert_eq!(d.rationale, "read-only");
        // Exactly one locked-down call: no tools, reviewer preamble.
        assert_eq!(model.requests().len(), 1);
        let request = &model.requests()[0];
        assert!(request.tools.is_empty());
        assert_eq!(request.preamble.as_deref(), Some(REVIEWER_SYSTEM_PROMPT));
    }

    #[tokio::test]
    async fn provider_error_fails_closed() {
        let model = MockCompletionModel::new([MockTurn::error("rate limited")]);
        assert!(matches!(
            review(&model, "bash", &["rm -rf /".into()]).await,
            Err(ReviewError::Provider { .. })
        ));
    }

    #[tokio::test]
    async fn timeout_fails_closed() {
        #[derive(Clone)]
        struct HangingModel;
        impl CompletionModel for HangingModel {
            async fn completion(
                &self,
                _request: rig_core::completion::CompletionRequest,
            ) -> Result<
                rig_core::completion::CompletionResponse,
                rig_core::completion::CompletionError,
            > {
                futures::future::pending().await
            }
            async fn stream(
                &self,
                _request: rig_core::completion::CompletionRequest,
            ) -> Result<
                rig_core::streaming::StreamingCompletionResponse,
                rig_core::completion::CompletionError,
            > {
                futures::future::pending().await
            }
        }
        let err = review_with_deadline(&HangingModel, "bash", &[], Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(matches!(err, ReviewError::Timeout { .. }));
    }

    #[test]
    fn parses_well_formed_allow() {
        let d =
            parse_decision(r#"{"verdict":"allow","risk":"low","rationale":"read-only"}"#).unwrap();
        assert_eq!(d.verdict, Verdict::Allow);
        assert_eq!(d.risk, Risk::Low);
        assert_eq!(d.rationale, "read-only");
    }

    #[test]
    fn parses_deny_embedded_in_prose() {
        let text = "Here is my decision:\n{\"verdict\":\"deny\",\"risk\":\"high\",\"rationale\":\"rm -rf\"}\nThanks";
        let d = parse_decision(text).unwrap();
        assert_eq!(d.verdict, Verdict::Deny);
        assert_eq!(d.risk, Risk::High);
        assert_eq!(d.rationale, "rm -rf");
    }

    #[test]
    fn parses_without_optional_fields() {
        let d = parse_decision(r#"{"verdict":"allow"}"#).unwrap();
        assert_eq!(d.verdict, Verdict::Allow);
        assert_eq!(d.risk, Risk::Low);
        assert!(d.rationale.contains("no rationale"));
    }

    #[test]
    fn malformed_json_is_repaired() {
        let d = parse_decision("{\"verdict\":\"allow\",\"risk\":\"low\",}").unwrap();
        assert_eq!(d.verdict, Verdict::Allow);
        assert_eq!(d.risk, Risk::Low);
    }

    #[test]
    fn empty_response_is_error() {
        assert!(matches!(parse_decision("   "), Err(ReviewError::Empty)));
    }

    #[test]
    fn non_json_is_parse_error() {
        assert!(matches!(
            parse_decision("I think it's fine"),
            Err(ReviewError::Parse { .. })
        ));
    }

    #[test]
    fn unknown_verdict_is_parse_error() {
        assert!(matches!(
            parse_decision(r#"{"verdict":"maybe"}"#),
            Err(ReviewError::Parse { .. })
        ));
    }

    #[test]
    fn verdict_aliases_normalize() {
        assert_eq!(Verdict::parse("Approved"), Some(Verdict::Allow));
        assert_eq!(Verdict::parse("REJECT"), Some(Verdict::Deny));
        assert_eq!(Verdict::parse("banana"), None);
    }

    #[test]
    fn risk_unknown_defaults_low() {
        let d = parse_decision(r#"{"verdict":"deny","risk":"enormous"}"#).unwrap();
        assert_eq!(d.risk, Risk::Low);
    }
}
