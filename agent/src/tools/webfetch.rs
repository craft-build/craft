//! `webfetch` tool: fetch an http(s) URL and return it as markdown, text, or
//! html (task #56, A.4).
//!
//! Ported from the reference `plugins/webfetch/init.lua` (tool semantics:
//! formats, caps, content-type handling) on top of `craft-lua/src/api/net.rs`
//! (`do_request`: retry ladder, Cloudflare-challenge fallback, size caps).
//!
//! Deviation from the reference, inherited from this repo's stricter SSRF
//! guard (`ssrf.rs`): the reference pins vetted DNS answers but lets reqwest
//! follow every redirect internally. Here redirects use `ssrf::redirect_policy()`,
//! which stops cross-host hops; the loop below then re-runs the guard on the
//! target URL and rebuilds the client with newly pinned addresses, so no hop
//! ever connects on unvetted DNS.

use std::time::Duration;

use rig_core::tool::{IntoToolOutput, PortableTool, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::ssrf::{self, GuardedDns};
use super::{MAX_OUTPUT_BYTES, Result, invalid};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;
const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
const MAX_RETRIES: u32 = 3;
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const FALLBACK_USER_AGENT: &str = "craft";
const CF_MITIGATED: &str = "cf-mitigated";
const CF_CHALLENGE: &str = "challenge";
/// Tags whose entire contents are dropped by the `text` format.
const SKIP_TAGS: [&str; 3] = ["script", "style", "noscript"];

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebfetchArgs {
    /// URL to fetch (http:// or https://). Plain http is upgraded to https.
    pub url: String,
    /// Output format: markdown (default), text, or html.
    pub format: Option<String>,
    /// Timeout in seconds (default 30, max 120).
    pub timeout: Option<u64>,
}

#[derive(Debug)]
pub struct WebfetchOutput {
    pub text: String,
}

impl IntoToolOutput for WebfetchOutput {
    fn into_tool_output(
        self,
    ) -> std::result::Result<ToolOutput, rig_core::tool::ToolExecutionError> {
        Ok(ToolOutput::text(self.text))
    }
}

/// Fetch a URL through the SSRF guard and return its contents as text.
#[derive(Clone, Copy, Default)]
pub struct Webfetch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Markdown,
    Text,
    Html,
}

impl Format {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "markdown" => Some(Self::Markdown),
            "text" => Some(Self::Text),
            "html" => Some(Self::Html),
            _ => None,
        }
    }

    fn accept_header(self) -> &'static str {
        match self {
            Self::Html => "text/html,*/*;q=0.5",
            Self::Text => "text/plain,text/html;q=0.9,*/*;q=0.5",
            Self::Markdown => "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.5",
        }
    }
}

impl PortableTool for Webfetch {
    const NAME: &'static str = "webfetch";
    type Args = WebfetchArgs;
    type Output = WebfetchOutput;
    type Error = rig_core::tool::ToolExecutionError;

    fn description(&self) -> String {
        "Fetch a URL and return its contents.\n\n\
         - Supports markdown (default), text, or html output formats.\n\
         - HTTP URLs are auto-upgraded to HTTPS.\n\
         - Max response size is 5MB, max timeout is 120s.\n\
         - Best used inside code_execution with some truncation / filter to \
         avoid context bloat."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(WebfetchArgs))
            .expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output> {
        let url = ssrf::validate_and_upgrade_url(&args.url).map_err(invalid)?;
        let format = match args.format.as_deref() {
            None => Format::Markdown,
            Some(raw) => {
                Format::parse(raw).ok_or_else(|| invalid(format!("unknown format: {raw}")))?
            }
        };
        let timeout = resolve_timeout(args.timeout);

        let body = fetch(&url, format, timeout).await.map_err(invalid)?;
        Ok(WebfetchOutput {
            text: truncate_output(&body),
        })
    }
}

fn resolve_timeout(raw: Option<u64>) -> Duration {
    Duration::from_secs(
        raw.unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS),
    )
}

/// Guard, connect, and follow redirects one validated hop at a time. Each
/// iteration pins freshly vetted addresses on a new client, so a redirect to
/// a different host can never connect on unvetted system DNS.
async fn fetch(
    url: &str,
    format: Format,
    timeout: Duration,
) -> std::result::Result<String, String> {
    let mut url = url.to_string();
    let mut redirects = 0usize;

    loop {
        let guarded = ssrf::resolve_and_check_ssrf(&url).await?;
        let response = request_with_retries(&url, format, timeout, &guarded).await?;

        // A stopped redirect comes back as the 3xx response itself; re-guard
        // the target before connecting again.
        let redirect_target = if response.status().is_redirection() {
            response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(|loc| {
                    response
                        .url()
                        .join(loc)
                        .map_err(|e| format!("invalid redirect target {loc:?}: {e}"))
                })
                .transpose()?
        } else {
            None
        };
        if let Some(next) = redirect_target {
            redirects += 1;
            if redirects > ssrf::MAX_REDIRECTS {
                return Err("too many redirects".into());
            }
            url = next.to_string();
            continue;
        }

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if let Some(len) = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            && len > MAX_RESPONSE_BYTES
        {
            return Err(format!("response too large: {len} bytes"));
        }

        let mut response = response;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("read error: {e}"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(format!(
                    "response too large: over {MAX_RESPONSE_BYTES} bytes"
                ));
            }
            body.extend_from_slice(&chunk);
        }

        let rendered = render_body(&String::from_utf8_lossy(&body), &content_type, format)?;
        return Ok(rendered);
    }
}

/// One client with DNS pinned to `guarded`, plus the reference's retry ladder:
/// up to three retries on transport errors and 5xx, and one `craft`
/// User-Agent retry when a GET is answered by a Cloudflare challenge page.
async fn request_with_retries(
    url: &str,
    format: Format,
    timeout: Duration,
    guarded: &GuardedDns,
) -> std::result::Result<reqwest::Response, String> {
    let client = reqwest::Client::builder()
        .resolve_to_addrs(&guarded.host, &guarded.addrs)
        .timeout(timeout)
        .redirect(ssrf::redirect_policy())
        .build()
        .map_err(|e| format!("client error: {e}"))?;

    let mut last_err = String::new();
    for attempt in 0..=MAX_RETRIES {
        let request = client
            .get(url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", format.accept_header())
            .build()
            .map_err(|e| format!("request build error: {e}"))?;
        match client.execute(request).await {
            Ok(response) => {
                let status = response.status().as_u16();
                let is_cf_challenge = status == 403
                    && response
                        .headers()
                        .get(CF_MITIGATED)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.contains(CF_CHALLENGE));
                if is_cf_challenge {
                    let retry = client
                        .get(url)
                        .header("User-Agent", FALLBACK_USER_AGENT)
                        .header("Accept", format.accept_header())
                        .build()
                        .map_err(|e| format!("request build error: {e}"))?;
                    match client.execute(retry).await {
                        Ok(response) => return Ok(response),
                        Err(e) => last_err = format!("request failed: {e}"),
                    }
                } else if status >= 500 && attempt < MAX_RETRIES {
                    last_err = format!("HTTP {status}");
                } else {
                    return Ok(response);
                }
            }
            Err(e) if attempt < MAX_RETRIES => last_err = format!("request failed: {e}"),
            Err(e) => return Err(format!("request failed: {e}")),
        }
    }
    Err(last_err)
}

fn is_image_content_type(content_type: &str) -> bool {
    content_type.starts_with("image/") && !content_type.contains("svg")
}

fn render_body(
    body: &str,
    content_type: &str,
    format: Format,
) -> std::result::Result<String, String> {
    if is_image_content_type(content_type) {
        return Err("image content cannot be displayed as text".into());
    }
    let is_html = content_type.contains("text/html");
    Ok(match format {
        Format::Markdown if is_html => htmd::convert(body).unwrap_or_else(|_| body.to_string()),
        Format::Text if is_html => strip_html(body),
        _ => body.to_string(),
    })
}

/// Strip tags for the `text` format: drop `script`/`style`/`noscript`
/// contents entirely, replace every tag with a single space, collapse runs
/// of whitespace, and trim. Ported from the reference `strip_html`.
fn strip_html(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut tag_buf = String::new();
    let mut skip_tag: Option<String> = None;
    let mut last_was_space = true;

    for ch in html.chars() {
        if ch == '<' {
            in_tag = true;
            tag_buf.clear();
        } else if in_tag && ch == '>' {
            in_tag = false;
            let tag_name = tag_buf.to_lowercase();
            let tag_name = tag_name.split_whitespace().next();
            if let Some(name) = tag_name {
                if let Some(open) = &skip_tag {
                    if name.strip_prefix('/') == Some(open.as_str()) {
                        skip_tag = None;
                    }
                } else if SKIP_TAGS.contains(&name) {
                    skip_tag = Some(name.to_string());
                }
            }
            if skip_tag.is_none() && !out.is_empty() && !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else if in_tag {
            tag_buf.push(ch);
        } else if skip_tag.is_some() {
            // inside a skipped element
        } else if ch.is_whitespace() {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }

    out.trim().to_string()
}

fn truncate_output(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_string();
    }
    let mut cut = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n…[truncated at {} bytes]",
        &text[..cut],
        MAX_OUTPUT_BYTES
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_parsing_and_accept_headers() {
        assert_eq!(Format::parse("markdown"), Some(Format::Markdown));
        assert_eq!(Format::parse("text"), Some(Format::Text));
        assert_eq!(Format::parse("html"), Some(Format::Html));
        assert_eq!(Format::parse("pdf"), None);
        assert_eq!(
            Format::Markdown.accept_header(),
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.5"
        );
        assert_eq!(
            Format::Text.accept_header(),
            "text/plain,text/html;q=0.9,*/*;q=0.5"
        );
        assert_eq!(Format::Html.accept_header(), "text/html,*/*;q=0.5");
    }

    #[test]
    fn timeout_clamping() {
        assert_eq!(resolve_timeout(None), Duration::from_secs(30));
        assert_eq!(resolve_timeout(Some(10)), Duration::from_secs(10));
        assert_eq!(resolve_timeout(Some(999)), Duration::from_secs(120));
        assert_eq!(resolve_timeout(Some(0)), Duration::from_secs(1));
    }

    #[test]
    fn image_content_type_detection() {
        assert!(is_image_content_type("image/png"));
        assert!(is_image_content_type("image/jpeg; charset=binary"));
        assert!(!is_image_content_type("image/svg+xml"));
        assert!(!is_image_content_type("text/html"));
        assert!(!is_image_content_type(""));
    }

    #[test]
    fn render_body_routes_by_format_and_content_type() {
        let html = "<p>Hello <b>world</b></p>";
        assert_eq!(
            render_body(html, "text/html", Format::Text).unwrap(),
            "Hello world"
        );
        // markdown conversion of real HTML produces markdown, not the raw body
        let md = render_body(html, "text/html", Format::Markdown).unwrap();
        assert_ne!(md, html);
        assert!(md.contains("world"));
        // non-HTML content passes through untouched for every format
        for format in [Format::Markdown, Format::Text, Format::Html] {
            assert_eq!(render_body("plain", "text/plain", format).unwrap(), "plain");
        }
        // images are refused regardless of format
        for format in [Format::Markdown, Format::Text, Format::Html] {
            let err = render_body("GIF89a", "image/gif", format).unwrap_err();
            assert_eq!(err, "image content cannot be displayed as text");
        }
    }

    #[test]
    fn truncate_output_respects_cap_and_boundaries() {
        let short = "hello";
        assert_eq!(truncate_output(short), "hello");
        let long = "ä".repeat(MAX_OUTPUT_BYTES); // 2 bytes per char
        let truncated = truncate_output(&long);
        assert!(truncated.len() < long.len());
        assert!(truncated.ends_with(&format!("…[truncated at {MAX_OUTPUT_BYTES} bytes]")));
        assert!(truncated.is_char_boundary(truncated.len() - 1));
    }

    // ── strip_html: port of the reference spec.lua cases ──

    #[test]
    fn strip_html_nested_tags_and_whitespace() {
        assert_eq!(
            strip_html("<div><p>Hello <b>world</b></p></div>"),
            "Hello world"
        );
        assert_eq!(
            strip_html("   <p>  lots   of    spaces  </p>   "),
            "lots of spaces"
        );
        assert_eq!(strip_html("<p>line1\n\n\nline2</p>"), "line1 line2");
    }

    #[test]
    fn strip_html_skip_tags() {
        assert_eq!(
            strip_html("before<script>alert('xss')</script>after"),
            "before after"
        );
        assert_eq!(
            strip_html("before<style>.a{color:red}</style>after"),
            "before after"
        );
        assert_eq!(
            strip_html("before<noscript>enable js</noscript>after"),
            "before after"
        );
        assert_eq!(strip_html("a<SCRIPT>evil()</SCRIPT>b"), "a b");
        assert_eq!(
            strip_html("a<script>var x = '<div>not real</div>';</script>b"),
            "a b"
        );
    }

    #[test]
    fn strip_html_mixed_content() {
        assert_eq!(
            strip_html("<p>keep</p><script>drop</script><p>also keep</p>"),
            "keep also keep"
        );
        assert_eq!(strip_html("<td>cell1</td><td>cell2</td>"), "cell1 cell2");
        assert_eq!(
            strip_html(r#"<a href="http://example.com" class="link">click</a>"#),
            "click"
        );
        assert_eq!(strip_html("before<br/>after"), "before after");
    }

    #[test]
    fn strip_html_edge_cases() {
        assert_eq!(strip_html(""), "");
        assert_eq!(strip_html("<div><span></span></div>"), "");
        assert_eq!(strip_html("hello<div"), "hello");
    }

    #[tokio::test]
    async fn rejects_unknown_format_before_any_network() {
        let error = Webfetch
            .call(WebfetchArgs {
                url: "https://example.com".into(),
                format: Some("pdf".into()),
                timeout: None,
            })
            .await
            .expect_err("invalid format");
        assert!(error.to_string().contains("unknown format: pdf"));
    }

    #[tokio::test]
    async fn rejects_non_http_scheme_before_any_network() {
        let error = Webfetch
            .call(WebfetchArgs {
                url: "ftp://example.com".into(),
                format: None,
                timeout: None,
            })
            .await
            .expect_err("bad scheme");
        assert!(error.to_string().contains("http://"));
    }
}
