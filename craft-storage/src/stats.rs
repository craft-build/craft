//! Append-only cost ledger. One JSONL record per agent run, used by `/stats`.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{StateDir, StorageError, now_epoch};

const COST_LEDGER_FILE: &str = "cost.jsonl";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CostUsage {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

impl CostUsage {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_creation + self.cache_read
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRecord {
    pub session_id: String,
    pub turn_id: Option<String>,
    pub ts: u64,
    pub model: String,
    pub provider: String,
    pub usage: CostUsage,
    pub cost_usd: f64,
    pub fast: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CostSummary {
    pub total_cost: f64,
    pub total_tokens: u64,
    pub by_model: Vec<(String, f64, u64)>,
    pub by_session: Vec<(String, f64, u64)>,
    pub records: usize,
}

impl CostSummary {
    pub fn session_count(&self) -> usize {
        self.by_session.len()
    }
}

pub struct CostLedger {
    path: PathBuf,
}

impl CostLedger {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join(COST_LEDGER_FILE),
        }
    }

    pub fn from_state_dir(dir: &StateDir) -> Result<Self, StorageError> {
        let path = dir.path().join(COST_LEDGER_FILE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, record: &CostRecord) -> Result<(), StorageError> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut buf = Vec::with_capacity(256);
        serde_json::to_writer(&mut buf, record)?;
        buf.push(b'\n');
        file.write_all(&buf)?;
        file.sync_data()?;
        Ok(())
    }

    pub fn read_all(&self) -> Result<Vec<CostRecord>, StorageError> {
        read_records(&self.path)
    }

    pub fn summary(&self) -> Result<CostSummary, StorageError> {
        summary_from(read_records(&self.path)?)
    }
}

fn read_records(path: &Path) -> Result<Vec<CostRecord>, StorageError> {
    let Ok(file) = File::open(path) else {
        return Ok(Vec::new());
    };
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<CostRecord>(&line) {
            Ok(r) => records.push(r),
            Err(e) => tracing::warn!(%e, "skipping malformed cost ledger line"),
        }
    }
    Ok(records)
}

fn summary_from(records: Vec<CostRecord>) -> Result<CostSummary, StorageError> {
    use std::collections::HashMap;

    let mut total_cost = 0.0f64;
    let mut total_tokens = 0u64;
    let mut by_model: HashMap<String, (f64, u64)> = HashMap::new();
    let mut by_session: HashMap<String, (f64, u64)> = HashMap::new();

    for r in &records {
        let tokens = r.usage.total();
        total_cost += r.cost_usd;
        total_tokens += tokens;
        by_model
            .entry(r.model.clone())
            .and_modify(|(c, t)| {
                *c += r.cost_usd;
                *t += tokens;
            })
            .or_insert((r.cost_usd, tokens));
        by_session
            .entry(r.session_id.clone())
            .and_modify(|(c, t)| {
                *c += r.cost_usd;
                *t += tokens;
            })
            .or_insert((r.cost_usd, tokens));
    }

    let mut by_model: Vec<(String, f64, u64)> =
        by_model.into_iter().map(|(k, (c, t))| (k, c, t)).collect();
    by_model.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut by_session: Vec<(String, f64, u64)> = by_session
        .into_iter()
        .map(|(k, (c, t))| (k, c, t))
        .collect();
    by_session.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    Ok(CostSummary {
        total_cost,
        total_tokens,
        by_model,
        by_session,
        records: records.len(),
    })
}

/// Convenience for tests and the CLI subcommand: build a record from primitives.
pub fn make_record(
    session_id: impl Into<String>,
    model: impl Into<String>,
    provider: impl Into<String>,
    usage: CostUsage,
    cost_usd: f64,
    fast: bool,
) -> CostRecord {
    CostRecord {
        session_id: session_id.into(),
        turn_id: None,
        ts: now_epoch(),
        model: model.into(),
        provider: provider.into(),
        usage,
        cost_usd,
        fast,
    }
}

const TINY_COST_THRESHOLD: f64 = 0.01;
const TOKEN_K: u64 = 1_000;
const TOKEN_M: u64 = 1_000_000;

/// Format a USD cost, showing extra precision for tiny amounts.
pub fn format_usd(v: f64) -> String {
    if v < TINY_COST_THRESHOLD && v > 0.0 {
        format!("${v:.4}")
    } else {
        format!("${v:.2}")
    }
}

/// Compact token count: `1.2k`, `3.40M`, else the raw integer.
pub fn format_tokens(v: u64) -> String {
    if v >= TOKEN_M {
        format!("{:.2}M", v as f64 / TOKEN_M as f64)
    } else if v >= TOKEN_K {
        format!("{:.1}k", v as f64 / TOKEN_K as f64)
    } else {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn tmp_ledger() -> (tempfile::TempDir, CostLedger) {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger = CostLedger::new(dir.path());
        (dir, ledger)
    }

    fn rec(model: &str, cost: f64, tokens: u64) -> CostRecord {
        make_record(
            crate::id::CraftId::generate().to_string(),
            model,
            "anthropic",
            CostUsage {
                input: tokens / 2,
                output: tokens / 2,
                ..Default::default()
            },
            cost,
            false,
        )
    }

    #[test]
    fn append_read_roundtrip() {
        let (_tmp, ledger) = tmp_ledger();
        ledger.append(&rec("claude-sonnet", 0.1, 1000)).unwrap();
        ledger.append(&rec("claude-opus", 0.5, 5000)).unwrap();
        let records = ledger.read_all().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].model, "claude-sonnet");
        assert_eq!(records[1].cost_usd, 0.5);
    }

    #[test]
    fn summary_aggregates_by_model_and_session() {
        let (_tmp, ledger) = tmp_ledger();
        ledger
            .append(&CostRecord {
                session_id: "s1".into(),
                turn_id: None,
                ts: 1,
                model: "sonnet".into(),
                provider: "anthropic".into(),
                usage: CostUsage {
                    input: 100,
                    output: 50,
                    ..Default::default()
                },
                cost_usd: 0.2,
                fast: false,
            })
            .unwrap();
        ledger
            .append(&CostRecord {
                session_id: "s1".into(),
                turn_id: None,
                ts: 2,
                model: "sonnet".into(),
                provider: "anthropic".into(),
                usage: CostUsage {
                    input: 200,
                    output: 0,
                    ..Default::default()
                },
                cost_usd: 0.3,
                fast: false,
            })
            .unwrap();
        ledger
            .append(&CostRecord {
                session_id: "s2".into(),
                turn_id: None,
                ts: 3,
                model: "opus".into(),
                provider: "anthropic".into(),
                usage: CostUsage {
                    input: 0,
                    output: 1000,
                    ..Default::default()
                },
                cost_usd: 1.0,
                fast: false,
            })
            .unwrap();

        let s = ledger.summary().unwrap();
        assert_eq!(s.records, 3);
        assert!((s.total_cost - 1.5).abs() < 1e-9);
        assert_eq!(s.total_tokens, 1350);
        assert_eq!(s.by_model.len(), 2);
        assert_eq!(s.by_model[0].0, "opus");
        assert!((s.by_model[0].1 - 1.0).abs() < 1e-9);
        assert_eq!(s.by_model[1].0, "sonnet");
        assert!((s.by_model[1].1 - 0.5).abs() < 1e-9);
        assert_eq!(s.by_session.len(), 2);
        assert_eq!(s.by_session[0].0, "s2");
        assert!((s.by_session[0].1 - 1.0).abs() < 1e-9);
        assert_eq!(s.by_session[1].0, "s1");
    }

    #[test_case(0.0 ; "zero_cost")]
    #[test_case(0.001 ; "tiny_cost")]
    fn handles_edge_costs(cost: f64) {
        let (_tmp, ledger) = tmp_ledger();
        ledger.append(&rec("m", cost, 10)).unwrap();
        let s = ledger.summary().unwrap();
        assert!((s.total_cost - cost).abs() < 1e-9);
    }

    #[test]
    fn empty_ledger_summary_is_empty() {
        let (_tmp, ledger) = tmp_ledger();
        let s = ledger.summary().unwrap();
        assert_eq!(s.records, 0);
        assert!(s.total_cost.abs() < 1e-9);
        assert!(s.by_model.is_empty());
    }

    #[test]
    fn append_is_concurrency_safe() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(COST_LEDGER_FILE);
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let ledger = CostLedger { path };
                    let mut record = rec("m", 0.01, 100);
                    record.session_id = format!("s{i}");
                    ledger.append(&record).unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let ledger = CostLedger { path };
        let records = ledger.read_all().unwrap();
        assert_eq!(records.len(), 8);
    }
}
