//! Provider usage panels (the TUI `/usage` overlay): per-plan limits and
//! today's per-model spend. Every provider-specific usage endpoint from
//! the pre-animist endpoint is gone; [`fetch_usage`] keeps the `Ok(None)` contract
//! every surface already handles, so panels render local session data
//! until a provider ships a usage API worth wiring (animist owns provider
//! clients; a revival lands there first).

use crate::config::ProviderConfig;
use crate::error::Result;

/// One row of "today's spend by model" for the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelUsageRow {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub spend_microdollars: u64,
}

/// One plan limit line: a label, an optional fill percentage, and an
/// optional reset time (unix seconds) with human detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageLimit {
    pub label: String,
    pub percentage: Option<u32>,
    pub reset_at: Option<u64>,
    pub detail: Option<String>,
}

/// A provider's usage snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderUsage {
    pub plan: Option<String>,
    pub limits: Vec<UsageLimit>,
    pub by_model_today: Vec<ModelUsageRow>,
}

/// Fetch a provider's usage panel. No supported provider exposes one
/// today, so this always answers `None` — surfaces fall back to the
/// session ledger.
pub async fn fetch_usage(_config: &ProviderConfig) -> Result<Option<ProviderUsage>> {
    Ok(None)
}
