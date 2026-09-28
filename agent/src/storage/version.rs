//! Latest-release check, ported from the reference
//! `craft-storage/src/version.rs` (task G.7).

use std::time::Duration;

use reqwest::Client;
use serde_json::Value;

use crate::error::{InvalidSnafu, Result};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const RELEASES_URL: &str = "https://api.github.com/repos/craft-build/craft/releases/latest";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Strict `major.minor.patch` ordering: pre-release or malformed strings on
/// either side compare as "not newer", so a garbage tag never triggers an
/// update.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Option<(u32, u32, u32)> {
        let mut it = s.split('.');
        Some((
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
        ))
    };
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

fn client() -> Result<Client> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| {
            InvalidSnafu {
                reason: format!("build HTTP client: {e}"),
            }
            .build()
        })
}

fn parse_tag(bytes: &[u8]) -> Result<String> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| {
        InvalidSnafu {
            reason: format!("parse release response: {e}"),
        }
        .build()
    })?;
    let tag = v.get("tag_name").and_then(|t| t.as_str()).ok_or_else(|| {
        InvalidSnafu {
            reason: "release response has no tag_name".to_string(),
        }
        .build()
    })?;
    Ok(tag.strip_prefix('v').unwrap_or(tag).to_owned())
}

pub async fn fetch_latest() -> Result<String> {
    let resp = client()?
        .get(RELEASES_URL)
        .header("Accept", "application/json")
        .header("User-Agent", "craft")
        .send()
        .await
        .map_err(|e| {
            InvalidSnafu {
                reason: format!("check latest release: {e}"),
            }
            .build()
        })?;
    let status = resp.status().as_u16();
    if status != 200 {
        return InvalidSnafu {
            reason: format!("release check returned HTTP {status}"),
        }
        .fail();
    }
    let bytes = resp.bytes().await.map_err(|e| {
        InvalidSnafu {
            reason: format!("read release response: {e}"),
        }
        .build()
    })?;
    parse_tag(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("0.2.0", "0.1.0", true  ; "minor_bump")]
    #[test_case("1.0.0", "0.9.9", true  ; "major_bump")]
    #[test_case("0.1.1", "0.1.0", true  ; "patch_bump")]
    #[test_case("0.1.0", "0.1.0", false ; "equal")]
    #[test_case("0.0.9", "0.1.0", false ; "older")]
    #[test_case("abc",   "0.1.0", false ; "garbage_latest")]
    #[test_case("1.0.0-rc1", "0.9.0", false ; "prerelease_ignored")]
    fn is_newer_cases(latest: &str, current: &str, expected: bool) {
        assert_eq!(is_newer(latest, current), expected);
    }

    #[test]
    fn parse_tag_strips_the_v_prefix() {
        assert_eq!(parse_tag(br#"{"tag_name":"v0.14.1"}"#).unwrap(), "0.14.1");
        assert_eq!(parse_tag(br#"{"tag_name":"0.14.1"}"#).unwrap(), "0.14.1");
    }

    #[test]
    fn parse_tag_rejects_missing_tag() {
        assert!(parse_tag(br#"{"message":"Not Found"}"#).is_err());
    }
}
