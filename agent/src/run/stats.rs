//! Run-wide usage and cost accounting: the per-model ledger folded into
//! the terminal `Event::Done`, and the spec to bill for each served call.

use std::collections::HashMap;
use std::sync::Arc;

use crate::usage::TokenUsage;

/// Run-wide accumulators for the terminal [`Event::Done`](super::Event::Done).
#[derive(Default)]
pub(crate) struct RunStats {
    pub(crate) usage: TokenUsage,
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
    served_fallback: Option<String>,
    refreshed: Option<&crate::provider::ModelHandle>,
) -> Option<Arc<str>> {
    let primary = primary?;
    let label = served_fallback
        .or_else(|| refreshed.map(|m| m.model.id.clone()))
        .unwrap_or_else(|| {
            primary
                .rsplit_once('/')
                .map_or(primary, |(_, m)| m)
                .to_owned()
        });
    // A label with a slash is already a full spec (possibly cross-provider);
    // only a bare id borrows the primary's provider.
    let spec = if label.contains('/') {
        label
    } else {
        let provider = primary.split_once('/').map_or(primary, |(p, _)| p);
        format!("{provider}/{label}")
    };
    Some(spec.into())
}

impl RunStats {
    /// Fold one model call's usage into the ledger, pricing the turn against
    /// today's table. An unresolvable spec still counts its tokens, unpriced.
    pub(crate) fn add_usage(&mut self, usage: &TokenUsage, spec: Option<&str>, fast: bool) {
        crate::usage::accumulate(&mut self.usage, usage);
        let Some(spec) = spec else { return };
        let cost = crate::usage::resolve_spec(spec).and_then(|m| m.billed_cost(usage, fast));
        *self.by_model.entry(spec.to_owned()).or_default() += crate::usage::billed(usage, cost);
    }
}
