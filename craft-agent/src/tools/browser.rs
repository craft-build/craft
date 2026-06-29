use std::sync::Arc;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt;
use tokio::sync::OnceCell;
use tokio::time::Duration;

use crate::ToolOutput;
use craft_providers::{ImageMediaType, ImageSource};
use craft_tool_macro::Tool;
use serde::Deserialize;

const DEFAULT_WAIT_MS: u64 = 1500;
const DEFAULT_VIEWPORT_WIDTH: u32 = 1280;
const DEFAULT_VIEWPORT_HEIGHT: u32 = 720;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PNG_BYTES: usize = 8 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const TRUNCATION_MARKER: &str = "\n[truncated …]";
const NON_HTTP_MSG: &str = "invalid url: {url} (only http and https are supported)";

static BROWSER: OnceCell<(Browser,)> = OnceCell::const_new();

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct BrowserScreenshot {
    #[param(description = "Absolute http(s) URL of the page to render")]
    url: String,
    #[param(description = "Viewport width in pixels (default 1280)")]
    width: Option<u32>,
    #[param(description = "Viewport height in pixels (default 720)")]
    height: Option<u32>,
    #[param(description = "Extra milliseconds to wait for SPA hydration (default 1500)")]
    wait_ms: Option<u64>,
    #[param(description = "Capture the full scrollable page (default true)")]
    full_page: Option<bool>,
}

impl BrowserScreenshot {
    pub const NAME: &str = "browser_screenshot";
    pub const DESCRIPTION: &str = include_str!("browser.md");
    pub const EXAMPLES: Option<&str> = Some(r#"[{"url": "https://example.com"}]"#);

    pub fn start_header(&self) -> String {
        format!("screenshot {}", self.url)
    }

    pub async fn execute(&self) -> Result<ToolOutput, String> {
        let url = validate_url(&self.url)?;
        let width = self.width.unwrap_or(DEFAULT_VIEWPORT_WIDTH).max(320);
        let height = self.height.unwrap_or(DEFAULT_VIEWPORT_HEIGHT).max(240);
        let wait = Duration::from_millis(self.wait_ms.unwrap_or(DEFAULT_WAIT_MS));
        let full_page = self.full_page.unwrap_or(true);

        let browser = browser().await?;
        let page = open_page(browser, url, width, height, wait).await?;
        let outcome = tokio::time::timeout(CAPTURE_TIMEOUT, async {
            let png = page
                .screenshot(
                    ScreenshotParams::builder()
                        .full_page(full_page)
                        .from_surface(true)
                        .build(),
                )
                .await
                .map_err(|e| format!("screenshot failed: {e}"))?;
            if png.len() > MAX_PNG_BYTES {
                return Err(format!(
                    "screenshot too large ({} bytes, max {}); narrow the viewport or disable full_page",
                    png.len(),
                    MAX_PNG_BYTES
                ));
            }
            Ok(png)
        })
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(5), page.close()).await;
        let png = outcome.map_err(|_| "screenshot timed out")??;

        let data = base64_encode(&png);
        let caption = if full_page {
            format!("[full-page screenshot of {url}, viewport {width}x{height}]")
        } else {
            format!("[screenshot of {url}, {width}x{height}]")
        };

        Ok(ToolOutput::Image {
            caption,
            source: ImageSource::new(ImageMediaType::Png, Arc::from(data)),
        })
    }
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct BrowserNavigate {
    #[param(description = "Absolute http(s) URL to navigate to")]
    url: String,
    #[param(description = "Viewport width in pixels (default 1280)")]
    width: Option<u32>,
    #[param(description = "Viewport height in pixels (default 720)")]
    height: Option<u32>,
    #[param(description = "Extra milliseconds to wait for SPA hydration (default 1500)")]
    wait_ms: Option<u64>,
}

impl BrowserNavigate {
    pub const NAME: &str = "browser_navigate";
    pub const DESCRIPTION: &str = include_str!("browser_navigate.md");
    pub const EXAMPLES: Option<&str> = Some(r#"[{"url": "https://example.com"}]"#);

    pub fn start_header(&self) -> String {
        format!("navigate {}", self.url)
    }

    pub async fn execute(&self) -> Result<ToolOutput, String> {
        let url = validate_url(&self.url)?;
        let width = self.width.unwrap_or(DEFAULT_VIEWPORT_WIDTH).max(320);
        let height = self.height.unwrap_or(DEFAULT_VIEWPORT_HEIGHT).max(240);
        let wait = Duration::from_millis(self.wait_ms.unwrap_or(DEFAULT_WAIT_MS));

        let browser = browser().await?;
        let page = open_page(browser, url, width, height, wait).await?;
        let result = tokio::time::timeout(CAPTURE_TIMEOUT, page_summary(&page)).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), page.close()).await;
        let summary = result.map_err(|_| "navigation timed out")??;

        Ok(ToolOutput::Markdown(summary))
    }
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct BrowserClick {
    #[param(description = "Absolute http(s) URL to navigate to before clicking")]
    url: String,
    #[param(description = "CSS selector of the element to click (first match)")]
    selector: String,
    #[param(
        description = "What to return after clicking: \"text\" or \"screenshot\" (default \"text\")"
    )]
    r#as: Option<String>,
    #[param(description = "Viewport width in pixels (default 1280)")]
    width: Option<u32>,
    #[param(description = "Viewport height in pixels (default 720)")]
    height: Option<u32>,
    #[param(description = "Extra milliseconds to wait after clicking (default 1500)")]
    wait_ms: Option<u64>,
    #[param(description = "Capture a full-page screenshot when as=\"screenshot\" (default true)")]
    full_page: Option<bool>,
}

impl BrowserClick {
    pub const NAME: &str = "browser_click";
    pub const DESCRIPTION: &str = include_str!("browser_click.md");
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"url": "https://example.com", "selector": "a.login"}]"#);

    pub fn start_header(&self) -> String {
        format!("click {} on {}", self.selector, self.url)
    }

    pub async fn execute(&self) -> Result<ToolOutput, String> {
        let url = validate_url(&self.url)?;
        let as_text = match self.r#as.as_deref().unwrap_or("text") {
            "text" => true,
            "screenshot" => false,
            other => {
                return Err(format!(
                    "invalid `as` value: {other} (expected \"text\" or \"screenshot\")"
                ));
            }
        };
        let width = self.width.unwrap_or(DEFAULT_VIEWPORT_WIDTH).max(320);
        let height = self.height.unwrap_or(DEFAULT_VIEWPORT_HEIGHT).max(240);
        let wait = Duration::from_millis(self.wait_ms.unwrap_or(DEFAULT_WAIT_MS));
        let full_page = self.full_page.unwrap_or(true);

        let browser = browser().await?;
        let page = open_page(browser, url, width, height, wait).await;
        let page = match page {
            Ok(p) => p,
            Err(e) => return Err(e),
        };

        let outcome = tokio::time::timeout(CAPTURE_TIMEOUT, async {
            let element = page
                .find_element(self.selector.as_str())
                .await
                .map_err(|e| {
                    format!(
                        "element not found for selector `{}`: {e} (use browser_screenshot to inspect the DOM)",
                        self.selector
                    )
                })?;
            element
                .click()
                .await
                .map_err(|e| format!("click failed: {e}"))?;
            tokio::time::sleep(wait).await;

            if as_text {
                let text = inner_text(&page, None).await?;
                Ok(ToolOutput::Markdown(text))
            } else {
                let png = page
                    .screenshot(
                        ScreenshotParams::builder()
                            .full_page(full_page)
                            .from_surface(true)
                            .build(),
                    )
                    .await
                    .map_err(|e| format!("screenshot failed: {e}"))?;
                if png.len() > MAX_PNG_BYTES {
                    return Err(format!(
                        "screenshot too large ({} bytes, max {}); narrow the viewport or disable full_page",
                        png.len(),
                        MAX_PNG_BYTES
                    ));
                }
                let data = base64_encode(&png);
                Ok(ToolOutput::Image {
                    caption: format!("[screenshot after clicking {url}]"),
                    source: ImageSource::new(ImageMediaType::Png, Arc::from(data)),
                })
            }
        })
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(5), page.close()).await;
        outcome.map_err(|_| "click timed out")?
    }
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct BrowserText {
    #[param(description = "Absolute http(s) URL of the page to read")]
    url: String,
    #[param(
        description = "Optional CSS selector to scope the extracted text (defaults to document.body)"
    )]
    selector: Option<String>,
    #[param(description = "Viewport width in pixels (default 1280)")]
    width: Option<u32>,
    #[param(description = "Viewport height in pixels (default 720)")]
    height: Option<u32>,
    #[param(description = "Extra milliseconds to wait for SPA hydration (default 1500)")]
    wait_ms: Option<u64>,
}

impl BrowserText {
    pub const NAME: &str = "browser_text";
    pub const DESCRIPTION: &str = include_str!("browser_text.md");
    pub const EXAMPLES: Option<&str> = Some(r#"[{"url": "https://example.com"}]"#);

    pub fn start_header(&self) -> String {
        format!("text {}", self.url)
    }

    pub async fn execute(&self) -> Result<ToolOutput, String> {
        let url = validate_url(&self.url)?;
        let width = self.width.unwrap_or(DEFAULT_VIEWPORT_WIDTH).max(320);
        let height = self.height.unwrap_or(DEFAULT_VIEWPORT_HEIGHT).max(240);
        let wait = Duration::from_millis(self.wait_ms.unwrap_or(DEFAULT_WAIT_MS));

        let browser = browser().await?;
        let page = open_page(browser, url, width, height, wait).await?;
        let selector = self.selector.as_deref();
        let result = tokio::time::timeout(CAPTURE_TIMEOUT, inner_text(&page, selector)).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), page.close()).await;
        let text = result.map_err(|_| "text extraction timed out")??;
        Ok(ToolOutput::Markdown(text))
    }
}

fn validate_url(raw: &str) -> Result<&str, String> {
    let url = raw.trim();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(NON_HTTP_MSG.replace("{url}", url));
    }
    Ok(url)
}

async fn open_page(
    browser: &Browser,
    url: &str,
    width: u32,
    height: u32,
    wait: Duration,
) -> Result<Page, String> {
    let page = browser
        .new_page(url)
        .await
        .map_err(|e| format!("failed to open page: {e}"))?;
    page.execute(SetDeviceMetricsOverrideParams::new(
        width as i64,
        height as i64,
        1.0,
        false,
    ))
    .await
    .map_err(|e| format!("failed to set viewport: {e}"))?;
    tokio::time::sleep(wait).await;
    Ok(page)
}

async fn page_summary(page: &Page) -> Result<String, String> {
    let final_url = page.url().await.map_err(|e| format!("{e}"))?;
    let title = page.get_title().await.map_err(|e| format!("{e}"))?;
    let body = inner_text(page, None).await?;
    let final_url = final_url.unwrap_or_default();
    let title = title.unwrap_or_default();
    Ok(format!("# {title}\n\nurl: {final_url}\n\n{body}"))
}

async fn inner_text(page: &Page, selector: Option<&str>) -> Result<String, String> {
    let raw = match selector {
        Some(sel) => page
            .find_element(sel)
            .await
            .map_err(|e| format!("element not found for selector `{sel}`: {e}"))?
            .inner_text()
            .await
            .map_err(|e| format!("failed to read element text: {e}"))?
            .unwrap_or_default(),
        None => page
            .evaluate("document.body ? (document.body.innerText || '') : ''")
            .await
            .map_err(|e| format!("failed to read page text: {e}"))?
            .into_value::<String>()
            .map_err(|e| format!("unexpected page text result: {e}"))?,
    };
    Ok(cap_text(raw.trim()))
}

fn cap_text(text: &str) -> String {
    if text.len() <= MAX_TEXT_BYTES {
        return text.to_string();
    }
    let mut cut = MAX_TEXT_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + TRUNCATION_MARKER.len());
    out.push_str(&text[..cut]);
    out.push_str(TRUNCATION_MARKER);
    out
}

async fn browser() -> Result<&'static Browser, String> {
    let (browser,) = BROWSER
        .get_or_try_init(|| async {
            let mut builder = BrowserConfig::builder()
                .window_size(DEFAULT_VIEWPORT_WIDTH, DEFAULT_VIEWPORT_HEIGHT)
                .new_headless_mode()
                .arg("--disable-gpu")
                .arg("--disable-dev-shm-usage")
                .launch_timeout(Duration::from_secs(60));
            // The setuid sandbox can't run as root (containers), so relax it only then.
            if running_as_root() {
                builder = builder.arg("--no-sandbox");
            }
            let config = builder
                .build()
                .map_err(|e| format!("invalid browser config: {e}"))?;
            let (browser, mut handler) = Browser::launch(config)
                .await
                .map_err(|e| format!("failed to launch browser: {e}"))?;
            tokio::spawn(async move {
                while handler.next().await.is_some() {}
                tracing::warn!(
                    "chromiumoxide handler exited; browser tools unavailable until restart"
                );
            });
            Ok::<(Browser,), String>((browser,))
        })
        .await?;
    Ok(browser)
}

#[cfg(unix)]
fn running_as_root() -> bool {
    let uid = unsafe { libc::getuid() };
    uid == 0
}

#[cfg(not(unix))]
fn running_as_root() -> bool {
    false
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

macro_rules! impl_browser_tool {
    ($ty:ty, $header:expr) => {
        super::impl_tool!(
            $ty,
            audience = super::ToolAudience::MAIN,
            tier = super::ToolTier::Extended
        );

        impl super::ToolInvocation for $ty {
            fn start_header(&self) -> super::HeaderFuture {
                super::HeaderFuture::Ready(super::HeaderResult::plain($header(self)))
            }
            fn permission_scopes(&self) -> super::BoxFuture<'_, Option<super::PermissionScopes>> {
                let scope = format!("url:{}", self.url);
                Box::pin(std::future::ready(Some(super::PermissionScopes::single(
                    scope,
                ))))
            }
            fn execute<'a>(self: Box<Self>, _ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
                Box::pin(async move { <$ty>::execute(&self).await.into() })
            }
        }
    };
}

impl_browser_tool!(BrowserScreenshot, BrowserScreenshot::start_header);
impl_browser_tool!(BrowserNavigate, BrowserNavigate::start_header);
impl_browser_tool!(BrowserClick, BrowserClick::start_header);
impl_browser_tool!(BrowserText, BrowserText::start_header);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_url_rejects_non_http() {
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("example.com").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("").is_err());
    }

    #[test]
    fn validate_url_accepts_http_s() {
        assert_eq!(
            validate_url("http://example.com").unwrap(),
            "http://example.com"
        );
        assert_eq!(
            validate_url("https://example.com/path?q=1").unwrap(),
            "https://example.com/path?q=1"
        );
    }

    #[test]
    fn validate_url_trims_whitespace() {
        assert_eq!(
            validate_url("  https://example.com  ").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn cap_text_keeps_small_strings_unchanged() {
        assert_eq!(cap_text("hello"), "hello");
        assert_eq!(cap_text(""), "");
    }

    #[test]
    fn cap_text_truncates_oversized_with_marker() {
        let big = "x".repeat(MAX_TEXT_BYTES + 100);
        let out = cap_text(&big);
        assert!(out.ends_with(TRUNCATION_MARKER));
        let without_marker = &out[..out.len() - TRUNCATION_MARKER.len()];
        assert_eq!(without_marker.len(), MAX_TEXT_BYTES);
    }

    #[test]
    fn cap_text_truncates_at_char_boundary() {
        let mut big = String::from("é").repeat(MAX_TEXT_BYTES);
        big.push_str("tail");
        let out = cap_text(&big);
        assert!(out.ends_with(TRUNCATION_MARKER));
        assert!(out.is_char_boundary(out.len() - TRUNCATION_MARKER.len()));
    }

    #[test]
    fn start_header_formats_for_each_tool() {
        let s = BrowserScreenshot {
            url: "https://a.test".into(),
            width: None,
            height: None,
            wait_ms: None,
            full_page: None,
        };
        assert_eq!(s.start_header(), "screenshot https://a.test");

        let n = BrowserNavigate {
            url: "https://b.test".into(),
            width: None,
            height: None,
            wait_ms: None,
        };
        assert_eq!(n.start_header(), "navigate https://b.test");

        let c = BrowserClick {
            url: "https://c.test".into(),
            selector: "a.go".into(),
            r#as: None,
            width: None,
            height: None,
            wait_ms: None,
            full_page: None,
        };
        assert_eq!(c.start_header(), "click a.go on https://c.test");

        let t = BrowserText {
            url: "https://d.test".into(),
            selector: None,
            width: None,
            height: None,
            wait_ms: None,
        };
        assert_eq!(t.start_header(), "text https://d.test");
    }
}
