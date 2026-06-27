use std::sync::Arc;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::ScreenshotParams;
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
        let url = self.url.trim();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!(
                "invalid url: {url} (only http and https are supported)"
            ));
        }

        let width = self.width.unwrap_or(DEFAULT_VIEWPORT_WIDTH).max(320);
        let height = self.height.unwrap_or(DEFAULT_VIEWPORT_HEIGHT).max(240);
        let wait = Duration::from_millis(self.wait_ms.unwrap_or(DEFAULT_WAIT_MS));
        let full_page = self.full_page.unwrap_or(true);

        let browser = browser().await?;

        Self::capture(browser, url, width, height, wait, full_page).await
    }

    async fn capture(
        browser: &Browser,
        url: &str,
        width: u32,
        height: u32,
        wait: Duration,
        full_page: bool,
    ) -> Result<ToolOutput, String> {
        let page = browser
            .new_page(url)
            .await
            .map_err(|e| format!("failed to open page: {e}"))?;

        let capture = async {
            page.execute(
                chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(
                    width as i64,
                    height as i64,
                    1.0,
                    false,
                ),
            )
            .await
            .map_err(|e| format!("failed to set viewport: {e}"))?;

            tokio::time::sleep(wait).await;

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
        };

        let outcome = tokio::time::timeout(CAPTURE_TIMEOUT, capture).await;
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
                    "chromiumoxide handler exited; browser_screenshot unavailable until restart"
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

super::impl_tool!(
    BrowserScreenshot,
    audience = super::ToolAudience::MAIN,
    tier = super::ToolTier::Extended
);

impl super::ToolInvocation for BrowserScreenshot {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(BrowserScreenshot::start_header(
            self,
        )))
    }
    fn permission_scopes(&self) -> super::BoxFuture<'_, Option<super::PermissionScopes>> {
        let scope = format!("url:{}", self.url);
        Box::pin(std::future::ready(Some(super::PermissionScopes::single(
            scope,
        ))))
    }
    fn execute<'a>(self: Box<Self>, _ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { BrowserScreenshot::execute(&self).await.into() })
    }
}
