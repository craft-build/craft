use std::sync::Arc;
use std::time::Duration;

use craft_providers::Message;
use craft_providers::ProviderEvent;
use craft_providers::RequestOptions;
use craft_providers::ThinkingConfig;
use craft_providers::model::Model;
use craft_providers::provider::Provider;
use flume;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::time::timeout;

use crate::CancelToken;

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

#[derive(Debug, Error)]
pub enum ReviewError {
    #[error("auto-review timed out after {0:?}")]
    Timeout(Duration),
    #[error("auto-review cancelled")]
    Cancelled,
    #[error("auto-review model call failed: {0}")]
    Provider(String),
    #[error("auto-review produced no text")]
    Empty,
    #[error("auto-review could not parse a decision: {0}")]
    Parse(String),
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
    let json = extract_json(trimmed)
        .ok_or_else(|| ReviewError::Parse("response was not a JSON object".to_string()))?;
    let raw: RawDecision = parse_json(json)?;
    let verdict = Verdict::parse(&raw.verdict)
        .ok_or_else(|| ReviewError::Parse(format!("unknown verdict: {:?}", raw.verdict)))?;
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

fn extract_json(text: &str) -> Option<&str> {
    if text.trim_start().starts_with('{') {
        return Some(text);
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if start < end {
        Some(&text[start..=end])
    } else {
        None
    }
}

fn parse_json<T: DeserializeOwned>(s: &str) -> Result<T, ReviewError> {
    serde_json::from_str(s).map_err(|e| ReviewError::Parse(format!("invalid JSON: {e}")))
}

/// Run a single locked-down model call to review a proposed tool action.
///
/// `tool` is the human-readable tool key and `scopes` the permission scopes
/// being requested. The call carries no tools and asks for plain JSON, so the
/// reviewer cannot itself mutate state. Fails closed on timeout, cancel,
/// provider error, or unparseable output.
pub async fn review(
    provider: &Arc<dyn Provider>,
    model: &Model,
    tool: &str,
    scopes: &[String],
    cancel: &CancelToken,
) -> Result<Decision, ReviewError> {
    let prompt = build_prompt(tool, scopes);
    let messages = vec![Message::user(prompt)];
    let (event_tx, event_rx) = flume::unbounded::<ProviderEvent>();
    tokio::spawn(async move { while event_rx.recv_async().await.is_ok() {} });
    let opts = RequestOptions {
        thinking: ThinkingConfig::Off,
        fast: false,
    };
    let call = async {
        provider
            .stream_message(
                model,
                &messages,
                REVIEWER_SYSTEM_PROMPT,
                &EMPTY_TOOLS,
                &event_tx,
                opts,
                None,
            )
            .await
    };
    let response = match timeout(REVIEWER_DEADLINE, cancel.race(call)).await {
        Ok(Ok(Ok(stream_response))) => stream_response,
        Ok(Ok(Err(provider_err))) => return Err(ReviewError::Provider(provider_err.to_string())),
        Ok(Err(_cancel_msg)) => return Err(ReviewError::Cancelled),
        Err(_) => return Err(ReviewError::Timeout(REVIEWER_DEADLINE)),
    };
    let text = response
        .message
        .first_text_content()
        .ok_or(ReviewError::Empty)?;
    parse_decision(text)
}

fn build_prompt(tool: &str, scopes: &[String]) -> String {
    let scope_list = if scopes.is_empty() {
        "(none)".to_string()
    } else {
        scopes.join("; ")
    };
    format!(
        "Decide whether to allow this tool call without asking the user.\n\nTool: {tool}\nRequested scopes: {scope_list}\n\nReply with only the JSON object."
    )
}

const EMPTY_TOOLS: serde_json::Value = serde_json::Value::Array(Vec::new());

#[cfg(test)]
mod tests {
    use super::*;

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
    fn empty_response_is_error() {
        assert!(matches!(parse_decision("   "), Err(ReviewError::Empty)));
    }

    #[test]
    fn non_json_is_parse_error() {
        assert!(matches!(
            parse_decision("I think it's fine"),
            Err(ReviewError::Parse(_))
        ));
    }

    #[test]
    fn unknown_verdict_is_parse_error() {
        assert!(matches!(
            parse_decision(r#"{"verdict":"maybe"}"#),
            Err(ReviewError::Parse(_))
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
