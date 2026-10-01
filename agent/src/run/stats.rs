//! Run-wide usage and cost accounting: the per-model ledger folded into
//! the terminal `Event::Done`, and the spec to bill for each served call.

use std::collections::HashMap;
use std::sync::Arc;

use crate::history;

/// Run-wide accumulators for the terminal [`Event::Done`](super::Event::Done).
#[derive(Default)]
pub(crate) struct RunStats {
    pub(crate) usage: history::Usage,
    pub(crate) context_size: u64,
    pub(crate) turns: u32,
    /// Per-model ledger (H.6): turns are priced when they run and their cost
    /// recorded, so summing is the truth (see `usage::settle_session`).
    pub(crate) by_model: HashMap<String, crate::usage::StoredTokenUsage>,
}

/// The spec to bill: the model that actually answered the call. Retry-chain
/// fallbacks and reauth-refreshed models carry only a bare model id, so the
/// primary spec's provider prefixes it.
pub(crate) fn served_spec(
    primary: Option<&str>,
    served_fallback: Option<&str>,
    refreshed: Option<&crate::providers::DynamicModel>,
) -> Option<Arc<str>> {
    let primary = primary?;
    let label = served_fallback
        .or_else(|| refreshed.and_then(|m| m.label()))
        .unwrap_or_else(|| primary.rsplit_once('/').map_or(primary, |(_, m)| m));
    // A label with a slash is already a full spec (possibly cross-provider);
    // only a bare id borrows the primary's provider.
    let spec = if label.contains('/') {
        label.to_owned()
    } else {
        let provider = primary.split_once('/').map_or(primary, |(p, _)| p);
        format!("{provider}/{label}")
    };
    Some(spec.into())
}

impl RunStats {
    /// Fold one model call's usage into the ledger, pricing the turn against
    /// today's table. An unresolvable spec still counts its tokens, unpriced.
    pub(crate) fn add_usage(&mut self, usage: &history::Usage, spec: Option<&str>, fast: bool) {
        self.usage.add(*usage);
        let Some(spec) = spec else { return };
        let tokens = crate::usage::TokenUsage::from(usage);
        let cost = crate::usage::resolve_spec(spec).and_then(|m| m.billed_cost(&tokens, fast));
        *self.by_model.entry(spec.to_owned()).or_default() += tokens.billed(cost);
    }
}
