//! Per-model usage accounting and the session cost ledger.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};

use super::super::{AgentEvent, UsageRow};

/// Session usage state: per-model token totals folded from each finished
/// run's `Event::Done` (the `/usage` overlay reads them), plus the
/// `cost.jsonl` ledger in the state dir. `ledger` is `None` when the state
/// dir is unavailable, meaning cost records are simply not written.
#[derive(Default)]
pub(super) struct UsageLedger {
    usage_by_model: HashMap<String, crate::usage::StoredTokenUsage>,
    ledger: Option<crate::storage::stats::CostLedger>,
    /// Id under which this TUI session's cost records are filed.
    session_id: String,
    /// Serializes blocking ledger appends across concurrent runs so records
    /// reach `cost.jsonl` in completion order.
    ledger_io: Arc<Mutex<()>>,
}

impl UsageLedger {
    /// Open the state dir's cost ledger and mint this session's id.
    pub(super) fn open() -> Self {
        Self {
            ledger: crate::storage::StateDir::resolve()
                .ok()
                .and_then(|dir| crate::storage::stats::CostLedger::from_state_dir(&dir).ok()),
            session_id: crate::id::SessionRef::generate().id().to_string(),
            ..Self::default()
        }
    }

    /// Per-model usage rows for the `/usage` overlay.
    pub(super) fn rows(&self) -> Vec<UsageRow> {
        let mut rows: Vec<UsageRow> = self
            .usage_by_model
            .iter()
            .map(|(spec, usage)| {
                let tokens = crate::usage::TokenUsage::from(*usage);
                UsageRow {
                    model: spec.clone(),
                    tokens: u64::from(tokens.total_input().saturating_add(tokens.output)),
                    cost: usage.cost,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.model.cmp(&b.model));
        rows
    }
}

/// Fold a finished run's per-model usage into the session totals and append
/// one `cost.jsonl` record per model. Ledger failures warn and never fail the
/// turn; a model whose cost could not be resolved records `cost_usd: null`
/// with its real token counts rather than a misleading `$0.00`.
pub(super) async fn record_run_usage(
    state: &Arc<Mutex<super::SessionState>>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    by_model: HashMap<String, crate::usage::StoredTokenUsage>,
    fast: bool,
) {
    if by_model.is_empty() {
        return;
    }
    let (records, ledger, ledger_io, rows) = {
        let mut session = state.lock().await;
        let mut records = Vec::new();
        for (spec, usage) in by_model {
            if session.usage.ledger.is_some() {
                let (provider, model) = spec.split_once('/').unwrap_or(("", spec.as_str()));
                records.push(crate::storage::stats::make_record(
                    session.usage.session_id.clone(),
                    model,
                    provider,
                    crate::storage::stats::CostUsage::from_stored(&usage),
                    usage.cost,
                    fast,
                ));
            }
            *session.usage.usage_by_model.entry(spec).or_default() += usage;
        }
        (
            records,
            session.usage.ledger.clone(),
            session.usage.ledger_io.clone(),
            session.usage.rows(),
        )
    };
    if let Some(ledger) = ledger {
        // flock + write + fsync are blocking; keep them off the async worker,
        // outside the session-state lock, and serialized across runs.
        let _guard = ledger_io.lock().await;
        if let Err(e) = tokio::task::spawn_blocking(move || {
            for record in &records {
                if let Err(e) = ledger.append(record) {
                    eprintln!("warning: failed to append cost record: {e}");
                }
            }
        })
        .await
        {
            eprintln!("warning: cost ledger append task failed: {e}");
        }
    }
    let _ = tx.send(AgentEvent::UsageSnapshot(rows));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::provider::live::SessionState;

    fn stored(input: u32, output: u32, cost: Option<f64>) -> crate::usage::StoredTokenUsage {
        crate::usage::StoredTokenUsage {
            input,
            output,
            cost,
            ..Default::default()
        }
    }

    fn session_with_ledger(root: &std::path::Path) -> SessionState {
        let dir = crate::storage::StateDir::from_path(root.to_path_buf());
        SessionState {
            usage: UsageLedger {
                ledger: Some(crate::storage::stats::CostLedger::from_state_dir(&dir).unwrap()),
                session_id: "s1".into(),
                ..UsageLedger::default()
            },
            ..SessionState::linked()
        }
    }

    fn ledger_records(root: &std::path::Path) -> Vec<crate::storage::stats::CostRecord> {
        let text = std::fs::read_to_string(root.join("cost.jsonl")).unwrap();
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn run_usage_folds_into_session_and_cost_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(session_with_ledger(tmp.path())));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut by_model = HashMap::new();
        by_model.insert(
            "anthropic/claude-sonnet-5".to_owned(),
            stored(100, 50, Some(0.001)),
        );
        by_model.insert("mock/free-model".to_owned(), stored(10, 5, None));

        record_run_usage(&state, &tx, by_model, false).await;

        let session = state.lock().await;
        assert_eq!(session.usage.usage_by_model.len(), 2);
        assert_eq!(
            session.usage.usage_by_model["anthropic/claude-sonnet-5"].cost,
            Some(0.001)
        );
        assert_eq!(session.usage.usage_by_model["mock/free-model"].cost, None);

        let records = ledger_records(tmp.path());
        assert_eq!(records.len(), 2, "one record per model");
        let by_spec: std::collections::BTreeMap<(&str, &str), (&Option<f64>, u64)> = records
            .iter()
            .map(|r| {
                (
                    (r.provider.as_str(), r.model.as_str()),
                    (&r.cost_usd, r.usage.total()),
                )
            })
            .collect();
        assert_eq!(by_spec[&("anthropic", "claude-sonnet-5")].1, 150);
        assert_eq!(by_spec[&("mock", "free-model")].0, &None);
        assert!(records.iter().all(|r| r.session_id == "s1"));

        assert!(matches!(rx.try_recv(), Ok(AgentEvent::UsageSnapshot(rows)) if rows.len() == 2));
    }

    #[tokio::test]
    async fn ledger_failure_never_breaks_the_turn() {
        let tmp = tempfile::tempdir().unwrap();
        // A directory where cost.jsonl should live makes every append fail.
        std::fs::create_dir(tmp.path().join("cost.jsonl")).unwrap();
        let state = Arc::new(Mutex::new(session_with_ledger(tmp.path())));
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut by_model = HashMap::new();
        by_model.insert(
            "anthropic/claude-sonnet-5".to_owned(),
            stored(1, 1, Some(0.0)),
        );

        record_run_usage(&state, &tx, by_model, false).await;

        // The session totals were still folded; only the ledger write warned.
        assert_eq!(state.lock().await.usage.usage_by_model.len(), 1);
    }
}
