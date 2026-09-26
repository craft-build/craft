//! Sidebar data loading: stats aggregation and persisted-session listings.

use crate::tui::modals::{SessionEntry, StatsView};
use crate::tui::provider::UsageRow;

/// Aggregate `cost.jsonl` for the `/stats` overlay. A missing state dir or
/// ledger reads as an empty table ("no runs recorded").
pub(crate) fn load_stats() -> StatsView {
    let empty = StatsView {
        empty: true,
        ..StatsView::default()
    };
    let Ok(dir) = crate::storage::StateDir::resolve() else {
        return empty;
    };
    let Ok(ledger) = crate::storage::stats::CostLedger::from_state_dir(&dir) else {
        return empty;
    };
    match ledger.summary() {
        Ok(summary) if summary.records > 0 => stats_view(summary),
        _ => empty,
    }
}

/// Build the `/stats` view from a cost-ledger summary, capping the by-model
/// table at 12 rows (the rest folds into an overflow line) and the top
/// sessions at 8, like the reference stats modal.
fn stats_view(summary: crate::storage::stats::CostSummary) -> StatsView {
    const MAX_MODEL_ROWS: usize = 12;
    const MAX_SESSION_ROWS: usize = 8;
    let sessions = summary.session_count();
    let models_overflow = summary.by_model.len().saturating_sub(MAX_MODEL_ROWS);
    let by_session = summary
        .by_session
        .into_iter()
        .take(MAX_SESSION_ROWS)
        .collect::<Vec<_>>();

    StatsView {
        rows: summary
            .by_model
            .into_iter()
            .take(MAX_MODEL_ROWS)
            .map(|(model, cost, tokens)| {
                // A $0 total on a spec the pricing table does not know
                // means unpriced, not free — show "—" like `/usage`.
                let cost = if cost == 0.0 && crate::usage::resolve_spec(&model).is_none() {
                    None
                } else {
                    Some(cost)
                };
                UsageRow {
                    model,
                    tokens,
                    cost,
                }
            })
            .collect(),
        by_session,
        models_overflow,
        total_cost: summary.total_cost,
        total_tokens: summary.total_tokens,
        sessions,
        empty: false,
    }
}

/// Entries for the `/sessions` picker: newest first, filtered to this cwd
/// like the headless session lookup.
pub(crate) fn load_session_entries() -> Vec<SessionEntry> {
    let Ok(dir) = crate::storage::StateDir::resolve() else {
        return Vec::new();
    };
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string());
    crate::headless::StoredSession::list(cwd.as_deref(), &dir)
        .unwrap_or_default()
        .into_iter()
        .map(|summary| SessionEntry {
            id: summary.id.as_str().to_owned(),
            title: summary.title,
            updated: rel_age(summary.updated_at),
        })
        .collect()
}

/// Relative age ("2h ago") of an epoch timestamp.
fn rel_age(epoch: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(epoch);
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::stats::CostSummary;

    #[test]
    fn stats_view_caps_models_and_sessions() {
        let summary = CostSummary {
            total_cost: 3.0,
            total_tokens: 10_000,
            by_model: (0..15).map(|i| (format!("p/m{i}"), 0.1, 100)).collect(),
            by_session: (0..10).map(|i| (format!("s{i}"), 0.2, 200)).collect(),
            records: 25,
        };
        let view = stats_view(summary);
        assert_eq!(view.rows.len(), 12);
        assert_eq!(view.models_overflow, 3);
        assert_eq!(view.by_session.len(), 8);
        assert_eq!(view.sessions, 10, "session count stays uncapped");
        assert!(!view.empty);
        assert_eq!(view.rows[0].model, "p/m0");
    }
}
