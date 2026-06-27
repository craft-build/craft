use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::discovery::{DiscoveredFile, Discovery};
use crate::types::{Finding, Priority};

mod orchestrator;
pub use orchestrator::{ReviewOrchestrator, ReviewProgress};

const CHECK_EXTENSIONS: &[&str] = &["md"];

/// Review severity for check authoring (`low`/`medium`/`high`/`critical`).
/// Maps onto the existing `Priority` (P0-P3) so findings flow through the same
/// renderers and stores as the `review` tool's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }

    pub const fn to_priority(self) -> Priority {
        match self {
            Self::Critical => Priority::P0,
            Self::High => Priority::P1,
            Self::Medium => Priority::P2,
            Self::Low => Priority::P3,
        }
    }
}

#[derive(Debug, Error)]
pub enum CheckError {
    #[error("missing frontmatter in {0}")]
    MissingFrontmatter(PathBuf),
    #[error("invalid frontmatter in {path}: {source}")]
    InvalidFrontmatter {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
}

/// A subprocess failure surfaced from a check or the main pass.
#[derive(Debug, Clone)]
pub struct ReviewError {
    pub check: String,
    pub message: String,
}

/// The result of running a review: collected findings plus any subprocess
/// failures (which would otherwise be indistinguishable from a clean run).
#[derive(Debug, Default)]
pub struct ReviewOutcome {
    pub findings: Vec<Finding>,
    pub errors: Vec<ReviewError>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct CheckFrontmatter {
    name: Option<String>,
    model: Option<String>,
    #[serde(rename = "turn-limit")]
    turn_limit: Option<u32>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(rename = "severity-default")]
    severity_default: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub model: Option<String>,
    pub turn_limit: Option<u32>,
    pub tools: Vec<String>,
    pub severity_default: Severity,
    pub body: String,
    pub path: PathBuf,
}

pub fn discover(discovery: &Discovery) -> Vec<DiscoveredFile> {
    discovery.discover_files("checks", CHECK_EXTENSIONS)
}

pub fn parse(file: &DiscoveredFile) -> Result<Check, CheckError> {
    let (frontmatter, body) = split_frontmatter(&file.content)
        .ok_or(CheckError::MissingFrontmatter(file.path.clone()))?;
    let fm: CheckFrontmatter =
        serde_yaml::from_str(&frontmatter).map_err(|source| CheckError::InvalidFrontmatter {
            path: file.path.clone(),
            source,
        })?;
    let severity_default = fm
        .severity_default
        .as_deref()
        .and_then(Severity::parse)
        .unwrap_or(Severity::Medium);
    Ok(Check {
        name: fm.name.unwrap_or_else(|| file.name.clone()),
        model: fm.model,
        turn_limit: fm.turn_limit,
        tools: fm.tools,
        severity_default,
        body,
        path: file.path.clone(),
    })
}

fn split_frontmatter(content: &str) -> Option<(String, String)> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.first()?.trim_end() != "---" {
        return None;
    }
    let close = lines[1..].iter().position(|l| l.trim_end() == "---")? + 1;
    let frontmatter = lines[1..close].join("\n");
    let body = lines
        .get(close + 1..)
        .map(|l| l.join("\n"))
        .unwrap_or_default();
    Some((frontmatter, body))
}

/// A raw finding as emitted by a check subprocess, before normalization to the
/// shared `Finding` type. Mirrors the JSON schema in the orchestrator's prompt.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RawFinding {
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub line_start: Option<u32>,
    #[serde(default)]
    pub line_end: Option<u32>,
    #[serde(default)]
    pub severity: Option<Severity>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub suggestion: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub rule_ids: Vec<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub check: Option<String>,
}

const DEFAULT_FINDING_CONFIDENCE: f64 = 0.7;
const UNTITLED_FINDING: &str = "Untitled finding";

impl RawFinding {
    /// Normalize a raw subprocess finding into the shared `Finding` type.
    pub fn to_finding(self, default_severity: Severity) -> Finding {
        let severity = self.severity.unwrap_or(default_severity);
        Finding {
            title: self.title.unwrap_or_else(|| UNTITLED_FINDING.to_string()),
            body: self.body.unwrap_or_default(),
            priority: severity.to_priority(),
            confidence: self.confidence.unwrap_or(DEFAULT_FINDING_CONFIDENCE),
            file_path: self.file_path.unwrap_or_default(),
            line_start: self.line_start.unwrap_or(0) as usize,
            line_end: self
                .line_end
                .unwrap_or_else(|| self.line_start.unwrap_or(0)) as usize,
            rule_ids: self.rule_ids,
            suggestion: self.suggestion,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_frontmatter_parses_yaml_and_body() {
        let content =
            "---\nname: audit\nmodel: anthropic/claude\nturn-limit: 5\n---\nReview the code.\n";
        let (fm, body) = split_frontmatter(content).unwrap();
        assert!(fm.contains("name: audit"));
        assert_eq!(body, "Review the code.");
    }

    #[test]
    fn split_frontmatter_returns_none_without_delimiters() {
        assert!(split_frontmatter("just text").is_none());
    }

    #[test]
    fn parse_builds_check_with_defaults() {
        let file = DiscoveredFile {
            name: "audit".into(),
            path: PathBuf::from(".agents/checks/audit.md"),
            scope: crate::discovery::Scope::Project(0),
            content: "---\nname: audit\nmodel: anthropic/claude\n---\nDo the thing.".into(),
        };
        let check = parse(&file).unwrap();
        assert_eq!(check.name, "audit");
        assert_eq!(check.model.as_deref(), Some("anthropic/claude"));
        assert_eq!(check.severity_default, Severity::Medium);
        assert_eq!(check.body, "Do the thing.");
    }

    #[test]
    fn parse_uses_file_name_when_frontmatter_omits_name() {
        let file = DiscoveredFile {
            name: "lint".into(),
            path: PathBuf::from(".agents/checks/lint.md"),
            scope: crate::discovery::Scope::Project(0),
            content: "---\nseverity-default: high\n---\nBody.".into(),
        };
        let check = parse(&file).unwrap();
        assert_eq!(check.name, "lint");
        assert_eq!(check.severity_default, Severity::High);
    }

    #[test]
    fn parse_rejects_missing_frontmatter() {
        let file = DiscoveredFile {
            name: "bad".into(),
            path: PathBuf::from(".agents/checks/bad.md"),
            scope: crate::discovery::Scope::Project(0),
            content: "no frontmatter here".into(),
        };
        assert!(parse(&file).is_err());
    }

    #[test]
    fn severity_ordering() {
        assert!(Severity::Critical > Severity::High);
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low);
    }

    #[test_case::test_case(Severity::Critical, Priority::P0; "critical_to_p0")]
    #[test_case::test_case(Severity::High, Priority::P1; "high_to_p1")]
    #[test_case::test_case(Severity::Medium, Priority::P2; "medium_to_p2")]
    #[test_case::test_case(Severity::Low, Priority::P3; "low_to_p3")]
    fn severity_to_priority(severity: Severity, expected: Priority) {
        assert_eq!(severity.to_priority(), expected);
    }

    #[test]
    fn raw_finding_normalizes_to_finding_with_defaults() {
        let raw = RawFinding {
            title: Some("Add error handling".into()),
            body: Some("Missing match arm".into()),
            severity: Some(Severity::High),
            file_path: Some("src/lib.rs".into()),
            line_start: Some(10),
            line_end: Some(12),
            rule_ids: vec!["rust-eh-1".into()],
            suggestion: Some("use thiserror".into()),
            confidence: Some(0.9),
            check: Some("audit".into()),
        };
        let finding = raw.to_finding(Severity::Medium);
        assert_eq!(finding.title, "Add error handling");
        assert_eq!(finding.priority, Priority::P1);
        assert_eq!(find_confidence(&finding), 0.9);
        assert_eq!(finding.file_path, "src/lib.rs");
        assert_eq!(finding.line_start, 10);
        assert_eq!(finding.line_end, 12);
        assert_eq!(finding.rule_ids, vec!["rust-eh-1".to_string()]);
        assert_eq!(finding.suggestion.as_deref(), Some("use thiserror"));
    }

    #[test]
    fn raw_finding_fills_defaults_when_fields_missing() {
        let raw = RawFinding {
            title: None,
            file_path: None,
            severity: None,
            line_start: Some(5),
            ..Default::default()
        };
        let finding = raw.to_finding(Severity::Low);
        assert_eq!(finding.title, "Untitled finding");
        assert_eq!(finding.priority, Priority::P3);
        assert_eq!(finding.line_start, 5);
        assert_eq!(finding.line_end, 5);
        assert_eq!(find_confidence(&finding), 0.7);
        assert!(finding.file_path.is_empty());
    }

    #[test]
    fn severity_parse_lowercase() {
        assert_eq!(Severity::parse("critical"), Some(Severity::Critical));
        assert_eq!(Severity::parse("HIGH"), Some(Severity::High));
        assert_eq!(Severity::parse("nonsense"), None);
    }

    #[allow(clippy::float_cmp)]
    fn find_confidence(f: &Finding) -> f64 {
        f.confidence
    }
}
