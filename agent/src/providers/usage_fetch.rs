//! Provider-side usage quota fetch (F.5), ported from the reference
//! `Provider::fetch_usage` (craft-providers/src/provider.rs:341).
//!
//! Only providers with a key-authenticated usage endpoint are implemented
//! here; the reference's anthropic and openai quota endpoints are OAuth-only
//! and this repo's provider registry is key-based, so those kinds report
//! `Ok(None)` (unsupported) rather than porting a dead auth path. DeepSeek's
//! balance endpoint works with the plain API key and is ported faithfully.

use serde::{Deserialize, Serialize};

use crate::config::ProviderConfig;
use crate::error::Result;
use crate::providers::{ProviderKind, Timeouts, credential, timeout_client};

const DEEPSEEK_BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

/// A provider-reported usage quota snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUsage {
    /// Subscription/plan level when the provider reports one (e.g. "lite").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub limits: Vec<UsageLimit>,
    /// Per-model breakdown of the current UTC day, when the provider exposes
    /// one. Sorted by the provider (typically spend desc).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_model_today: Vec<ModelUsageRow>,
}

/// One row of a provider-reported per-model usage table. Money is kept as
/// integer micro-dollars to keep `ProviderUsage` in `Eq` territory for tests;
/// callers format at the UI layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsageRow {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// Spend in micro-dollars (1_000_000 = $1.00).
    pub spend_microdollars: u64,
}

/// A single quota window (e.g. a 5-hour or weekly token quota).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLimit {
    /// Human-readable label for the window, provided by the provider.
    pub label: String,
    /// Usage percentage within the window, 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percentage: Option<u32>,
    /// When the window resets, as epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<u64>,
    /// Extra provider-supplied context, e.g. "$2.33 spent" for usage credits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Fetch the provider-side usage quota. `Ok(None)` means the provider does
/// not expose a programmatic usage endpoint reachable with the configured
/// credentials.
pub async fn fetch_usage(config: &ProviderConfig) -> Result<Option<ProviderUsage>> {
    if config.kind != ProviderKind::Deepseek {
        return Ok(None);
    }
    let env_name = config
        .api_key_env
        .clone()
        .or_else(|| config.kind.api_key_env_default().map(str::to_string))
        .unwrap_or_else(|| "DEEPSEEK_API_KEY".to_string());
    let key = credential(&env_name)?;
    let body = get_text(&key, DEEPSEEK_BALANCE_URL).await?;
    Ok(Some(parse_deepseek_balance(&body)))
}

async fn get_text(key: &str, url: &str) -> Result<String> {
    let client = timeout_client(Timeouts::default())?;
    let response = client
        .get(url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(crate::error::client_error)?;
    let status = response.status();
    let text = response.text().await.map_err(crate::error::client_error)?;
    if !status.is_success() {
        return crate::error::InvalidSnafu {
            reason: format!("usage endpoint returned {status}: {text}"),
        }
        .fail();
    }
    Ok(text)
}

#[derive(Deserialize)]
struct BalanceResponse {
    balance_infos: Vec<BalanceInfo>,
}

#[derive(Deserialize)]
struct BalanceInfo {
    currency: String,
    total_balance: String,
    granted_balance: String,
    topped_up_balance: String,
}

/// Ported from the reference `impl From<BalanceResponse> for ProviderUsage`
/// (craft-providers/src/providers/deepseek.rs:105).
fn parse_deepseek_balance(body: &str) -> ProviderUsage {
    let resp: BalanceResponse = serde_json::from_str(body).unwrap_or(BalanceResponse {
        balance_infos: Vec::new(),
    });
    let limits = resp
        .balance_infos
        .into_iter()
        .map(|b| {
            let symbol = match b.currency.as_str() {
                "USD" => "$",
                "CNY" => "¥",
                _ => "",
            };
            UsageLimit {
                label: "Balance".into(),
                percentage: None,
                reset_at: None,
                detail: Some(format!(
                    "total: {}{}, topped-up: {}{}, granted: {}{}",
                    symbol, b.total_balance, symbol, b.topped_up_balance, symbol, b.granted_balance
                )),
            }
        })
        .collect();
    ProviderUsage {
        plan: None,
        limits,
        by_model_today: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_reference_balance_shape() {
        let usage = parse_deepseek_balance(
            r#"{"balance_infos": [{
                "currency": "USD",
                "total_balance": "110.00",
                "granted_balance": "10.00",
                "topped_up_balance": "100.00"
            }]}"#,
        );
        assert_eq!(usage.plan, None);
        assert_eq!(usage.by_model_today, Vec::new());
        assert_eq!(usage.limits.len(), 1);
        let limit = &usage.limits[0];
        assert_eq!(limit.label, "Balance");
        assert_eq!(limit.percentage, None);
        assert_eq!(
            limit.detail.as_deref(),
            Some("total: $110.00, topped-up: $100.00, granted: $10.00")
        );
    }

    #[test]
    fn malformed_balance_body_yields_no_limits() {
        let usage = parse_deepseek_balance("not json");
        assert!(usage.limits.is_empty());
    }

    #[tokio::test]
    async fn non_deepseek_providers_are_unsupported() {
        let config = ProviderConfig {
            kind: ProviderKind::Anthropic,
            api_key_env: None,
            base_url: None,
            api_version: None,
            account_id: None,
            discover_models: false,
            models: Default::default(),
        };
        // Never touches the network or the environment.
        assert!(fetch_usage(&config).await.unwrap().is_none());
    }
}
