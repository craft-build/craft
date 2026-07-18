use std::sync::Arc;

use base64::Engine;
use craft_providers::{ImageMediaType, ImageSource};
use craft_tool_macro::{Args, Tool};
use playwright_rs::{
    Browser as PwBrowser, BrowserContext, Error as PwError, LaunchOptions, Page, Playwright,
    ScreenshotClip, ScreenshotOptions, Viewport, WaitForOptions, WaitForState,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Mutex, OnceCell};
use tokio::time::Duration;

use crate::ToolOutput;

const DEFAULT_WAIT_MS: u64 = 1500;
const DEFAULT_VIEWPORT_WIDTH: u32 = 1280;
const DEFAULT_VIEWPORT_HEIGHT: u32 = 720;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PNG_BYTES: usize = 8 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_EVAL_BYTES: usize = 256 * 1024;
const TRUNCATION_MARKER: &str = "\n[truncated …]";
const NON_HTTP_MSG: &str = "invalid url: {url} (only http and https are supported)";
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_SCROLL_AMOUNT: i64 = 500;
const NOT_LAUNCHED_MSG: &str =
    "browser is not launched. Run `browser status` after installing Playwright.";

const VALID_ACTIONS: &[&str] = &[
    "status",
    "list_tabs",
    "new_tab",
    "select_tab",
    "get_active_tab",
    "close_tab",
    "open",
    "screenshot",
    "get_content",
    "interactables",
    "click",
    "type",
    "fill_form",
    "press",
    "eval",
    "scroll",
    "wait",
    "select",
];

const ACTION_PRE_NAVIGATES: &[&str] = &[
    "open",
    "screenshot",
    "get_content",
    "interactables",
    "click",
    "type",
    "fill_form",
    "press",
    "eval",
    "scroll",
    "wait",
    "select",
];

static SESSION: OnceCell<Arc<Mutex<BrowserSession>>> = OnceCell::const_new();

struct BrowserSession {
    playwright: Option<Playwright>,
    browser: Option<PwBrowser>,
    contexts: Vec<BrowserContext>,
    active: Option<usize>,
}

struct TabInfo {
    index: usize,
    url: String,
    title: Option<String>,
}

impl BrowserSession {
    fn new() -> Self {
        Self {
            playwright: None,
            browser: None,
            contexts: Vec::new(),
            active: None,
        }
    }

    async fn launch(&mut self) -> Result<(), String> {
        let pw = Playwright::launch()
            .await
            .map_err(|e| format!("failed to start playwright driver: {e}"))?;
        let mut opts = LaunchOptions::new().headless(true).args(vec![
            "--disable-gpu".into(),
            "--disable-dev-shm-usage".into(),
        ]);
        if running_as_root() {
            opts = opts.args(vec!["--no-sandbox".into()]);
        }
        let browser = pw
            .chromium()
            .launch_with_options(opts)
            .await
            .map_err(|e| format!("failed to launch browser: {e}"))?;
        self.playwright = Some(pw);
        self.browser = Some(browser);
        Ok(())
    }

    fn is_launched(&self) -> bool {
        self.browser.is_some()
    }

    fn page(&self, tab: Option<usize>) -> Result<Page, String> {
        let active = self.active.ok_or_else(|| {
            "no active tab. Open a URL with `browser open` or create one with `browser new_tab`."
                .to_string()
        })?;
        let idx = tab.unwrap_or(active);
        let ctx = self
            .contexts
            .get(idx)
            .ok_or_else(|| format!("tab index {idx} does not exist"))?;
        ctx.pages()
            .into_iter()
            .next()
            .ok_or_else(|| format!("tab {idx} has no open page"))
    }

    async fn new_tab(
        &mut self,
        url: Option<&str>,
        width: u32,
        height: u32,
    ) -> Result<usize, String> {
        let browser = self
            .browser
            .as_ref()
            .ok_or_else(|| NOT_LAUNCHED_MSG.to_string())?;
        let ctx = browser
            .new_context()
            .await
            .map_err(|e| format!("failed to create browser context: {e}"))?;
        let page = ctx
            .new_page()
            .await
            .map_err(|e| format!("failed to open page: {e}"))?;
        page.set_viewport_size(Viewport { width, height })
            .await
            .map_err(|e| format!("failed to set viewport: {e}"))?;
        if let Some(u) = url {
            let u = validate_url(u)?;
            page.goto(u, None)
                .await
                .map_err(|e| format!("navigation failed: {e}"))?;
            tokio::time::sleep(Duration::from_millis(DEFAULT_WAIT_MS)).await;
        }
        self.contexts.push(ctx);
        let idx = self.contexts.len() - 1;
        self.active = Some(idx);
        Ok(idx)
    }

    async fn select_tab(&mut self, index: usize) -> Result<(), String> {
        if index >= self.contexts.len() {
            return Err(format!("tab index {index} does not exist"));
        }
        self.active = Some(index);
        Ok(())
    }

    async fn list_tabs(&self) -> Result<Vec<TabInfo>, String> {
        let mut out = Vec::with_capacity(self.contexts.len());
        for (i, ctx) in self.contexts.iter().enumerate() {
            let pages = ctx.pages();
            let (url, title) = match pages.into_iter().next() {
                Some(p) => {
                    let url = p.url();
                    let title = p.title().await.ok();
                    (url, title)
                }
                None => ("about:blank".to_string(), None),
            };
            out.push(TabInfo {
                index: i,
                url,
                title,
            });
        }
        Ok(out)
    }

    async fn close_tab(&mut self, index: usize) -> Result<(), String> {
        let ctx = self
            .contexts
            .get(index)
            .ok_or_else(|| format!("tab index {index} does not exist"))?
            .clone();
        ctx.close()
            .await
            .map_err(|e| format!("failed to close tab: {e}"))?;
        self.contexts.remove(index);
        if self.contexts.is_empty() {
            self.active = None;
        } else if self.active == Some(index) {
            self.active = Some(index.saturating_sub(1));
        } else if let Some(a) = self.active
            && a > index
        {
            self.active = Some(a - 1);
        }
        Ok(())
    }
}

async fn session() -> Result<&'static Arc<Mutex<BrowserSession>>, String> {
    SESSION
        .get_or_try_init(|| async {
            let mut s = BrowserSession::new();
            s.launch().await?;
            Ok::<_, String>(Arc::new(Mutex::new(s)))
        })
        .await
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Browser {
    #[param(description = "Browser action to perform")]
    action: String,
    #[param(
        description = "Absolute http(s) URL to navigate to. Required for 'open', optional for most others (navigates first if provided)."
    )]
    url: Option<String>,
    #[param(
        description = "CSS selector of the element to interact with. Used by click, type, select, scroll, screenshot, wait."
    )]
    selector: Option<String>,
    #[param(description = "Text to type into the element (for 'type' action)")]
    text: Option<String>,
    #[param(
        description = "Key chord to press, e.g. 'Enter', 'Tab', 'Escape', 'ctrl+shift+t' (for 'press' action)"
    )]
    key: Option<String>,
    #[param(
        description = "Form fields to fill (for 'fill_form' action). Array of {selector, value, type?, checked?} objects."
    )]
    fields: Option<Vec<FormField>>,
    #[param(description = "JavaScript to execute in the page context (for 'eval' action)")]
    script: Option<String>,
    #[param(
        description = "Content format for 'get_content': 'text', 'html', 'markdown', or 'title' (default: 'text')"
    )]
    format: Option<String>,
    #[param(
        description = "Filter for 'interactables': 'all', 'links', 'inputs', 'buttons' (default: 'all')"
    )]
    filter: Option<String>,
    #[param(description = "Tab index to operate on (default: active tab)")]
    tab: Option<usize>,
    #[param(description = "Viewport width in pixels (default 1280)")]
    width: Option<u32>,
    #[param(description = "Viewport height in pixels (default 720)")]
    height: Option<u32>,
    #[param(description = "Extra milliseconds to wait after navigation (default 1500)")]
    wait_ms: Option<u64>,
    #[param(description = "Full-page screenshot (default true, for 'screenshot' action)")]
    full_page: Option<bool>,
    #[param(
        description = "Screenshot region [x, y, width, height] in CSS pixels (for 'screenshot' action)"
    )]
    region: Option<Vec<u32>>,
    #[param(
        description = "Scroll direction 'up' or 'down' (for 'scroll' action, default: 'down')"
    )]
    direction: Option<String>,
    #[param(description = "Scroll amount in pixels (for 'scroll' action, default: 500)")]
    amount: Option<i64>,
    #[param(description = "Scroll to top (true) or bottom (false) of page (for 'scroll' action)")]
    to_top: Option<bool>,
    #[param(description = "Timeout in milliseconds for 'wait' action (default: 10000)")]
    timeout_ms: Option<u64>,
    #[param(
        description = "Wait for element to be visible, not just present (for 'wait' action, default: true)"
    )]
    visible: Option<bool>,
    #[param(description = "Value to select in a dropdown (for 'select' action)")]
    value: Option<String>,
    #[param(description = "Clear the field before typing (for 'type' action, default: true)")]
    clear: Option<bool>,
    #[param(description = "Press Enter after typing (for 'type' action, default: false)")]
    submit: Option<bool>,
    #[param(description = "Submit button CSS selector (for 'fill_form' when submit=true)")]
    submit_selector: Option<String>,
}

#[derive(Args, Debug, Clone, Deserialize)]
pub struct FormField {
    #[param(description = "CSS selector of the form field")]
    selector: String,
    #[param(description = "Value to set (for text/select) or target state")]
    value: String,
    #[param(description = "Field type: 'text', 'select', 'checkbox', or 'radio' (default: text)")]
    #[serde(rename = "type")]
    field_type: Option<String>,
    #[param(description = "For checkbox/radio: the target checked state")]
    checked: Option<bool>,
}

impl Browser {
    pub const NAME: &str = "browser";
    pub const DESCRIPTION: &str = include_str!("browser.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
        {"action": "screenshot", "url": "https://example.com"},
        {"action": "click", "url": "https://example.com", "selector": "a.login"},
        {"action": "get_content", "url": "https://example.com", "format": "text"},
        {"action": "fill_form", "url": "https://example.com", "fields": [{"selector": "email", "value": "a@b.com"}, {"selector": "password", "value": "secret"}], "submit": true, "submit_selector": "login-btn"},
        {"action": "eval", "url": "https://example.com", "script": "document.title"}
    ]"#,
    );

    pub fn start_header(&self) -> String {
        match self.action.as_str() {
            "screenshot" => {
                let url = self.url.as_deref().unwrap_or("current page");
                format!("browser screenshot {url}")
            }
            "open" | "navigate" => {
                format!("browser open {}", self.url.as_deref().unwrap_or("?"))
            }
            "click" => {
                let sel = self.selector.as_deref().unwrap_or("?");
                let url = self
                    .url
                    .as_deref()
                    .map(|u| format!(" on {u}"))
                    .unwrap_or_default();
                format!("browser click {sel}{url}")
            }
            "type" => {
                let sel = self.selector.as_deref().unwrap_or("?");
                let text = self.text.as_deref().unwrap_or("");
                let preview: String = text.chars().take(30).collect();
                format!(
                    "browser type {sel} \"{preview}{}\"",
                    if text.chars().count() > 30 { "…" } else { "" }
                )
            }
            "fill_form" => {
                let count = self.fields.as_ref().map(|f| f.len()).unwrap_or(0);
                format!("browser fill_form ({count} fields)")
            }
            "press" => {
                format!("browser press {}", self.key.as_deref().unwrap_or("?"))
            }
            "eval" => {
                let script = self.script.as_deref().unwrap_or("");
                let preview: String = script.chars().take(40).collect();
                format!(
                    "browser eval \"{preview}{}\"",
                    if script.chars().count() > 40 {
                        "…"
                    } else {
                        ""
                    }
                )
            }
            "scroll" => {
                if self.to_top == Some(true) {
                    "browser scroll to top".into()
                } else if self.to_top == Some(false) {
                    "browser scroll to bottom".into()
                } else if let Some(sel) = &self.selector {
                    format!("browser scroll to {sel}")
                } else {
                    let dir = self.direction.as_deref().unwrap_or("down");
                    let amt = self.amount.unwrap_or(DEFAULT_SCROLL_AMOUNT);
                    format!("browser scroll {dir} {amt}px")
                }
            }
            "wait" => {
                let sel = self.selector.as_deref().unwrap_or("page load");
                format!("browser wait for {sel}")
            }
            "interactables" => {
                let filter = self.filter.as_deref().unwrap_or("all");
                format!("browser interactables ({filter})")
            }
            "get_content" => {
                let fmt = self.format.as_deref().unwrap_or("text");
                format!("browser get_content ({fmt})")
            }
            "list_tabs" => "browser list_tabs".into(),
            "new_tab" => {
                let url = self.url.as_deref().unwrap_or("about:blank");
                format!("browser new_tab {url}")
            }
            "select_tab" => format!("browser select_tab {}", self.tab.unwrap_or(0)),
            "close_tab" => format!("browser close_tab {}", self.tab.unwrap_or(0)),
            "get_active_tab" => "browser get_active_tab".into(),
            "status" => "browser status".into(),
            "select" => {
                let sel = self.selector.as_deref().unwrap_or("?");
                let val = self.value.as_deref().unwrap_or("?");
                format!("browser select {val} in {sel}")
            }
            other => format!("browser {other}"),
        }
    }

    pub async fn run(&self) -> Result<ToolOutput, String> {
        if !VALID_ACTIONS.contains(&self.action.as_str()) {
            let valid = VALID_ACTIONS.join(", ");
            return Err(format!(
                "invalid browser action '{}'. Valid actions: {valid}",
                self.action
            ));
        }

        let handle = match self.action.as_str() {
            "status" => return Self::action_status().await,
            _ => session().await?,
        };

        let width = self.width.unwrap_or(DEFAULT_VIEWPORT_WIDTH).max(320);
        let height = self.height.unwrap_or(DEFAULT_VIEWPORT_HEIGHT).max(240);
        let wait = Duration::from_millis(self.wait_ms.unwrap_or(DEFAULT_WAIT_MS));

        let navigated_url = if ACTION_PRE_NAVIGATES.contains(&self.action.as_str()) {
            if let Some(url) = &self.url {
                let url = validate_url(url)?;
                let mut s = handle.lock().await;
                ensure_active_tab(&mut s, url, width, height, wait).await?;
                true
            } else {
                false
            }
        } else {
            false
        };

        match self.action.as_str() {
            "status" => unreachable!(),
            "list_tabs" => self.action_list_tabs(handle).await,
            "new_tab" => self.action_new_tab(handle, width, height).await,
            "select_tab" => self.action_select_tab(handle).await,
            "get_active_tab" => self.action_get_active_tab(handle).await,
            "close_tab" => self.action_close_tab(handle).await,
            "open" => self.action_open(handle, wait, navigated_url).await,
            "screenshot" => self.action_screenshot(handle, width, height).await,
            "get_content" => self.action_get_content(handle).await,
            "interactables" => self.action_interactables(handle).await,
            "click" => self.action_click(handle, wait).await,
            "type" => self.action_type(handle).await,
            "fill_form" => self.action_fill_form(handle).await,
            "press" => self.action_press(handle).await,
            "eval" => self.action_eval(handle).await,
            "scroll" => self.action_scroll(handle).await,
            "wait" => self.action_wait(handle).await,
            "select" => self.action_select(handle).await,
            _ => unreachable!("validated above"),
        }
    }

    async fn action_status() -> Result<ToolOutput, String> {
        let launched = SESSION.get().is_some();
        let status = if launched { "ready" } else { "not launched" };
        let tabs = if let Some(h) = SESSION.get() {
            let s = h.lock().await;
            s.contexts.len()
        } else {
            0
        };
        Ok(ToolOutput::Markdown(format!(
            "Browser: {status}\nTabs: {tabs}\n\nIf the browser is not launched, ensure the \
             Playwright driver is installed (`npm install -g playwright@1.60.0` or set \
             `PLAYWRIGHT_DRIVER_PATH`)."
        )))
    }

    async fn action_list_tabs(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
    ) -> Result<ToolOutput, String> {
        let s = handle.lock().await;
        let tabs = s.list_tabs().await?;
        if tabs.is_empty() {
            return Ok(ToolOutput::Markdown("No open tabs.".into()));
        }
        let mut out = String::from("| Tab | URL | Title |\n|-----|-----|-------|\n");
        let active = s.active;
        for t in tabs {
            let mark = if active == Some(t.index) { " *" } else { "" };
            out.push_str(&format!(
                "| {}{} | {} | {} |\n",
                t.index,
                mark,
                t.url,
                t.title.unwrap_or_default()
            ));
        }
        Ok(ToolOutput::Markdown(out))
    }

    async fn action_new_tab(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
        width: u32,
        height: u32,
    ) -> Result<ToolOutput, String> {
        let mut s = handle.lock().await;
        if !s.is_launched() {
            s.launch().await?;
        }
        let url = self.url.as_deref().and_then(|u| validate_url(u).ok());
        let idx = s.new_tab(url, width, height).await?;
        Ok(ToolOutput::Markdown(format!("Opened tab {idx}.")))
    }

    async fn action_select_tab(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
    ) -> Result<ToolOutput, String> {
        let mut s = handle.lock().await;
        let idx = self
            .tab
            .ok_or_else(|| "select_tab requires 'tab'".to_string())?;
        s.select_tab(idx).await?;
        Ok(ToolOutput::Markdown(format!("Switched to tab {idx}.")))
    }

    async fn action_get_active_tab(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
    ) -> Result<ToolOutput, String> {
        let s = handle.lock().await;
        let idx = s.active.ok_or_else(|| "no active tab".to_string())?;
        let tabs = s.list_tabs().await?;
        let t = tabs
            .into_iter()
            .find(|t| t.index == idx)
            .ok_or_else(|| "active tab vanished".to_string())?;
        Ok(ToolOutput::Markdown(format!(
            "Active tab: {idx}\nURL: {}\nTitle: {}",
            t.url,
            t.title.unwrap_or_default()
        )))
    }

    async fn action_close_tab(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
    ) -> Result<ToolOutput, String> {
        let mut s = handle.lock().await;
        let idx = self
            .tab
            .ok_or_else(|| "close_tab requires 'tab'".to_string())?;
        s.close_tab(idx).await?;
        Ok(ToolOutput::Markdown(format!("Closed tab {idx}.")))
    }

    async fn action_open(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
        wait: Duration,
        navigated_url: bool,
    ) -> Result<ToolOutput, String> {
        let s = handle.lock().await;
        let page = s.page(None)?;
        drop(s);
        let summary = tokio::time::timeout(CAPTURE_TIMEOUT, async {
            if !navigated_url && !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            page_summary(&page).await
        })
        .await
        .map_err(|_| "navigation timed out")??;
        Ok(ToolOutput::Markdown(summary))
    }

    async fn action_screenshot(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
        width: u32,
        height: u32,
    ) -> Result<ToolOutput, String> {
        let s = handle.lock().await;
        if let Some(sel) = &self.selector {
            let page = s.page(self.tab)?;
            drop(s);
            return capture_element(&page, sel).await;
        }
        let page = s.page(self.tab)?;
        page.set_viewport_size(Viewport { width, height })
            .await
            .map_err(|e| format!("failed to set viewport: {e}"))?;
        drop(s);
        let full_page = self.full_page.unwrap_or(true);
        if let Some(r) = self.region.as_deref()
            && r.len() != 4
        {
            return Err("region must have exactly 4 elements [x, y, width, height]".to_string());
        }
        let png = tokio::time::timeout(CAPTURE_TIMEOUT, async {
            let opts = build_screenshot_opts(full_page, self.region.as_deref());
            page.screenshot(Some(opts)).await
        })
        .await
        .map_err(|_| "screenshot timed out")?
        .map_err(|e| format!("screenshot failed: {e}"))?;
        check_png_size(&png)?;
        let url = page.url();
        let caption = if full_page {
            format!("[full-page screenshot of {url}, viewport {width}x{height}]")
        } else {
            format!("[screenshot of {url}, {width}x{height}]")
        };
        Ok(image_output(&png, caption))
    }

    async fn action_get_content(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
    ) -> Result<ToolOutput, String> {
        let s = handle.lock().await;
        let page = s.page(self.tab)?;
        drop(s);
        let format = self.format.as_deref().unwrap_or("text");
        let selector = self.selector.as_deref();
        let body: String = tokio::time::timeout(CAPTURE_TIMEOUT, async {
            let result: Result<String, String> = match format {
                "title" => {
                    let t = page.title().await.map_err(|e| format!("{e}"))?;
                    Ok(t)
                }
                "html" => {
                    let html = if let Some(sel) = selector {
                        let el = page.locator(sel).await;
                        el.evaluate::<_, String>("el => el.outerHTML", None::<String>)
                            .await
                            .map_err(|e| format!("failed to read element html: {e}"))
                    } else {
                        page.content()
                            .await
                            .map_err(|e| format!("failed to read page html: {e}"))
                    }?;
                    Ok(cap_text(&html))
                }
                "markdown" => {
                    let html = if let Some(sel) = selector {
                        let el = page.locator(sel).await;
                        el.evaluate::<_, String>("el => el.outerHTML", None::<String>)
                            .await
                            .map_err(|e| format!("failed to read element html: {e}"))?
                    } else {
                        page.content()
                            .await
                            .map_err(|e| format!("failed to read page html: {e}"))?
                    };
                    let md = htmd::convert(&html).unwrap_or(html);
                    Ok(cap_text(&md))
                }
                _ => {
                    let text = read_inner_text(&page, selector).await?;
                    Ok(text)
                }
            };
            result
        })
        .await
        .map_err(|_| "content extraction timed out")??;
        Ok(ToolOutput::Markdown(body))
    }

    async fn action_interactables(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
    ) -> Result<ToolOutput, String> {
        let s = handle.lock().await;
        let page = s.page(self.tab)?;
        drop(s);
        let filter = self.filter.as_deref().unwrap_or("all");
        let filter_clause = match filter {
            "links" => "el.tagName === 'A'",
            "inputs" => "['INPUT','SELECT','TEXTAREA'].includes(el.tagName)",
            "buttons" => {
                "el.tagName === 'BUTTON' || (el.tagName === 'INPUT' && el.type === 'submit')"
            }
            _ => "true",
        };
        let script = format!(
            r#"() => {{
              const els = Array.from(document.querySelectorAll('a, button, input, select, textarea, [role="button"], [onclick]'));
              const out = [];
              for (const el of els) {{
                if (!({filter_clause})) continue;
                const r = el.getBoundingClientRect();
                const visible = r.width > 0 && r.height > 0;
                const id = el.id ? '#' + el.id : '';
                const name = el.name ? '[name="' + el.name + '"]' : '';
                let selector = id || name || el.tagName.toLowerCase();
                if (el.className && typeof el.className === 'string' && el.className.trim()) {{
                  const cls = el.className.trim().split(/\s+/).slice(0,2).map(c => '.' + c).join('');
                  if (!id) selector = selector + cls;
                }}
                out.push({{
                  index: out.length,
                  tag: el.tagName.toLowerCase(),
                  type: el.getAttribute('type') || null,
                  name: el.getAttribute('name') || null,
                  text: (el.innerText || el.textContent || '').trim().slice(0, 80),
                  href: el.getAttribute('href') || null,
                  placeholder: el.getAttribute('placeholder') || null,
                  selector: selector,
                  visible: visible,
                }});
              }}
              return out;
            }}"#
        );
        let elements: Vec<Value> = tokio::time::timeout(CAPTURE_TIMEOUT, async {
            page.evaluate::<(), Vec<Value>>(&script, None::<&()>)
                .await
                .map_err(|e| format!("interactables query failed: {e}"))
        })
        .await
        .map_err(|_| "interactables query timed out")??;
        let json = serde_json::to_string_pretty(&elements)
            .map_err(|e| format!("failed to serialize interactables: {e}"))?;
        Ok(ToolOutput::Markdown(cap_eval(&json)))
    }

    async fn action_click(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
        wait: Duration,
    ) -> Result<ToolOutput, String> {
        let sel = self
            .selector
            .as_deref()
            .ok_or_else(|| "click requires 'selector'".to_string())?;
        let s = handle.lock().await;
        let page = s.page(self.tab)?;
        drop(s);
        let text = tokio::time::timeout(CAPTURE_TIMEOUT, async {
            let locator = page.locator(sel).await;
            locator.click(None).await.map_err(|e| click_err(sel, e))?;
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            read_inner_text(&page, None).await
        })
        .await
        .map_err(|_| "click timed out")??;
        Ok(ToolOutput::Markdown(text))
    }

    async fn action_type(&self, handle: &Arc<Mutex<BrowserSession>>) -> Result<ToolOutput, String> {
        let sel = self
            .selector
            .as_deref()
            .ok_or_else(|| "type requires 'selector'".to_string())?;
        let text = self
            .text
            .as_deref()
            .ok_or_else(|| "type requires 'text'".to_string())?;
        let clear = self.clear.unwrap_or(true);
        let submit = self.submit.unwrap_or(false);
        let s = handle.lock().await;
        let page = s.page(self.tab)?;
        drop(s);
        tokio::time::timeout(CAPTURE_TIMEOUT, async {
            let locator = page.locator(sel).await;
            if clear {
                let _ = locator.clear(None).await;
            }
            locator
                .fill(text, None)
                .await
                .map_err(|e| format!("failed to type into `{sel}`: {e}"))?;
            if submit {
                locator
                    .press("Enter", None)
                    .await
                    .map_err(|e| format!("failed to submit: {e}"))?;
            }
            Result::<(), String>::Ok(())
        })
        .await
        .map_err(|_| "type timed out")??;
        let preview: String = text.chars().take(40).collect();
        Ok(ToolOutput::Markdown(format!(
            "Typed \"{preview}\" into `{sel}`{}.",
            if submit { " and pressed Enter" } else { "" }
        )))
    }

    async fn action_fill_form(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
    ) -> Result<ToolOutput, String> {
        let fields = self
            .fields
            .as_ref()
            .ok_or_else(|| "fill_form requires 'fields'".to_string())?;
        if fields.is_empty() {
            return Err("fill_form requires at least one field".into());
        }
        let submit = self.submit.unwrap_or(false);
        let submit_selector = self.submit_selector.as_deref();
        let s = handle.lock().await;
        let page = s.page(self.tab)?;
        drop(s);
        let mut filled = 0usize;
        tokio::time::timeout(CAPTURE_TIMEOUT, async {
            for f in fields {
                let locator = page.locator(&f.selector).await;
                match f.field_type.as_deref().unwrap_or("text") {
                    "select" => {
                        locator
                            .select_option(f.value.as_str(), None)
                            .await
                            .map_err(|e| {
                                format!("failed to select `{}` in `{}`: {e}", f.value, f.selector)
                            })?;
                    }
                    "checkbox" | "radio" => {
                        let checked = f.checked.unwrap_or(true);
                        locator
                            .set_checked(checked, None)
                            .await
                            .map_err(|e| format!("failed to set `{}`: {e}", f.selector))?;
                    }
                    _ => {
                        locator
                            .fill(&f.value, None)
                            .await
                            .map_err(|e| format!("failed to fill `{}`: {e}", f.selector))?;
                    }
                }
                filled += 1;
            }
            if submit {
                let sel = submit_selector.ok_or_else(|| {
                    "fill_form with submit=true requires 'submit_selector'".to_string()
                })?;
                page.locator(sel)
                    .await
                    .click(None)
                    .await
                    .map_err(|e| format!("failed to click submit `{sel}`: {e}"))?;
            }
            Result::<(), String>::Ok(())
        })
        .await
        .map_err(|_| "fill_form timed out")??;
        Ok(ToolOutput::Markdown(format!(
            "Filled {filled} field(s){}.",
            if submit {
                " and submitted the form"
            } else {
                ""
            }
        )))
    }

    async fn action_press(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
    ) -> Result<ToolOutput, String> {
        let key = self
            .key
            .as_deref()
            .ok_or_else(|| "press requires 'key'".to_string())?;
        let chord = parse_chord(key)?;
        let s = handle.lock().await;
        let page = s.page(self.tab)?;
        drop(s);
        page.keyboard()
            .press(&chord, None)
            .await
            .map_err(|e| format!("failed to press `{key}`: {e}"))?;
        Ok(ToolOutput::Markdown(format!("Pressed `{key}`.")))
    }

    async fn action_eval(&self, handle: &Arc<Mutex<BrowserSession>>) -> Result<ToolOutput, String> {
        let script = self
            .script
            .as_deref()
            .ok_or_else(|| "eval requires 'script'".to_string())?;
        let s = handle.lock().await;
        let page = s.page(self.tab)?;
        drop(s);
        let result: Value = tokio::time::timeout(CAPTURE_TIMEOUT, async {
            page.evaluate::<(), Value>(script, None::<&()>)
                .await
                .map_err(|e| format!("eval failed: {e}"))
        })
        .await
        .map_err(|_| "eval timed out")??;
        let rendered = match result {
            Value::Null => "null".to_string(),
            Value::String(s) => s,
            other => serde_json::to_string_pretty(&other)
                .map_err(|e| format!("failed to serialize eval result: {e}"))?,
        };
        Ok(ToolOutput::Markdown(cap_eval(&rendered)))
    }

    async fn action_scroll(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
    ) -> Result<ToolOutput, String> {
        let s = handle.lock().await;
        let page = s.page(self.tab)?;
        drop(s);
        if let Some(sel) = &self.selector {
            tokio::time::timeout(CAPTURE_TIMEOUT, async {
                page.locator(sel)
                    .await
                    .scroll_into_view_if_needed()
                    .await
                    .map_err(|e| format!("failed to scroll to `{sel}`: {e}"))
            })
            .await
            .map_err(|_| "scroll timed out")??;
            return Ok(ToolOutput::Markdown(format!("Scrolled `{sel}` into view.")));
        }
        if self.to_top == Some(true) {
            tokio::time::timeout(CAPTURE_TIMEOUT, async {
                page.evaluate::<(), ()>("() => window.scrollTo(0, 0)", None::<&()>)
                    .await
            })
            .await
            .map_err(|_| "scroll timed out")?
            .map_err(|e| format!("scroll failed: {e}"))?;
            return Ok(ToolOutput::Markdown("Scrolled to top.".into()));
        }
        if self.to_top == Some(false) {
            tokio::time::timeout(CAPTURE_TIMEOUT, async {
                page.evaluate::<(), ()>(
                    "() => window.scrollTo(0, document.body.scrollHeight)",
                    None::<&()>,
                )
                .await
            })
            .await
            .map_err(|_| "scroll timed out")?
            .map_err(|e| format!("scroll failed: {e}"))?;
            return Ok(ToolOutput::Markdown("Scrolled to bottom.".into()));
        }
        let dir = self.direction.as_deref().unwrap_or("down");
        let amt = self.amount.unwrap_or(DEFAULT_SCROLL_AMOUNT);
        let dy = if dir == "up" { -amt } else { amt };
        page.mouse()
            .wheel(0, dy as i32)
            .await
            .map_err(|e| format!("scroll failed: {e}"))?;
        Ok(ToolOutput::Markdown("Scrolled.".into()))
    }

    async fn action_wait(&self, handle: &Arc<Mutex<BrowserSession>>) -> Result<ToolOutput, String> {
        let timeout = self.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
        let visible = self.visible.unwrap_or(true);
        let s = handle.lock().await;
        let page = s.page(self.tab)?;
        drop(s);
        if let Some(sel) = &self.selector {
            let state = if visible {
                WaitForState::Visible
            } else {
                WaitForState::Attached
            };
            let opts = WaitForOptions::builder()
                .state(state)
                .timeout(timeout as f64)
                .build();
            tokio::time::timeout(CAPTURE_TIMEOUT, async {
                page.locator(sel).await.wait_for(Some(opts)).await
            })
            .await
            .map_err(|_| "wait timed out")?
            .map_err(|e| format!("element `{sel}` did not appear: {e}"))?;
            return Ok(ToolOutput::Markdown(format!("Element `{sel}` is present.")));
        }
        tokio::time::timeout(CAPTURE_TIMEOUT, page.wait_for_load_state(None))
            .await
            .map_err(|_| "wait for load timed out")?
            .map_err(|e| format!("wait for load failed: {e}"))?;
        Ok(ToolOutput::Markdown(
            "Page reached a stable load state.".into(),
        ))
    }

    async fn action_select(
        &self,
        handle: &Arc<Mutex<BrowserSession>>,
    ) -> Result<ToolOutput, String> {
        let sel = self
            .selector
            .as_deref()
            .ok_or_else(|| "select requires 'selector'".to_string())?;
        let value = self
            .value
            .as_deref()
            .ok_or_else(|| "select requires 'value'".to_string())?;
        let s = handle.lock().await;
        let page = s.page(self.tab)?;
        drop(s);
        page.locator(sel)
            .await
            .select_option(value, None)
            .await
            .map_err(|e| format!("failed to select `{value}` in `{sel}`: {e}"))?;
        Ok(ToolOutput::Markdown(format!(
            "Selected `{value}` in `{sel}`."
        )))
    }
}

async fn ensure_active_tab(
    s: &mut BrowserSession,
    url: &str,
    width: u32,
    height: u32,
    wait: Duration,
) -> Result<(), String> {
    if !s.is_launched() {
        s.launch().await?;
    }
    if s.active.is_none() || s.contexts.is_empty() {
        s.new_tab(Some(url), width, height).await?;
        return Ok(());
    }
    let page = s.page(None)?;
    page.goto(url, None)
        .await
        .map_err(|e| format!("navigation failed: {e}"))?;
    page.set_viewport_size(Viewport { width, height })
        .await
        .map_err(|e| format!("failed to set viewport: {e}"))?;
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    Ok(())
}

fn click_err(sel: &str, e: PwError) -> String {
    format!(
        "click failed for selector `{sel}`: {e} (use `browser interactables` or `browser screenshot` to inspect the DOM)"
    )
}

async fn capture_element(page: &Page, sel: &str) -> Result<ToolOutput, String> {
    let png = tokio::time::timeout(CAPTURE_TIMEOUT, async {
        page.locator(sel).await.screenshot(None).await
    })
    .await
    .map_err(|_| "element screenshot timed out")?
    .map_err(|e| format!("element screenshot failed for `{sel}`: {e}"))?;
    check_png_size(&png)?;
    Ok(image_output(
        &png,
        format!("[screenshot of element `{sel}`]"),
    ))
}

fn check_png_size(png: &[u8]) -> Result<(), String> {
    if png.len() > MAX_PNG_BYTES {
        return Err(format!(
            "screenshot too large ({} bytes, max {}); narrow the viewport or disable full_page",
            png.len(),
            MAX_PNG_BYTES
        ));
    }
    Ok(())
}

fn image_output(png: &[u8], caption: String) -> ToolOutput {
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    ToolOutput::Image {
        caption,
        source: ImageSource::new(ImageMediaType::Png, Arc::from(data)),
    }
}

fn build_screenshot_opts(full_page: bool, region: Option<&[u32]>) -> ScreenshotOptions {
    let mut builder = ScreenshotOptions::builder();
    if full_page {
        builder = builder.full_page(true);
    }
    if let Some(r) = region
        && r.len() == 4
    {
        builder = builder.clip(ScreenshotClip {
            x: r[0] as f64,
            y: r[1] as f64,
            width: r[2] as f64,
            height: r[3] as f64,
        });
    }
    builder.build()
}

async fn page_summary(page: &Page) -> Result<String, String> {
    let url = page.url();
    let title = page
        .title()
        .await
        .map_err(|e| format!("failed to read title: {e}"))?;
    let body = read_inner_text(page, None).await?;
    Ok(format!("# {title}\n\nurl: {url}\n\n{body}"))
}

async fn read_inner_text(page: &Page, selector: Option<&str>) -> Result<String, String> {
    let raw = match selector {
        Some(sel) => {
            let locator = page.locator(sel).await;
            locator
                .inner_text()
                .await
                .map_err(|e| format!("failed to read element text: {e}"))?
        }
        None => {
            let body: String = page
                .evaluate::<(), String>(
                    "() => document.body ? (document.body.innerText || '') : ''",
                    None::<&()>,
                )
                .await
                .map_err(|e| format!("failed to read page text: {e}"))?;
            body
        }
    };
    Ok(cap_text(raw.trim()))
}

fn validate_url(raw: &str) -> Result<&str, String> {
    let url = raw.trim();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(NON_HTTP_MSG.replace("{url}", url));
    }
    Ok(url)
}

fn cap_text(text: &str) -> String {
    cap_at(text, MAX_TEXT_BYTES, TRUNCATION_MARKER)
}

fn cap_eval(text: &str) -> String {
    cap_at(text, MAX_EVAL_BYTES, TRUNCATION_MARKER)
}

fn cap_at(text: &str, max_bytes: usize, marker: &str) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + marker.len());
    out.push_str(&text[..cut]);
    out.push_str(marker);
    out
}

fn parse_chord(input: &str) -> Result<String, String> {
    let parts: Vec<&str> = input.split('+').map(|p| p.trim()).collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return Err(format!("invalid key chord: '{input}'"));
    }
    let (modifiers, key) = parts.split_at(parts.len() - 1);
    let mut out = String::new();
    for m in modifiers {
        out.push_str(&normalize_modifier(m)?);
        out.push('+');
    }
    out.push_str(&normalize_key(key[0])?);
    Ok(out)
}

fn normalize_modifier(m: &str) -> Result<String, String> {
    Ok(match m.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => "Control".to_string(),
        "alt" | "option" => "Alt".to_string(),
        "shift" => "Shift".to_string(),
        "cmd" | "meta" | "command" | "win" => "Meta".to_string(),
        other => return Err(format!("unknown modifier: '{other}'")),
    })
}

fn normalize_key(k: &str) -> Result<String, String> {
    Ok(match k {
        "Enter" | "enter" => "Enter".to_string(),
        "Tab" | "tab" => "Tab".to_string(),
        "Escape" | "escape" | "Esc" | "esc" => "Escape".to_string(),
        "Backspace" | "backspace" => "Backspace".to_string(),
        "Delete" | "delete" | "Del" | "del" => "Delete".to_string(),
        "ArrowUp" | "up" => "ArrowUp".to_string(),
        "ArrowDown" | "down" => "ArrowDown".to_string(),
        "ArrowLeft" | "left" => "ArrowLeft".to_string(),
        "ArrowRight" | "right" => "ArrowRight".to_string(),
        "Home" | "home" => "Home".to_string(),
        "End" | "end" => "End".to_string(),
        "PageUp" | "pageup" => "PageUp".to_string(),
        "PageDown" | "pagedown" => "PageDown".to_string(),
        "Space" | "space" => "Space".to_string(),
        single if single.chars().count() == 1 => single.to_string(),
        other => return Err(format!("unknown key: '{other}'")),
    })
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

super::impl_tool!(
    Browser,
    audience = super::ToolAudience::MAIN,
    tier = super::ToolTier::Extended
);

impl super::ToolInvocation for Browser {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(self.start_header()))
    }

    fn permission_scopes(&self) -> super::BoxFuture<'_, Option<super::PermissionScopes>> {
        let scope = match (&self.url, self.action.as_str()) {
            (Some(url), _) => format!("url:{url}"),
            (
                None,
                "new_tab" | "open" | "screenshot" | "get_content" | "click" | "type" | "fill_form"
                | "eval" | "interactables" | "select",
            ) => format!("browser:{}", self.action),
            _ => "browser:*".to_string(),
        };
        Box::pin(std::future::ready(Some(super::PermissionScopes::single(
            scope,
        ))))
    }

    fn execute<'a>(self: Box<Self>, _ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { self.run().await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

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
    fn validate_url_accepts_local_dev_server() {
        assert_eq!(
            validate_url("http://localhost:1420").unwrap(),
            "http://localhost:1420"
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

    #[test_case("Enter"              ; "single_key")]
    #[test_case("ctrl+shift+t"       ; "chord")]
    #[test_case("cmd+a"              ; "cmd_modifier")]
    #[test_case("Control+Alt+Delete" ; "mixed_case")]
    fn parse_chord_valid(input: &str) {
        let out = parse_chord(input).unwrap();
        assert!(!out.is_empty());
        if input.contains('+') {
            assert!(out.contains('+'), "chord should keep '+': {out}");
        }
    }

    #[test_case(""        ; "empty")]
    #[test_case("ctrl+"   ; "trailing_plus")]
    fn parse_chord_invalid(input: &str) {
        assert!(parse_chord(input).is_err());
    }

    #[test_case("ctrl", "Control" ; "ctrl")]
    #[test_case("control", "Control" ; "control_full")]
    #[test_case("cmd", "Meta" ; "cmd")]
    #[test_case("meta", "Meta" ; "meta")]
    #[test_case("alt", "Alt" ; "alt")]
    #[test_case("shift", "Shift" ; "shift")]
    fn normalize_modifier_maps(input: &str, expected: &str) {
        assert_eq!(normalize_modifier(input).unwrap(), expected);
    }

    #[test]
    fn start_header_formats_each_action() {
        let mk = |action: &str, url: Option<&str>, selector: Option<&str>| Browser {
            action: action.into(),
            url: url.map(String::from),
            selector: selector.map(String::from),
            text: None,
            key: None,
            fields: None,
            script: None,
            format: None,
            filter: None,
            tab: None,
            width: None,
            height: None,
            wait_ms: None,
            full_page: None,
            region: None,
            direction: None,
            amount: None,
            to_top: None,
            timeout_ms: None,
            visible: None,
            value: None,
            clear: None,
            submit: None,
            submit_selector: None,
        };
        assert_eq!(
            mk("screenshot", Some("https://a.test"), None).start_header(),
            "browser screenshot https://a.test"
        );
        assert_eq!(
            mk("open", Some("https://b.test"), None).start_header(),
            "browser open https://b.test"
        );
        assert_eq!(
            mk("click", Some("https://c.test"), Some("a.go")).start_header(),
            "browser click a.go on https://c.test"
        );
        assert_eq!(
            mk("list_tabs", None, None).start_header(),
            "browser list_tabs"
        );
    }

    #[test]
    fn valid_actions_table_is_complete() {
        assert_eq!(VALID_ACTIONS.len(), 18);
        for a in [
            "status",
            "open",
            "screenshot",
            "get_content",
            "click",
            "type",
            "eval",
        ] {
            assert!(VALID_ACTIONS.contains(&a), "missing action: {a}");
        }
    }
}
