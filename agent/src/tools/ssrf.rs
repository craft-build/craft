//! SSRF guard for outbound web requests (webfetch, task #56).
//!
//! Ported from the reference `craft-lua/src/api/net.rs` (comparison.md points
//! at `internal_urls.rs`, which actually holds internal URL scheme dispatch —
//! the code wins). Behavior:
//!
//! - Only `http:`/`https:` URLs; `http:` is upgraded to `https:`.
//! - Literal IPs and every DNS answer are checked against private, loopback,
//!   link-local, unspecified, unique-local, and reserved ranges (including
//!   CGNAT 100.64/10 where Alibaba parks instance metadata at
//!   100.100.100.200, and IPv4-mapped IPv6 spellings of all of the above).
//! - The checked DNS answers are returned so the caller pins them on its
//!   reqwest client via `resolve_to_addrs`, closing the classic DNS-rebinding
//!   window between check and connect.
//! - Redirects are capped and may not hop to a private literal IP. Any
//!   cross-host hop is stopped (not connected): the caller re-runs
//!   `resolve_and_check_ssrf` on the target URL and rebuilds the client with
//!   newly pinned addresses, so no hop ever connects on unvetted DNS.
//!
//! A resolver failure is the network's, not the guard's verdict: unresolvable
//! hosts report a lookup error, never a "blocked:" message.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

/// A resolver saying "try again" (a cold cache, a link that just came back)
/// has not answered yet, so a couple of retries go out before a name lookup
/// counts as a failure. The request's own retry budget never covers this,
/// since the guard runs before the first attempt.
const DNS_ATTEMPTS: u32 = 3;
const DNS_RETRY_DELAY: Duration = Duration::from_millis(150);
pub const MAX_REDIRECTS: usize = 10;

/// Reserved IPv4 ranges the standard library has no predicate for. Carrier
/// grade NAT is the one that bites: Alibaba Cloud parks its instance metadata
/// service on it at 100.100.100.200. Then protocol assignments, benchmarking,
/// and everything from 240.0.0.0 up, which takes in the broadcast address.
const RESERVED_V4_NETS: [(Ipv4Addr, u8); 4] = [
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];

/// The host plus its vetted addresses, ready for
/// `reqwest::ClientBuilder::resolve_to_addrs`.
#[derive(Debug, Clone)]
pub struct GuardedDns {
    pub host: String,
    pub addrs: Vec<SocketAddr>,
}

/// Accept only `http://`/`https://` and upgrade plain http to https.
pub fn validate_and_upgrade_url(url: &str) -> Result<String, String> {
    if let Some(rest) = url.strip_prefix("http://") {
        return Ok(format!("https://{rest}"));
    }
    if url.starts_with("https://") {
        return Ok(url.to_string());
    }
    Err(format!(
        "URL must start with http:// or https://, got: {url}"
    ))
}

/// Parse the URL once with the `url` crate and return its canonical host and
/// port. `url`'s host parsing lowercases domains and normalizes trailing
/// dots, and reqwest keys its `resolve_to_addrs` override by the very same
/// `host_str()` value — so THIS canonical string is the only key the caller
/// may pin `GuardedDns` addresses under. Hand-parsing the URL string here
/// instead would let spellings like `EXAMPLE.com` or `example.com.` silently
/// miss the pin and fall back to live (unvetted) DNS.
fn canonical_host_and_port(url: &str) -> Result<(String, u16), String> {
    // The URL version in use rejects zone IDs outright, which would turn a
    // fail-closed "blocked" verdict into a parse error; pre-check so a scoped
    // literal can never fall through to DNS as a hostname.
    if let Some(start) = url.find('[')
        && let Some(end) = url[start..].find(']')
        && url[start..start + end].contains('%')
    {
        return Err("blocked: zone-scoped IPv6 address is not allowed".into());
    }
    let parsed = url::Url::parse(url).map_err(|e| format!("cannot parse URL '{url}': {e}"))?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err(format!("URL must be http:// or https://, got: {url}"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("userinfo (user:pass@) is not allowed in URLs".into());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("cannot extract host from URL: {url}"))?
        .to_string();
    // Explicit ports win; webfetch upgrades http to https first, so the
    // scheme default is 443 in practice.
    let port = parsed.port_or_known_default().unwrap_or(443);
    Ok((host, port))
}

/// Resolve the URL's host and vet every answer. On success the caller must pin
/// `addrs` on its client so the connection cannot be re-resolved to a
/// different (possibly private) address between check and connect.
pub async fn resolve_and_check_ssrf(url: &str) -> Result<GuardedDns, String> {
    let (host, port) = canonical_host_and_port(url)?;

    if let Some(ip) = parse_literal_ip(&host)? {
        if is_private_ip(&ip) {
            return Err(format!("blocked: {ip} is a private/metadata address"));
        }
        return Ok(GuardedDns {
            host,
            addrs: vec![SocketAddr::new(ip, port)],
        });
    }

    let addr = format!("{host}:{port}");
    let addrs: Vec<_> = resolve(&addr)
        .await
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .collect();

    if addrs.is_empty() {
        return Err(format!("no addresses found for {host}"));
    }

    for sa in &addrs {
        if is_private_ip(&sa.ip()) {
            return Err(format!(
                "blocked: {host} resolves to private address {}",
                sa.ip()
            ));
        }
    }

    Ok(GuardedDns { host, addrs })
}

/// What to do with a redirect hop. `Stop` hands the hop back to the caller,
/// who must re-run `resolve_and_check_ssrf` on the target URL and rebuild the
/// client with the new pinned addresses before continuing — otherwise a fresh
/// hostname would connect on unvetted system DNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectAction {
    Follow,
    Stop,
    Blocked(String),
}

pub fn redirect_action(previous_host: Option<&str>, next: &reqwest::Url) -> RedirectAction {
    let Some(raw_host) = next.host_str() else {
        return RedirectAction::Blocked("redirect target has no host".into());
    };
    // `host_str` keeps the brackets on IPv6 literals; strip them for parsing.
    let host = raw_host.trim_start_matches('[').trim_end_matches(']');
    match parse_literal_ip(host) {
        Err(reason) => return RedirectAction::Blocked(reason),
        Ok(Some(ip)) if is_private_ip(&ip) => {
            return RedirectAction::Blocked(format!(
                "blocked: redirect to private/metadata address {ip}"
            ));
        }
        _ => {}
    }
    if previous_host.is_some_and(|prev| prev != host) {
        RedirectAction::Stop
    } else {
        RedirectAction::Follow
    }
}

/// Redirect policy for guarded clients: capped hops, private/metadata literal
/// IPs refused, and cross-host hops stopped for re-validation. Same-host hops
/// stay pinned to the addresses already vetted by `resolve_and_check_ssrf`.
pub fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        let previous_host = attempt.previous().last().and_then(|u| u.host_str());
        match redirect_action(previous_host, attempt.url()) {
            RedirectAction::Follow => attempt.follow(),
            RedirectAction::Stop => attempt.stop(),
            RedirectAction::Blocked(reason) => attempt.error(reason),
        }
    })
}

/// Retried lookup around `tokio::net::lookup_host`, which already runs
/// getaddrinfo on the blocking pool rather than on the executor thread.
async fn resolve(addr: &str) -> std::io::Result<impl Iterator<Item = SocketAddr>> {
    let mut attempt = 1;
    loop {
        match tokio::net::lookup_host(addr).await {
            Ok(addrs) => return Ok(addrs),
            Err(e) if attempt == DNS_ATTEMPTS => return Err(e),
            Err(_) => {}
        }
        attempt += 1;
        tokio::time::sleep(DNS_RETRY_DELAY).await;
    }
}

/// Parse a URL host as a literal IP, normalizing spellings the URL parser,
/// std, and resolvers disagree on. IPv4-mapped and IPv4-compatible IPv6 forms
/// collapse to their v4 address so every later check sees one
/// representation. Zone-scoped literals fail closed instead of falling
/// through to DNS, where a scoped address could slip past as a hostname.
fn parse_literal_ip(host: &str) -> Result<Option<IpAddr>, String> {
    if host.contains('%') {
        return Err(format!("blocked: {host} is a zone-scoped address"));
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => Ok(Some(normalize_v6(ip))),
        Err(_) => Ok(None),
    }
}

fn normalize_v6(ip: IpAddr) -> IpAddr {
    let IpAddr::V6(v6) = ip else { return ip };
    if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4()) {
        return IpAddr::V4(v4);
    }
    ip
}

fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            // 0.0.0.0/8 ("this network", and what IPv4-compatible IPv6 forms
            // like ::1 normalize to) has no std predicate; fail closed on it.
            v4.octets()[0] == 0
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || is_reserved_v4(*v4)
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(&IpAddr::V4(v4));
            }
            if let Some(v4) = v6.to_ipv4() {
                return is_private_ip(&IpAddr::V4(v4));
            }
            let bytes = v6.octets();
            // 64:ff9b::/96 NAT64 embeds the target IPv4 address in the low
            // 32 bits; judge the embedded address, not the public-looking v6.
            if bytes[..12] == [0, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0] {
                let v4 = Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]);
                if is_private_ip(&IpAddr::V4(v4)) {
                    return true;
                }
            }
            // fe80::/10 link-local and fec0::/10 site-local. Site-local was
            // deprecated rather than withdrawn, and stacks still route it.
            if bytes[0] == 0xfe && matches!(bytes[1] & 0xc0, 0x80 | 0xc0) {
                return true;
            }
            if bytes[0] & 0xfe == 0xfc {
                return true;
            }
            false
        }
    }
}

fn ip_in_net(ip: IpAddr, net: IpAddr, prefix: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(ip), IpAddr::V4(net)) => {
            let shift = 32 - prefix;
            (u32::from(ip) >> shift) == (u32::from(net) >> shift)
        }
        (IpAddr::V6(ip), IpAddr::V6(net)) => {
            let shift = 128 - prefix;
            (u128::from(ip) >> shift) == (u128::from(net) >> shift)
        }
        _ => false,
    }
}

fn is_reserved_v4(v4: Ipv4Addr) -> bool {
    RESERVED_V4_NETS
        .iter()
        .any(|(net, prefix)| ip_in_net(v4.into(), (*net).into(), *prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn https_passes_through_and_http_upgrades() {
        assert_eq!(
            validate_and_upgrade_url("https://example.com").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            validate_and_upgrade_url("http://example.com").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn unsupported_schemes_and_bare_domains_rejected() {
        assert!(validate_and_upgrade_url("ftp://example.com").is_err());
        assert!(validate_and_upgrade_url("example.com").is_err());
    }

    /// Regression for the DNS-pin key mismatch: the URL must be parsed with
    /// the `url` crate (not hand-split), because reqwest parses the request
    /// URL with the same crate and keys `resolve_to_addrs` by its
    /// `host_str()`. Any spelling difference between the pin key and the
    /// host reqwest connects to silently falls back to unvetted live DNS.
    #[test]
    fn canonical_host_matches_reqwest_resolve_key() {
        // Mixed case: reqwest connects to a URL parsed with the same crate,
        // so its key is `example.com`; the vetted GuardedDns host must be
        // exactly that string for the pin to apply.
        assert_eq!(
            canonical_host_and_port("https://EXample.COM/path").unwrap(),
            ("example.com".to_string(), 443)
        );
        // Trailing-dot spelling: the url crate keeps the dot, and reqwest
        // will ask for exactly the same string — the invariant is that the
        // pin key IS the url-crate `host_str()`, dot or no dot, never a
        // hand-re-parsed variant of it.
        assert_eq!(
            canonical_host_and_port("https://www.example.com./").unwrap(),
            (
                url::Url::parse("https://www.example.com./")
                    .unwrap()
                    .host_str()
                    .unwrap()
                    .to_string(),
                443
            )
        );
        // Ports: explicit port wins; https defaults to 443.
        assert_eq!(
            canonical_host_and_port("https://example.com:8443/x").unwrap(),
            ("example.com".to_string(), 8443)
        );
        // IPv6 literal: the url crate's `host_str()` keeps the brackets, and
        // reqwest's override lookup uses the URI host in the same bracketed
        // form — the old hand-parser stripped brackets and its IPv6 pins
        // silently missed.
        assert_eq!(
            canonical_host_and_port("https://[::1]:9090/path").unwrap(),
            ("[::1]".to_string(), 9090)
        );
        // Garbage and non-http(s) schemes are rejected, as is userinfo.
        assert!(canonical_host_and_port("not-a-url").is_err());
        assert!(canonical_host_and_port("ftp://example.com").is_err());
        assert!(canonical_host_and_port("https://user:pass@127.0.0.1/").is_err());
    }

    #[tokio::test]
    async fn userinfo_urls_are_rejected_clearly() {
        let err = resolve_and_check_ssrf("https://user:pass@127.0.0.1/")
            .await
            .unwrap_err();
        assert!(err.contains("userinfo"), "{err}");
    }

    #[tokio::test]
    async fn public_literal_ip_allowed() {
        let dns = resolve_and_check_ssrf("https://8.8.8.8").await.unwrap();
        assert_eq!(dns.host, "8.8.8.8");
        assert_eq!(dns.addrs.len(), 1);
        assert_eq!(dns.addrs[0].port(), 443);
    }

    #[tokio::test]
    async fn literal_ip_with_port_keeps_port() {
        let dns = resolve_and_check_ssrf("https://1.1.1.1:8443/x")
            .await
            .unwrap();
        assert_eq!(dns.addrs[0].port(), 8443);
    }

    #[tokio::test]
    async fn loopback_blocked() {
        assert!(resolve_and_check_ssrf("https://127.0.0.1").await.is_err());
    }

    #[tokio::test]
    async fn rfc1918_blocked() {
        assert!(resolve_and_check_ssrf("https://192.168.1.1").await.is_err());
        assert!(resolve_and_check_ssrf("https://10.0.0.1").await.is_err());
        assert!(resolve_and_check_ssrf("https://172.16.0.1").await.is_err());
    }

    #[tokio::test]
    async fn cloud_metadata_blocked() {
        assert!(
            resolve_and_check_ssrf("https://169.254.169.254")
                .await
                .is_err()
        );
        assert!(
            resolve_and_check_ssrf("https://100.100.100.200")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn ipv6_blocked_spellings() {
        assert!(resolve_and_check_ssrf("https://[::1]").await.is_err());
        assert!(
            resolve_and_check_ssrf("https://[::ffff:127.0.0.1]")
                .await
                .is_err()
        );
        assert!(
            resolve_and_check_ssrf("https://[::ffff:169.254.169.254]")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unspecified_blocked() {
        assert!(resolve_and_check_ssrf("https://0.0.0.0").await.is_err());
    }

    /// A resolver with no answer has reached no verdict, so the failure must
    /// not be worded as a block. `.invalid` is reserved by RFC 6761, so every
    /// resolver answers NXDOMAIN for it.
    #[tokio::test]
    async fn unresolvable_host_is_a_network_failure_not_a_block() {
        const HOST: &str = "craft.invalid";
        let err = resolve_and_check_ssrf(&format!("https://{HOST}/"))
            .await
            .expect_err(HOST);
        assert!(!err.starts_with("blocked:"), "{err}");
        assert!(err.contains(HOST), "{err}");
    }

    fn private(ip: IpAddr) -> bool {
        is_private_ip(&ip)
    }

    #[test]
    fn v4_predicate_boundaries() {
        assert!(private(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))));
        assert!(private(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(private(IpAddr::V4(Ipv4Addr::new(172, 31, 255, 255))));
        assert!(!private(IpAddr::V4(Ipv4Addr::new(172, 32, 0, 1))));
        assert!(private(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 1))));
        assert!(!private(IpAddr::V4(Ipv4Addr::new(1, 0, 0, 1))));
    }

    #[test]
    fn ipv6_mapped_and_local_ranges() {
        assert!(private(IpAddr::V6(Ipv6Addr::new(
            0, 0, 0, 0, 0, 0xffff, 0x0a00, 0x0001
        ))));
        assert!(!private(IpAddr::V6(Ipv6Addr::new(
            0, 0, 0, 0, 0, 0xffff, 0x0808, 0x0808
        ))));
        assert!(private(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
        assert!(private(IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        ))));
        assert!(private(IpAddr::V6(Ipv6Addr::new(
            0xfc00, 0, 0, 0, 0, 0, 0, 1
        ))));
        assert!(private(IpAddr::V6(Ipv6Addr::new(
            0xfd00, 0, 0, 0, 0, 0, 0, 1
        ))));
        assert!(!private(IpAddr::V6(Ipv6Addr::new(
            0x2001, 0xdb8, 0, 0, 0, 0, 0, 1
        ))));
    }

    /// Ranges the standard library has no predicate for. Each is checked in
    /// both spellings, because `::ffff:100.100.100.200` reaches the same host
    /// as `100.100.100.200`.
    #[test]
    fn reserved_v4_blocked_in_both_spellings() {
        for v4 in [
            Ipv4Addr::new(100, 100, 100, 200),
            Ipv4Addr::new(100, 64, 0, 0),
            Ipv4Addr::new(100, 127, 255, 255),
            Ipv4Addr::new(192, 0, 0, 1),
            Ipv4Addr::new(198, 18, 0, 1),
            Ipv4Addr::new(198, 19, 255, 255),
            Ipv4Addr::new(240, 0, 0, 1),
            Ipv4Addr::BROADCAST,
        ] {
            assert!(private(IpAddr::V4(v4)), "{v4}");
            assert!(private(IpAddr::V6(v4.to_ipv6_mapped())), "{v4} mapped");
        }
    }

    /// The address just past each range, so a prefix that is one bit too wide
    /// does not pass unnoticed.
    #[test]
    fn addresses_beside_reserved_ranges_stay_public() {
        for v4 in [
            Ipv4Addr::new(100, 63, 255, 255),
            Ipv4Addr::new(100, 128, 0, 0),
            Ipv4Addr::new(192, 0, 1, 1),
            Ipv4Addr::new(198, 20, 0, 0),
            Ipv4Addr::new(198, 17, 255, 255),
        ] {
            assert!(!private(IpAddr::V4(v4)), "{v4}");
            assert!(!private(IpAddr::V6(v4.to_ipv6_mapped())), "{v4} mapped");
        }
    }

    #[test]
    fn redirect_action_decisions() {
        let same = reqwest::Url::parse("https://example.com/next").unwrap();
        assert_eq!(
            redirect_action(Some("example.com"), &same),
            RedirectAction::Follow
        );
        assert_eq!(redirect_action(None, &same), RedirectAction::Follow);

        let cross = reqwest::Url::parse("https://other.example.org/").unwrap();
        assert_eq!(
            redirect_action(Some("example.com"), &cross),
            RedirectAction::Stop
        );

        let private = reqwest::Url::parse("https://169.254.169.254/").unwrap();
        assert!(matches!(
            redirect_action(Some("example.com"), &private),
            RedirectAction::Blocked(_)
        ));
        let mapped = reqwest::Url::parse("https://[::ffff:a00:1]/").unwrap();
        assert!(matches!(
            redirect_action(None, &mapped),
            RedirectAction::Blocked(_)
        ));
    }

    #[test]
    fn ipv4_compatible_and_nat64_spellings() {
        assert!(private(IpAddr::V6(Ipv6Addr::new(
            0, 0, 0, 0, 0, 0, 0x7f00, 1
        ))));
        assert!(private(IpAddr::V6(Ipv6Addr::new(
            0x0064, 0xff9b, 0, 0, 0, 0, 0x0a00, 0x0001
        ))));
        assert!(!private(IpAddr::V6(Ipv6Addr::new(
            0x0064, 0xff9b, 0, 0, 0, 0, 0x0808, 0x0808
        ))));
        assert_eq!(
            normalize_v6(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0x0808, 0x0808))),
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))
        );
    }

    #[tokio::test]
    async fn zone_scoped_literals_fail_closed() {
        let err = resolve_and_check_ssrf("https://[::1%25eth0]/")
            .await
            .unwrap_err();
        assert!(err.starts_with("blocked:"), "{err}");
        let err = parse_literal_ip("fe80::1%eth0").unwrap_err();
        assert!(err.starts_with("blocked:"), "{err}");
    }

    #[tokio::test]
    async fn dotted_quad_mapped_literal_blocked() {
        assert!(
            resolve_and_check_ssrf("https://[::ffff:10.0.0.1]")
                .await
                .is_err()
        );
    }
}
