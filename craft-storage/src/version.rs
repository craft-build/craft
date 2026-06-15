use reqwest::Client;
use std::time::Duration;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const RELEASES_URL: &str = "https://api.github.com/repos/craft-build/craft/releases/latest";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum VersionError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("server returned HTTP {0}")]
    Status(u16),
    #[error("invalid response: {0}")]
    InvalidResponse(&'static str),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}

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

fn client() -> Result<Client, VersionError> {
    Ok(Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?)
}

fn parse_tag(bytes: &[u8]) -> Result<String, VersionError> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or(VersionError::InvalidResponse("missing tag_name"))?;
    Ok(tag.strip_prefix('v').unwrap_or(tag).to_owned())
}

pub async fn fetch_latest() -> Result<String, VersionError> {
    let resp = client()?
        .get(RELEASES_URL)
        .header("Accept", "application/json")
        .header("User-Agent", "craft")
        .send()
        .await?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(VersionError::Status(status));
    }
    let bytes = resp.bytes().await?;
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
}
