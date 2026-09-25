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
        Ok(summary) if summary.records > 0 => {
            let sessions = summary.session_count();
            let total_cost = summary.total_cost;
            let total_tokens = summary.total_tokens;
            StatsView {
                rows: summary
                    .by_model
                    .into_iter()
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
                total_cost,
                total_tokens,
                sessions,
                empty: false,
            }
        }
        _ => empty,
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
