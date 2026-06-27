use std::process::Stdio;
use std::sync::Arc;

use regex::Regex;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tracing::warn;

use super::{Check, RawFinding, ReviewError, ReviewOutcome, Severity, discover, parse};
use crate::discovery::Discovery;
use crate::tools::{
    STYLEGUIDE_GET_TOOL_NAME, STYLEGUIDE_LIST_TOOL_NAME, STYLEGUIDE_SEARCH_TOOL_NAME,
};
use crate::types::{Finding, Priority};

const MAX_CONCURRENT: usize = 4;

/// Tools always available to a review subprocess so checks can ground findings
/// in styleguide rules. Findings are emitted via the JSON output contract, not
/// the in-process `report_finding` tool (which cannot cross the subprocess
/// boundary), so it is deliberately not included here.
const DEFAULT_REVIEW_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    STYLEGUIDE_LIST_TOOL_NAME,
    STYLEGUIDE_SEARCH_TOOL_NAME,
    STYLEGUIDE_GET_TOOL_NAME,
];

const FINDINGS_INSTRUCTIONS: &str = "Review the current codebase and report issues. \
Use the styleguide tools (styleguide_list, styleguide_search, styleguide_get) to find \
the rules that apply, and link each finding to them via rule_ids. \
You MUST respond with ONLY a single JSON object of this exact shape (no prose, no markdown):\n\
{\"findings\": [{\"file_path\": \"path\", \"line_start\": 0, \"line_end\": 0, \
\"severity\": \"low|medium|high|critical\", \"title\": \"short summary\", \
\"suggestion\": \"how to fix\", \"body\": \"detailed explanation\", \
\"rule_ids\": [\"styleguide-rule-id\"], \"confidence\": 0.8}]}\n\
If there are no issues, respond with exactly {\"findings\": []}.";

/// Progress events emitted while a review runs, for live terminal display.
#[derive(Debug, Clone)]
pub enum ReviewProgress {
    ChecksStarted {
        total: usize,
    },
    CheckStarted {
        name: String,
    },
    CheckFinished {
        name: String,
        findings: usize,
        errored: bool,
    },
    FilePassStarted {
        total: usize,
    },
    FileReviewed {
        file: String,
        findings: usize,
        errored: bool,
    },
}

pub struct ReviewOrchestrator {
    pub model: Option<String>,
    pub check_filter: Option<Regex>,
    pub min_severity: Option<Severity>,
    pub max_concurrent: usize,
    pub no_file_pass: bool,
}

impl ReviewOrchestrator {
    pub fn new(
        model: Option<String>,
        check_filter: Option<Regex>,
        min_severity: Option<Severity>,
    ) -> Self {
        Self {
            model,
            check_filter,
            min_severity,
            max_concurrent: MAX_CONCURRENT,
            no_file_pass: false,
        }
    }

    pub fn with_no_file_pass(mut self, skip: bool) -> Self {
        self.no_file_pass = skip;
        self
    }

    pub fn discover_checks(&self, discovery: &Discovery) -> Vec<Check> {
        discover(discovery)
            .iter()
            .filter_map(|f| match parse(f) {
                Ok(c) => Some(c),
                Err(e) => {
                    warn!(path = %f.path.display(), error = %e, "skipping unparseable check");
                    None
                }
            })
            .filter(|c| {
                self.check_filter
                    .as_ref()
                    .is_none_or(|re| re.is_match(&c.name))
            })
            .collect()
    }

    pub async fn run<F>(&self, discovery: &Discovery, on_progress: F) -> ReviewOutcome
    where
        F: Fn(ReviewProgress) + Send + Sync + 'static,
    {
        let on_progress = Arc::new(on_progress);
        let checks = self.discover_checks(discovery);
        on_progress(ReviewProgress::ChecksStarted {
            total: checks.len(),
        });
        let semaphore = Arc::new(Semaphore::new(self.max_concurrent));
        let mut handles = Vec::new();
        for check in checks {
            let sem = Arc::clone(&semaphore);
            let model = check.model.clone().or_else(|| self.model.clone());
            let name = check.name.clone();
            let turn_limit = check.turn_limit;
            let severity_default = check.severity_default;
            let tools = merge_tools(&check.tools);
            let prompt = build_check_prompt(&check);
            let on_progress = Arc::clone(&on_progress);
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.ok();
                on_progress(ReviewProgress::CheckStarted { name: name.clone() });
                let outcome = run_subprocess(&prompt, model.as_deref(), turn_limit, &tools).await;
                match outcome {
                    Ok(raw) => {
                        let findings: Vec<_> = raw
                            .into_iter()
                            .map(|mut r| {
                                if r.check.is_none() {
                                    r.check = Some(name.clone());
                                }
                                r.to_finding(severity_default)
                            })
                            .collect();
                        on_progress(ReviewProgress::CheckFinished {
                            name: name.clone(),
                            findings: findings.len(),
                            errored: false,
                        });
                        Ok(findings)
                    }
                    Err(message) => {
                        on_progress(ReviewProgress::CheckFinished {
                            name: name.clone(),
                            findings: 0,
                            errored: true,
                        });
                        Err(ReviewError {
                            check: name,
                            message,
                        })
                    }
                }
            }));
        }

        let mut outcome = ReviewOutcome::default();
        for h in handles {
            match h.await {
                Ok(Ok(findings)) => outcome.findings.extend(findings),
                Ok(Err(e)) => outcome.errors.push(e),
                Err(e) => outcome.errors.push(ReviewError {
                    check: "task".into(),
                    message: e.to_string(),
                }),
            }
        }
        if !self.no_file_pass {
            let (findings, errors) = self.main_pass(&on_progress).await;
            outcome.findings.extend(findings);
            outcome.errors.extend(errors);
        }
        outcome.findings = self.filter(outcome.findings);
        outcome
    }

    async fn main_pass<F>(&self, on_progress: &Arc<F>) -> (Vec<Finding>, Vec<ReviewError>)
    where
        F: Fn(ReviewProgress) + Send + Sync + 'static,
    {
        let files = touched_files().await;
        if files.is_empty() {
            return (Vec::new(), Vec::new());
        }
        on_progress(ReviewProgress::FilePassStarted { total: files.len() });
        let semaphore = Arc::new(Semaphore::new(self.max_concurrent));
        let mut handles = Vec::new();
        for file in files {
            let sem = Arc::clone(&semaphore);
            let model = self.model.clone();
            let prompt = build_file_prompt(&file);
            let tools: Vec<String> = DEFAULT_REVIEW_TOOLS.iter().map(|s| (*s).into()).collect();
            let on_progress = Arc::clone(on_progress);
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.ok();
                let outcome = run_subprocess(&prompt, model.as_deref(), None, &tools).await;
                match outcome {
                    Ok(raw) => {
                        let findings: Vec<_> = raw
                            .into_iter()
                            .map(|mut r| {
                                if r.check.is_none() {
                                    r.check = Some("main".to_string());
                                }
                                r.to_finding(Severity::Medium)
                            })
                            .collect();
                        on_progress(ReviewProgress::FileReviewed {
                            file: file.clone(),
                            findings: findings.len(),
                            errored: false,
                        });
                        Ok(findings)
                    }
                    Err(message) => {
                        on_progress(ReviewProgress::FileReviewed {
                            file: file.clone(),
                            findings: 0,
                            errored: true,
                        });
                        Err(ReviewError {
                            check: format!("main:{file}"),
                            message,
                        })
                    }
                }
            }));
        }
        let mut findings = Vec::new();
        let mut errors = Vec::new();
        for h in handles {
            match h.await {
                Ok(Ok(f)) => findings.extend(f),
                Ok(Err(e)) => errors.push(e),
                Err(e) => errors.push(ReviewError {
                    check: "main".into(),
                    message: e.to_string(),
                }),
            }
        }
        (findings, errors)
    }

    fn filter(&self, findings: Vec<Finding>) -> Vec<Finding> {
        let min = self.min_severity.map(|s| s.to_priority());
        findings
            .into_iter()
            .filter(|f| match min {
                None => true,
                Some(min_p) => priority_rank(f.priority) <= priority_rank(min_p),
            })
            .collect()
    }
}

/// Lower rank = more severe. Used to keep a finding when its priority is at
/// least as severe as the configured minimum.
fn priority_rank(p: Priority) -> u8 {
    match p {
        Priority::P0 => 0,
        Priority::P1 => 1,
        Priority::P2 => 2,
        Priority::P3 => 3,
    }
}

/// Merge a check's declared tools with the default review tools, deduping by
/// snake_case name. A check may restrict the set but cannot drop the styleguide
/// and report_finding tools needed to ground and emit findings.
fn merge_tools(check_tools: &[String]) -> Vec<String> {
    let mut tools: Vec<String> = DEFAULT_REVIEW_TOOLS.iter().map(|s| (*s).into()).collect();
    for t in check_tools {
        if !tools.iter().any(|e| e == t) {
            tools.push(t.clone());
        }
    }
    tools
}

fn build_check_prompt(check: &Check) -> String {
    format!("{}\n\n{FINDINGS_INSTRUCTIONS}", check.body)
}

fn build_file_prompt(file: &str) -> String {
    format!(
        "Review the file `{file}` for correctness, code quality, and security issues. \
Use the `read` tool to inspect it, then report issues.\n\n{FINDINGS_INSTRUCTIONS}"
    )
}

async fn touched_files() -> Vec<String> {
    let output = match Command::new("git")
        .arg("diff")
        .arg("--name-only")
        .arg("HEAD")
        .output()
        .await
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(String::from)
        .filter(|l| !l.is_empty())
        .collect()
}

fn build_subprocess_args(
    model: Option<&str>,
    turn_limit: Option<u32>,
    tools: &[String],
) -> Vec<String> {
    let mut args = vec![
        "run".into(),
        "--no-session".into(),
        "--quiet".into(),
        "--output-format".into(),
        "json".into(),
    ];
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    if let Some(max) = turn_limit {
        args.push("--max-turns".into());
        args.push(max.to_string());
    }
    if !tools.is_empty() {
        args.push("--allowed-tools".into());
        args.push(tools.join(","));
    }
    args
}

async fn run_subprocess(
    prompt: &str,
    model: Option<&str>,
    turn_limit: Option<u32>,
    tools: &[String],
) -> Result<Vec<RawFinding>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("resolve craft binary: {e}"))?;
    let args = build_subprocess_args(model, turn_limit, tools);
    let mut cmd = Command::new(exe);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn craft subprocess: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        drop(stdin);
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("review subprocess failed: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_findings(&text)
}

fn parse_findings(run_output: &str) -> Result<Vec<RawFinding>, String> {
    let run_json: Value =
        serde_json::from_str(run_output).map_err(|e| format!("parse subprocess output: {e}"))?;
    if run_json.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
        let msg = run_json
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(format!("agent error: {msg}"));
    }
    let result = run_json
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let findings_json = extract_json_object(result).unwrap_or(Value::Null);
    let findings = findings_json
        .get("findings")
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<RawFinding>(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    Ok(findings)
}

fn extract_json_object(text: &str) -> Option<Value> {
    if let Some(start) = text.find("```json")
        && let after = &text[start + "```json".len()..]
        && let Some(end) = after.find("```")
        && let Ok(v) = serde_json::from_str::<Value>(after[..end].trim())
    {
        return Some(v);
    }
    if let Some(start) = text.find("```")
        && let after = &text[start + 3..]
        && let Some(end) = after.find("```")
        && let Ok(v) = serde_json::from_str::<Value>(after[..end].trim())
    {
        return Some(v);
    }
    if let (Some(s), Some(e)) = (text.find('{'), text.rfind('}'))
        && e > s
        && let Ok(v) = serde_json::from_str::<Value>(&text[s..=e])
    {
        return Some(v);
    }
    serde_json::from_str::<Value>(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_object_from_plain() {
        let v = extract_json_object(r#"{"findings": []}"#).unwrap();
        assert!(v.get("findings").unwrap().is_array());
    }

    #[test]
    fn extract_json_object_from_fenced() {
        let v = extract_json_object(
            "Here you go:\n```json\n{\"findings\": [{\"severity\": \"high\"}]}\n```\n",
        )
        .unwrap();
        assert_eq!(
            v.get("findings")
                .and_then(|f| f.as_array())
                .map(|a| a.len()),
            Some(1)
        );
    }

    #[test]
    fn extract_json_object_from_prose_and_object() {
        let v = extract_json_object("Sure! {\"findings\": []} done").unwrap();
        assert!(v.get("findings").unwrap().is_array());
    }

    #[test]
    fn parse_findings_extracts_with_rule_ids_and_confidence() {
        let run_output = r#"{"result":"{\"findings\":[{\"file_path\":\"a.rs\",\"severity\":\"low\",\"title\":\"t\",\"rule_ids\":[\"rust-1\"],\"confidence\":0.5}]}", "is_error": false}"#;
        let findings = parse_findings(run_output).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].file_path.as_deref(), Some("a.rs"));
        assert_eq!(findings[0].severity, Some(Severity::Low));
        assert_eq!(findings[0].rule_ids, vec!["rust-1".to_string()]);
        assert_eq!(findings[0].confidence, Some(0.5));
    }

    #[test]
    fn parse_findings_handles_empty() {
        let run_output = r#"{"result":"{\"findings\": []}", "is_error": false}"#;
        assert!(parse_findings(run_output).unwrap().is_empty());
    }

    #[test]
    fn parse_findings_surfaces_agent_error() {
        let run_output = r#"{"result":"model not available", "is_error": true}"#;
        let err = parse_findings(run_output).unwrap_err();
        assert!(err.contains("model not available"));
    }

    #[test]
    fn parse_findings_errors_on_garbage() {
        assert!(parse_findings("not json").is_err());
    }

    #[test]
    fn build_file_prompt_mentions_file() {
        let p = build_file_prompt("src/lib.rs");
        assert!(p.contains("src/lib.rs"));
    }

    #[test]
    fn build_subprocess_args_base_only() {
        let args = build_subprocess_args(None, None, &[]);
        assert_eq!(
            args,
            vec!["run", "--no-session", "--quiet", "--output-format", "json"]
        );
    }

    #[test]
    fn build_subprocess_args_with_model_and_turn_limit() {
        let args = build_subprocess_args(Some("anthropic/claude"), Some(5), &[]);
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"anthropic/claude".to_string()));
        assert!(args.contains(&"--max-turns".to_string()));
        assert!(args.contains(&"5".to_string()));
    }

    #[test]
    fn build_subprocess_args_with_tools() {
        let tools = vec!["read".into(), "grep".into()];
        let args = build_subprocess_args(None, None, &tools);
        let allowed_idx = args.iter().position(|a| a == "--allowed-tools").unwrap();
        assert!(args[allowed_idx + 1].contains("read"));
        assert!(args[allowed_idx + 1].contains("grep"));
    }

    #[test]
    fn merge_tools_includes_styleguide_by_default() {
        let tools = merge_tools(&[]);
        assert!(tools.contains(&STYLEGUIDE_GET_TOOL_NAME.to_string()));
        assert!(tools.contains(&STYLEGUIDE_SEARCH_TOOL_NAME.to_string()));
        assert!(tools.contains(&"read".to_string()));
        assert!(tools.contains(&"grep".to_string()));
    }

    #[test]
    fn merge_tools_dedupes_check_declared_tools() {
        let declared = vec!["read".into(), "edit".into()];
        let tools = merge_tools(&declared);
        let read_count = tools.iter().filter(|t| *t == "read").count();
        assert_eq!(read_count, 1);
        assert!(tools.contains(&"edit".to_string()));
    }

    #[test]
    fn filter_keeps_at_or_above_min_severity() {
        let orchestrator = ReviewOrchestrator::new(None, None, Some(Severity::High));
        let findings = vec![
            make_finding(Priority::P0, "critical"),
            make_finding(Priority::P1, "high"),
            make_finding(Priority::P2, "medium"),
            make_finding(Priority::P3, "low"),
        ];
        let kept = orchestrator.filter(findings);
        let kept_priorities: Vec<Priority> = kept.iter().map(|f| f.priority).collect();
        assert_eq!(kept_priorities, vec![Priority::P0, Priority::P1]);
    }

    #[test]
    fn filter_keeps_all_when_no_min() {
        let orchestrator = ReviewOrchestrator::new(None, None, None);
        let findings = vec![
            make_finding(Priority::P2, "m"),
            make_finding(Priority::P3, "l"),
        ];
        assert_eq!(orchestrator.filter(findings).len(), 2);
    }

    #[test]
    fn priority_rank_orders_by_severity() {
        assert!(priority_rank(Priority::P0) < priority_rank(Priority::P1));
        assert!(priority_rank(Priority::P1) < priority_rank(Priority::P2));
        assert!(priority_rank(Priority::P2) < priority_rank(Priority::P3));
    }

    fn make_finding(priority: Priority, title: &str) -> Finding {
        Finding {
            title: title.into(),
            body: String::new(),
            priority,
            confidence: 0.7,
            file_path: String::new(),
            line_start: 0,
            line_end: 0,
            rule_ids: Vec::new(),
            suggestion: None,
        }
    }
}
