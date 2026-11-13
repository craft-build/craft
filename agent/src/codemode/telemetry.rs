//! One JSONL record per `code` run, appended best-effort to
//! `paths::state_dir()/codemode.jsonl`. Written from the async side, which
//! is the only place model, session, and clock are reachable; IO errors
//! log and never fail the tool call.

use std::{collections::BTreeMap, io::Write, path::PathBuf};

use serde::Serialize;

use super::bridge::{RunOutcome, Tally};

const FILE_NAME: &str = "codemode.jsonl";
const MAX_DETAIL_BYTES: usize = 512;

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
pub struct ToolTally {
    pub ok: usize,
    pub err: usize,
}

/// The wire shape of one record; `craft stats --codemode` deserializes the
/// same fields.
#[derive(Debug, Serialize)]
pub struct Record<'a> {
    pub ts: String,
    pub model: &'a str,
    pub session_id: &'a str,
    pub outcome: &'a str,
    pub detail: String,
    pub calls: usize,
    pub tools: BTreeMap<&'a str, ToolTally>,
    pub duration_ms: u128,
    pub script_lines: usize,
}

#[cfg(test)]
static TEST_STATE_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn telemetry_path() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(dir) = TEST_STATE_DIR.lock().unwrap().clone() {
        return Some(dir.join(FILE_NAME));
    }
    crate::paths::state_dir()
        .ok()
        .map(|dir| dir.join(FILE_NAME))
}

/// Redirects telemetry into a fixture directory for one test; call with
/// `None` to restore the default resolution. Tests that keep telemetry on
/// must hold this for their whole run so concurrent records stay isolated.
#[cfg(test)]
pub(crate) fn set_test_state_dir(dir: Option<PathBuf>) {
    *TEST_STATE_DIR.lock().unwrap() = dir;
}

pub fn record(
    outcome: &RunOutcome,
    model: Option<&str>,
    session_id: Option<&str>,
    script_lines: usize,
) {
    let failure = outcome.failure.as_ref();
    let detail = failure.map_or("", |failure| failure.detail.as_str());
    let record = Record {
        ts: jiff::Timestamp::now().to_string(),
        model: model.unwrap_or("unknown"),
        session_id: session_id.unwrap_or("unknown"),
        outcome: failure.map_or("ok", |failure| failure.class.as_str()),
        detail: truncate(detail),
        calls: outcome.calls,
        tools: outcome
            .tallies
            .iter()
            .map(|(tool, Tally { ok, err })| (tool.as_str(), ToolTally { ok: *ok, err: *err }))
            .collect(),
        duration_ms: outcome.duration.as_millis(),
        script_lines,
    };
    let Some(path) = telemetry_path() else {
        return;
    };
    if let Err(error) = append_to(&path, &record) {
        tracing::warn!(
            path = %path.display(),
            %error,
            "codemode telemetry write failed"
        );
    }
}

fn append_to(path: &std::path::Path, record: &Record<'_>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line =
        serde_json::to_string(record).map_err(|error| std::io::Error::other(error.to_string()))?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

fn truncate(text: &str) -> String {
    let mut end = text.len().min(MAX_DETAIL_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::codemode::diagnose::{Class, Failure};

    fn outcome() -> RunOutcome {
        let mut tallies = BTreeMap::new();
        tallies.insert("read_text".to_owned(), Tally { ok: 2, err: 0 });
        tallies.insert("edit".to_owned(), Tally { ok: 1, err: 1 });
        RunOutcome {
            text: "Completed 3 of 4 tool calls before failing\nRuntime: boom".to_owned(),
            failure: Some(Failure::plain(Class::Runtime, "ValueError: boom")),
            calls: 4,
            tallies,
            duration: std::time::Duration::from_millis(250),
        }
    }

    #[test]
    fn record_writes_one_jsonl_line_per_call_with_expected_fields() {
        // One test, sequential calls: the state-dir override is process-wide.
        let dir = tempfile::tempdir().unwrap();
        set_test_state_dir(Some(dir.path().to_path_buf()));
        record(&outcome(), Some("anthropic/m"), Some("s1"), 12);
        let ok = RunOutcome {
            text: "42\n".to_owned(),
            failure: None,
            calls: 1,
            tallies: BTreeMap::new(),
            duration: std::time::Duration::from_millis(5),
        };
        record(&ok, None, None, 2);
        set_test_state_dir(None);

        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        let mut lines = text.lines();
        let failure_line: serde_json::Value =
            serde_json::from_str(lines.next().expect("failure record")).unwrap();
        let ok_line: serde_json::Value =
            serde_json::from_str(lines.next().expect("ok record")).unwrap();
        assert_eq!(lines.next(), None, "exactly one line per call");

        assert_eq!(failure_line["model"], "anthropic/m");
        assert_eq!(failure_line["session_id"], "s1");
        assert_eq!(failure_line["outcome"], "runtime");
        assert_eq!(failure_line["detail"], "ValueError: boom");
        assert_eq!(failure_line["calls"], 4);
        assert_eq!(failure_line["script_lines"], 12);
        assert_eq!(failure_line["duration_ms"], 250);
        assert_eq!(
            failure_line["tools"]["read_text"],
            serde_json::json!({"ok": 2, "err": 0})
        );
        assert_eq!(
            failure_line["tools"]["edit"],
            serde_json::json!({"ok": 1, "err": 1})
        );
        assert!(
            failure_line["ts"].as_str().unwrap().contains('T'),
            "RFC3339 ts"
        );

        assert_eq!(ok_line["outcome"], "ok");
        assert_eq!(ok_line["model"], "unknown");
        assert_eq!(ok_line["session_id"], "unknown");
        assert_eq!(ok_line["detail"], "");
    }

    #[test]
    fn detail_is_capped_on_a_char_boundary() {
        let long = "é".repeat(600);
        assert_eq!(truncate(&long).chars().count(), 256);
        assert_eq!(truncate("short"), "short");
    }
}
