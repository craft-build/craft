use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use craft_providers::{ImageMediaType, ImageSource};
use craft_tool_macro::{Args, Tool};
use once_cell::sync::OnceCell;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use xa11y::input::ScrollDelta;
use xa11y::{
    Anchor, App, AppExt, Element, InputSim, Key, Rect, Subscription, TreeNode, input_sim,
    screenshot, screenshot_element, screenshot_region,
};

use crate::ToolOutput;

const DEFAULT_TREE_DEPTH: usize = 4;
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_SCROLL_TICKS: i32 = 3;
const DEFAULT_EVENT_TIMEOUT_MS: u64 = 10_000;
/// One notch of a typical scroll wheel; used as the magnitude for `to_top`/`to_bottom`.
const SCROLL_PAGE_TICKS: i32 = 127;
/// Upper bound on user-supplied timeouts so a confused/careless caller cannot wedge
/// the single worker thread for minutes (`wait`/`next_event`/`connect` serialize it).
const MAX_TIMEOUT_MS: u64 = 60_000;
const MAX_PNG_BYTES: usize = 8 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const TRUNCATION_MARKER: &str = "\n[truncated ...]";
const NOT_CONNECTED_MSG: &str =
    "no active app. Use `desktop connect` (with `app` or `pid`) to select a target first.";

const VALID_ACTIONS: &[&str] = &[
    "status",
    "apps",
    "connect",
    "active",
    "disconnect",
    "tree",
    "dump",
    "read",
    "find",
    "screenshot",
    "click",
    "type",
    "fill",
    "press",
    "scroll",
    "wait",
    "select",
    "subscribe",
    "next_event",
];

const ACTIONS_NEED_APP: &[&str] = &[
    "active",
    "disconnect",
    "tree",
    "dump",
    "read",
    "find",
    "screenshot",
    "click",
    "type",
    "fill",
    "scroll",
    "wait",
    "select",
    "subscribe",
    "next_event",
];

// ── Async bridge: a dedicated OS thread owns the xa11y session (process-global
// singleton provider + the active `App`, an `InputSim`, and any subscription).
// All blocking xa11y calls happen on that thread; the async tool talks to it
// over a flume channel, one `DesktopCmd` per invocation carried alongside a
// oneshot reply. Re-creating the provider per call (spawn_blocking) would race
// the singleton and discard the active app/subscription between invocations, so
// a long-lived worker is the clean ownership story. The thread exits when all
// senders drop.

static SESSION: OnceCell<DesktopHandle> = OnceCell::new();

struct DesktopHandle {
    tx: flume::Sender<(DesktopCmd, Reply)>,
}

impl DesktopHandle {
    fn send(&self, cmd: DesktopCmd, reply: Reply) {
        let _ = self.tx.send((cmd, reply));
    }
}

type Reply = oneshot::Sender<DesktopResult>;

// Payload only — the reply travels separately so the worker can always answer,
// even when the handler panics before producing a result.
enum DesktopCmd {
    Apps,
    Connect {
        target: ConnectTarget,
        timeout: Duration,
    },
    Active,
    Disconnect,
    Tree {
        max_depth: Option<usize>,
    },
    Dump {
        max_depth: Option<usize>,
    },
    Read {
        selector: Option<String>,
        format: String,
        max_depth: Option<usize>,
    },
    Find {
        selector: String,
        limit: Option<usize>,
    },
    Screenshot {
        target: ShotTarget,
    },
    Click {
        selector: String,
    },
    Type {
        selector: String,
        text: String,
        clear: bool,
        submit: bool,
    },
    Fill {
        fields: Vec<DesktopField>,
        submit: bool,
    },
    Press {
        key: String,
    },
    Scroll {
        selector: Option<String>,
        direction: String,
        amount: i32,
        to_top: Option<bool>,
    },
    Wait {
        selector: String,
        state: String,
        timeout: Duration,
    },
    Select {
        selector: String,
        value: String,
    },
    Subscribe,
    NextEvent {
        timeout: Duration,
        filter: Option<String>,
    },
}

#[derive(Clone)]
enum ConnectTarget {
    Name(String),
    Pid(u32),
}

#[derive(Clone, Debug)]
enum ShotTarget {
    Full,
    Element(String),
    Region(Rect),
}

enum DesktopResult {
    Text(String),
    Image { png_base64: String, caption: String },
    Err(String),
}

struct Worker {
    rx: flume::Receiver<(DesktopCmd, Reply)>,
    app: Option<App>,
    input: Option<InputSim>,
    subscription: Option<Subscription>,
}

impl Worker {
    fn run(mut self) {
        while let Ok((cmd, reply)) = self.rx.recv() {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| self.handle(&cmd)))
                .unwrap_or_else(|panic| {
                    // A panic may have torn platform handles held in `self`; drop
                    // the active app/subscription so the next call starts clean
                    // instead of reusing half-released state.
                    self.app = None;
                    self.subscription = None;
                    DesktopResult::Err(format!("desktop worker panicked: {panic:?}"))
                });
            let _ = reply.send(result);
        }
    }

    fn input_sim(&mut self) -> Result<&InputSim, String> {
        if self.input.is_none() {
            self.input = Some(input_sim().map_err(err_str)?);
        }
        Ok(self.input.as_ref().expect("just set"))
    }

    fn require_app(&self) -> Result<&App, String> {
        self.app
            .as_ref()
            .ok_or_else(|| NOT_CONNECTED_MSG.to_string())
    }

    fn press_enter(&mut self) -> Result<(), String> {
        let sim = self.input_sim()?;
        sim.keyboard().chord(Key::Enter, &[]).map_err(err_str)
    }

    fn handle(&mut self, cmd: &DesktopCmd) -> DesktopResult {
        match cmd {
            DesktopCmd::Apps => self.action_apps(),
            DesktopCmd::Connect { target, timeout } => {
                self.action_connect(target.clone(), *timeout)
            }
            DesktopCmd::Active => self.action_active(),
            DesktopCmd::Disconnect => self.action_disconnect(),
            DesktopCmd::Tree { max_depth } => self.action_tree(*max_depth),
            DesktopCmd::Dump { max_depth } => self.action_dump(*max_depth),
            DesktopCmd::Read {
                selector,
                format,
                max_depth,
            } => self.action_read(selector.clone(), format.clone(), *max_depth),
            DesktopCmd::Find { selector, limit } => self.action_find(selector.clone(), *limit),
            DesktopCmd::Screenshot { target } => self.action_screenshot(target.clone()),
            DesktopCmd::Click { selector } => self.action_click(selector.clone()),
            DesktopCmd::Type {
                selector,
                text,
                clear,
                submit,
            } => self.action_type(selector.clone(), text.clone(), *clear, *submit),
            DesktopCmd::Fill { fields, submit } => self.action_fill(fields.clone(), *submit),
            DesktopCmd::Press { key } => self.action_press(key.clone()),
            DesktopCmd::Scroll {
                selector,
                direction,
                amount,
                to_top,
            } => self.action_scroll(selector.clone(), direction.clone(), *amount, *to_top),
            DesktopCmd::Wait {
                selector,
                state,
                timeout,
            } => self.action_wait(selector.clone(), state.clone(), *timeout),
            DesktopCmd::Select { selector, value } => {
                self.action_select(selector.clone(), value.clone())
            }
            DesktopCmd::Subscribe => self.action_subscribe(),
            DesktopCmd::NextEvent { timeout, filter } => {
                self.action_next_event(*timeout, filter.clone())
            }
        }
    }

    fn action_apps(&mut self) -> DesktopResult {
        let apps = match App::list() {
            Ok(a) => a,
            Err(e) => return DesktopResult::Err(format!("failed to list apps: {e}")),
        };
        if apps.is_empty() {
            return DesktopResult::Text("No running apps found.".into());
        }
        let mut out = String::from("| App | PID | Focused |\n|-----|-----|---------|\n");
        for a in apps {
            let pid = a.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
            let focused = if a.focused() { "yes" } else { "" };
            out.push_str(&format!("| {} | {} | {} |\n", a.name, pid, focused));
        }
        DesktopResult::Text(cap_text(&out))
    }

    fn action_connect(&mut self, target: ConnectTarget, timeout: Duration) -> DesktopResult {
        let app = match target {
            ConnectTarget::Name(name) => App::by_name(&name, timeout),
            ConnectTarget::Pid(pid) => App::by_pid(pid, timeout),
        };
        match app {
            Ok(a) => {
                let pid = a.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
                let summary = format!("Connected to {} (pid {}).", a.name, pid);
                self.app = Some(a);
                self.subscription = None;
                DesktopResult::Text(summary)
            }
            Err(e) => DesktopResult::Err(format!("connect failed: {e}")),
        }
    }

    fn action_active(&self) -> DesktopResult {
        let Some(app) = &self.app else {
            return DesktopResult::Err(NOT_CONNECTED_MSG.to_string());
        };
        let pid = app.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
        DesktopResult::Text(format!(
            "Active app: {} (pid {})\nFocused: {}",
            app.name,
            pid,
            app.focused()
        ))
    }

    fn action_disconnect(&mut self) -> DesktopResult {
        if self.app.is_none() {
            return DesktopResult::Err(NOT_CONNECTED_MSG.to_string());
        }
        self.app = None;
        self.subscription = None;
        DesktopResult::Text("Disconnected.".into())
    }

    fn action_tree(&self, max_depth: Option<usize>) -> DesktopResult {
        let app = match self.require_app() {
            Ok(a) => a,
            Err(e) => return DesktopResult::Err(e),
        };
        let depth = max_depth.or(Some(DEFAULT_TREE_DEPTH));
        let tree = match app.tree(depth) {
            Ok(t) => t,
            Err(e) => return DesktopResult::Err(format!("tree failed: {e}")),
        };
        let mut out = String::new();
        render_tree(&tree, 0, &mut out);
        DesktopResult::Text(cap_text(&out))
    }

    fn action_dump(&self, max_depth: Option<usize>) -> DesktopResult {
        let app = match self.require_app() {
            Ok(a) => a,
            Err(e) => return DesktopResult::Err(e),
        };
        let depth = max_depth.or(Some(DEFAULT_TREE_DEPTH));
        match app.dump(depth) {
            Ok(s) => DesktopResult::Text(cap_text(&s)),
            Err(e) => DesktopResult::Err(format!("dump failed: {e}")),
        }
    }

    fn action_read(
        &self,
        selector: Option<String>,
        format: String,
        max_depth: Option<usize>,
    ) -> DesktopResult {
        let app = match self.require_app() {
            Ok(a) => a,
            Err(e) => return DesktopResult::Err(e),
        };
        let depth = max_depth.or(Some(DEFAULT_TREE_DEPTH));
        let tree = if let Some(sel) = &selector {
            match app.locator(sel).tree(depth) {
                Ok(t) => t,
                Err(e) => return DesktopResult::Err(format!("read failed for `{sel}`: {e}")),
            }
        } else {
            match app.tree(depth) {
                Ok(t) => t,
                Err(e) => return DesktopResult::Err(format!("read failed: {e}")),
            }
        };
        let out = match format.as_str() {
            "json" => serde_json::to_string_pretty(&tree)
                .map(|s| cap_text(&s))
                .unwrap_or_else(|_| cap_text(&format!("{tree:?}"))),
            "text" => {
                let mut buf = String::new();
                flatten_tree_text(&tree, &mut buf);
                cap_text(&buf)
            }
            _ => {
                let mut buf = String::new();
                render_tree(&tree, 0, &mut buf);
                cap_text(&buf)
            }
        };
        DesktopResult::Text(out)
    }

    fn action_find(&self, selector: String, limit: Option<usize>) -> DesktopResult {
        let app = match self.require_app() {
            Ok(a) => a,
            Err(e) => return DesktopResult::Err(e),
        };
        let mut elements = match app.locator(&selector).elements() {
            Ok(e) => e,
            Err(e) => return DesktopResult::Err(format!("find failed for `{selector}`: {e}")),
        };
        if let Some(n) = limit {
            elements.truncate(n);
        }
        let arr: Vec<Value> = elements.iter().map(element_json).collect();
        let json = serde_json::to_string_pretty(&arr).unwrap_or_else(|_| format!("{arr:?}"));
        DesktopResult::Text(cap_text(&json))
    }

    fn action_screenshot(&mut self, target: ShotTarget) -> DesktopResult {
        let (shot_res, label) = match &target {
            ShotTarget::Full => (screenshot(), "full".to_string()),
            ShotTarget::Region(r) => (
                screenshot_region(*r),
                format!("region [{},{},{},{}]", r.x, r.y, r.width, r.height),
            ),
            ShotTarget::Element(sel) => {
                let app = match self.require_app() {
                    Ok(a) => a,
                    Err(e) => return DesktopResult::Err(e),
                };
                let element = match app.locator(sel).element() {
                    Ok(e) => e,
                    Err(e) => {
                        return DesktopResult::Err(format!("screenshot failed for `{sel}`: {e}"));
                    }
                };
                (screenshot_element(&element), format!("element `{sel}`"))
            }
        };
        let shot = match shot_res {
            Ok(s) => s,
            Err(e) => return DesktopResult::Err(format!("screenshot failed: {e}")),
        };
        let png = match shot.to_png() {
            Ok(p) => p,
            Err(e) => return DesktopResult::Err(format!("png encode failed: {e}")),
        };
        if let Err(e) = check_png_size(&png) {
            return DesktopResult::Err(e);
        }
        let data = base64::engine::general_purpose::STANDARD.encode(&png);
        let caption = format!("[screenshot of {label}, {}x{}]", shot.width, shot.height);
        DesktopResult::Image {
            png_base64: data,
            caption,
        }
    }

    fn action_click(&self, selector: String) -> DesktopResult {
        let app = match self.require_app() {
            Ok(a) => a,
            Err(e) => return DesktopResult::Err(e),
        };
        match app.locator(&selector).press() {
            Ok(()) => DesktopResult::Text(format!("Activated `{selector}`.")),
            Err(e) => DesktopResult::Err(format!(
                "click failed for `{selector}`: {e} (use `desktop find` or `desktop screenshot` to inspect the tree)"
            )),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn action_type(
        &mut self,
        selector: String,
        text: String,
        clear: bool,
        submit: bool,
    ) -> DesktopResult {
        let app = match self.require_app() {
            Ok(a) => a,
            Err(e) => return DesktopResult::Err(e),
        };
        let locator = app.locator(&selector);
        if clear {
            let _ = locator.set_value("");
        }
        if let Err(e) = locator.type_text(&text) {
            return DesktopResult::Err(format!("failed to type into `{selector}`: {e}"));
        }
        if submit && let Err(e) = self.press_enter() {
            return DesktopResult::Err(format!("failed to submit: {e}"));
        }
        let preview: String = text.chars().take(40).collect();
        DesktopResult::Text(format!(
            "Typed \"{preview}\" into `{selector}`{}.",
            if submit { " and pressed Enter" } else { "" }
        ))
    }

    fn action_fill(&mut self, fields: Vec<DesktopField>, submit: bool) -> DesktopResult {
        let app = match self.require_app() {
            Ok(a) => a,
            Err(e) => return DesktopResult::Err(e),
        };
        let mut filled = 0usize;
        for f in &fields {
            let locator = app.locator(&f.selector);
            let res = match f.field_type.as_deref().unwrap_or("text") {
                "select" => locator.set_value(&f.value),
                "check" => {
                    let checked = f.checked.unwrap_or(true);
                    if checked {
                        locator.select()
                    } else {
                        locator.toggle()
                    }
                }
                _ => locator.set_value(&f.value),
            };
            if let Err(e) = res {
                return DesktopResult::Err(format!("failed to fill `{}`: {e}", f.selector));
            }
            filled += 1;
        }
        if submit && let Err(e) = self.press_enter() {
            return DesktopResult::Err(format!("failed to submit: {e}"));
        }
        DesktopResult::Text(format!(
            "Filled {filled} field(s){}.",
            if submit { " and pressed Enter" } else { "" }
        ))
    }

    fn action_press(&mut self, key: String) -> DesktopResult {
        let (main, held) = match parse_chord(&key) {
            Ok(parts) => parts,
            Err(e) => return DesktopResult::Err(e),
        };
        let sim = match self.input_sim() {
            Ok(s) => s,
            Err(e) => return DesktopResult::Err(e),
        };
        if let Err(e) = sim.keyboard().chord(main, &held) {
            return DesktopResult::Err(format!("failed to press `{key}`: {}", err_str(e)));
        }
        DesktopResult::Text(format!("Pressed `{key}`."))
    }

    fn action_scroll(
        &mut self,
        selector: Option<String>,
        direction: String,
        amount: i32,
        to_top: Option<bool>,
    ) -> DesktopResult {
        let app = match self.require_app() {
            Ok(a) => a,
            Err(e) => return DesktopResult::Err(e),
        };
        if let Some(sel) = &selector {
            if let Err(e) = app.locator(sel).scroll_into_view() {
                return DesktopResult::Err(format!("failed to scroll `{sel}` into view: {e}"));
            }
            return DesktopResult::Text(format!("Scrolled `{sel}` into view."));
        }
        let dy = match to_top {
            Some(true) => -SCROLL_PAGE_TICKS,
            Some(false) => SCROLL_PAGE_TICKS,
            None => {
                let up = direction.eq_ignore_ascii_case("up");
                if !up && !direction.eq_ignore_ascii_case("down") {
                    return DesktopResult::Err(format!(
                        "invalid scroll direction '{direction}': expected 'up' or 'down'"
                    ));
                }
                if up { -amount } else { amount }
            }
        };
        let active = app.as_element();
        let point = match xa11y::point_for(&active, Anchor::Center) {
            Ok(p) => p,
            Err(e) => return DesktopResult::Err(format!("scroll failed: {e}")),
        };
        let sim = match self.input_sim() {
            Ok(s) => s,
            Err(e) => return DesktopResult::Err(e),
        };
        if let Err(e) = sim.mouse().scroll(point, ScrollDelta::vertical(dy)) {
            return DesktopResult::Err(format!("scroll failed: {e}"));
        }
        DesktopResult::Text("Scrolled.".into())
    }

    fn action_wait(&self, selector: String, state: String, timeout: Duration) -> DesktopResult {
        let app = match self.require_app() {
            Ok(a) => a,
            Err(e) => return DesktopResult::Err(e),
        };
        let locator = app.locator(&selector);
        let res = match state.as_str() {
            "attached" => locator.wait_attached(timeout).map(|_| ()),
            "enabled" => locator.wait_enabled(timeout).map(|_| ()),
            "hidden" => locator.wait_hidden(timeout),
            "disabled" => locator.wait_disabled(timeout).map(|_| ()),
            "visible" => locator.wait_visible(timeout).map(|_| ()),
            other => return DesktopResult::Err(format!("unknown wait state '{other}'")),
        };
        match res {
            Ok(()) => DesktopResult::Text(format!("Element `{selector}` is {state}.")),
            Err(e) => DesktopResult::Err(format!("wait failed for `{selector}`: {e}")),
        }
    }

    fn action_select(&self, selector: String, value: String) -> DesktopResult {
        let app = match self.require_app() {
            Ok(a) => a,
            Err(e) => return DesktopResult::Err(e),
        };
        if let Err(e) = app.locator(&selector).set_value(&value) {
            return DesktopResult::Err(format!("failed to select `{value}` in `{selector}`: {e}"));
        }
        DesktopResult::Text(format!("Selected `{value}` in `{selector}`."))
    }

    fn action_subscribe(&mut self) -> DesktopResult {
        let app = match self.require_app() {
            Ok(a) => a,
            Err(e) => return DesktopResult::Err(e),
        };
        match app.subscribe() {
            Ok(sub) => {
                self.subscription = Some(sub);
                DesktopResult::Text("Subscribed to events on the active app.".into())
            }
            Err(e) => DesktopResult::Err(format!("subscribe failed: {e}")),
        }
    }

    fn action_next_event(&mut self, timeout: Duration, filter: Option<String>) -> DesktopResult {
        let Some(sub) = &self.subscription else {
            return DesktopResult::Err(
                "no active subscription. Run `desktop subscribe` first.".into(),
            );
        };
        let event = match sub.recv(timeout) {
            Ok(ev) => ev,
            Err(e) => return DesktopResult::Err(format!("next_event: {e}")),
        };
        if let Some(f) = filter.as_deref()
            && !event_kind_name(&event.kind).eq_ignore_ascii_case(f)
        {
            return DesktopResult::Err(format!(
                "no event matching filter '{f}' within timeout (saw {})",
                event_kind_name(&event.kind)
            ));
        }
        let kind = event_kind_name(&event.kind).to_string();
        let target = event
            .target
            .as_ref()
            .map(|t| format!("{} {}", t.role, t.name.as_deref().unwrap_or("")))
            .unwrap_or_default();
        let obj = json!({
            "kind": kind,
            "app": event.app_name,
            "pid": event.app_pid,
            "target": target,
        });
        DesktopResult::Text(cap_text(&obj.to_string()))
    }
}

fn spawn_worker() -> DesktopHandle {
    let (tx, rx) = flume::unbounded::<(DesktopCmd, Reply)>();
    std::thread::Builder::new()
        .name("desktop-a11y".into())
        .spawn({
            let tx = tx.clone();
            move || {
                Worker {
                    rx,
                    app: None,
                    input: None,
                    subscription: None,
                }
                .run();
                drop(tx);
            }
        })
        .expect("spawn desktop worker thread");
    DesktopHandle { tx }
}

fn session() -> Result<&'static DesktopHandle, String> {
    SESSION.get_or_try_init(|| {
        let handle = spawn_worker();
        Ok::<DesktopHandle, String>(handle)
    })
}

fn send_and_recv(cmd: DesktopCmd) -> Result<DesktopResult, String> {
    let handle = session()?;
    let (tx, rx) = oneshot::channel();
    handle.send(cmd, tx);
    rx.blocking_recv()
        .map_err(|e| format!("desktop worker dropped reply: {e}"))
}

async fn dispatch(cmd: DesktopCmd) -> Result<ToolOutput, String> {
    let result = tokio::task::spawn_blocking(move || send_and_recv(cmd))
        .await
        .map_err(|e| format!("desktop worker task failed: {e}"))??;
    match result {
        DesktopResult::Err(e) => Err(e),
        other => Ok(other.into()),
    }
}

impl From<DesktopResult> for ToolOutput {
    fn from(result: DesktopResult) -> ToolOutput {
        match result {
            DesktopResult::Text(t) => ToolOutput::Markdown(t),
            DesktopResult::Image {
                png_base64,
                caption,
            } => ToolOutput::Image {
                caption,
                source: ImageSource::new(ImageMediaType::Png, Arc::from(png_base64)),
            },
            DesktopResult::Err(e) => ToolOutput::Markdown(e),
        }
    }
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Desktop {
    #[param(description = "Desktop action to perform")]
    action: String,
    #[param(
        description = "App name (for 'connect'). One of 'app' or 'pid' is required for connect."
    )]
    app: Option<String>,
    #[param(description = "Process id alternative to 'app' for 'connect'.")]
    pid: Option<u32>,
    #[param(
        description = "xa11y CSS-like selector (e.g. button[name='OK']). Used by find/click/type/fill/scroll/wait/select/read/screenshot(element)."
    )]
    selector: Option<String>,
    #[param(description = "Text to type ('type') or value to set ('select'/'fill').")]
    text: Option<String>,
    #[param(description = "Key chord for 'press' ('Enter', 'cmd+a', ...).")]
    key: Option<String>,
    #[param(
        description = "Fields to fill (for 'fill'). Array of {selector, value, type?, checked?} objects."
    )]
    fields: Option<Vec<DesktopField>>,
    #[param(description = "Format for 'read': 'tree' (default), 'json', or 'text'.")]
    format: Option<String>,
    #[param(description = "Max tree depth for 'tree'/'dump'/'read' (default 4).")]
    max_depth: Option<usize>,
    #[param(
        description = "Region [x, y, width, height] in logical screen pixels (for 'screenshot')."
    )]
    region: Option<Vec<i32>>,
    #[param(description = "Scroll direction 'up' or 'down' (default 'down').")]
    direction: Option<String>,
    #[param(description = "Scroll amount in ticks (default 3).")]
    amount: Option<i32>,
    #[param(description = "Scroll to top (true) or bottom (false) of content.")]
    to_top: Option<bool>,
    #[param(description = "Limit number of results for 'find'.")]
    limit: Option<usize>,
    #[param(description = "Timeout in ms for connect/wait/next_event (default 10000, max 60000).")]
    timeout_ms: Option<u64>,
    #[param(
        description = "Desired state for 'wait': visible/attached/enabled/hidden/disabled (default visible)."
    )]
    state: Option<String>,
    #[param(description = "Value for 'select'.")]
    value: Option<String>,
    #[param(description = "Clear the field before typing (for 'type', default true).")]
    clear: Option<bool>,
    #[param(description = "Press Enter after typing/filling (default false).")]
    submit: Option<bool>,
    #[param(
        description = "Optional EventKind name filter for 'next_event' (e.g. 'focus_changed')."
    )]
    event_filter: Option<String>,
}

#[derive(Args, Debug, Clone, Deserialize)]
pub struct DesktopField {
    #[param(description = "xa11y selector of the field")]
    selector: String,
    #[param(description = "Value to set (for text/select) or target state")]
    value: String,
    #[param(description = "Field type: 'text', 'select', or 'check' (default text)")]
    #[serde(rename = "type")]
    field_type: Option<String>,
    #[param(description = "For check fields: the target checked state")]
    checked: Option<bool>,
}

impl Desktop {
    pub const NAME: &str = "desktop";
    pub const DESCRIPTION: &str = include_str!("desktop.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
        {"action": "apps"},
        {"action": "connect", "app": "Calculator"},
        {"action": "tree"},
        {"action": "click", "selector": "button[name='=']"},
        {"action": "screenshot"},
        {"action": "type", "selector": "textfield", "text": "hello", "submit": true}
    ]"#,
    );

    pub fn start_header(&self) -> String {
        match self.action.as_str() {
            "status" => "desktop status".into(),
            "apps" => "desktop apps".into(),
            "connect" => {
                let target = self
                    .app
                    .clone()
                    .or_else(|| self.pid.map(|p| p.to_string()))
                    .unwrap_or_else(|| "?".into());
                format!("desktop connect {target}")
            }
            "active" => "desktop active".into(),
            "disconnect" => "desktop disconnect".into(),
            "tree" => "desktop tree".into(),
            "dump" => "desktop dump".into(),
            "read" => {
                let fmt = self.format.as_deref().unwrap_or("tree");
                format!("desktop read ({fmt})")
            }
            "find" => {
                let sel = self.selector.as_deref().unwrap_or("?");
                format!("desktop find {sel}")
            }
            "screenshot" => {
                if let Some(sel) = &self.selector {
                    format!("desktop screenshot {sel}")
                } else {
                    "desktop screenshot".into()
                }
            }
            "click" => {
                let sel = self.selector.as_deref().unwrap_or("?");
                format!("desktop click {sel}")
            }
            "type" => {
                let sel = self.selector.as_deref().unwrap_or("?");
                let text = self.text.as_deref().unwrap_or("");
                let preview: String = text.chars().take(30).collect();
                format!(
                    "desktop type {sel} \"{preview}{}\"",
                    if text.chars().count() > 30 { "..." } else { "" }
                )
            }
            "fill" => {
                let count = self.fields.as_ref().map(|f| f.len()).unwrap_or(0);
                format!("desktop fill ({count} fields)")
            }
            "press" => format!("desktop press {}", self.key.as_deref().unwrap_or("?")),
            "scroll" => {
                if self.to_top == Some(true) {
                    "desktop scroll to top".into()
                } else if self.to_top == Some(false) {
                    "desktop scroll to bottom".into()
                } else if let Some(sel) = &self.selector {
                    format!("desktop scroll to {sel}")
                } else {
                    let dir = self.direction.as_deref().unwrap_or("down");
                    let amt = self.amount.unwrap_or(DEFAULT_SCROLL_TICKS);
                    format!("desktop scroll {dir} {amt}")
                }
            }
            "wait" => {
                let sel = self.selector.as_deref().unwrap_or("?");
                let state = self.state.as_deref().unwrap_or("visible");
                format!("desktop wait for {sel} ({state})")
            }
            "select" => {
                let sel = self.selector.as_deref().unwrap_or("?");
                let val = self.value.as_deref().unwrap_or("?");
                format!("desktop select {val} in {sel}")
            }
            "subscribe" => "desktop subscribe".into(),
            "next_event" => "desktop next_event".into(),
            other => format!("desktop {other}"),
        }
    }

    pub async fn run(&self) -> Result<ToolOutput, String> {
        if !VALID_ACTIONS.contains(&self.action.as_str()) {
            let valid = VALID_ACTIONS.join(", ");
            return Err(format!(
                "invalid desktop action '{}'. Valid actions: {valid}",
                self.action
            ));
        }
        if self.action == "status" {
            return Self::action_status().await;
        }
        if ACTIONS_NEED_APP.contains(&self.action.as_str()) && SESSION.get().is_none() {
            return Err(
                "desktop worker not started. Run any desktop action to start it, or `desktop status`.".into(),
            );
        }
        match self.action.as_str() {
            "apps" => dispatch(DesktopCmd::Apps).await,
            "connect" => {
                let target = match (&self.app, self.pid) {
                    (Some(name), _) => ConnectTarget::Name(name.clone()),
                    (None, Some(pid)) => ConnectTarget::Pid(pid),
                    (None, None) => return Err("connect requires 'app' or 'pid'".to_string()),
                };
                let t = self.capped_timeout(DEFAULT_CONNECT_TIMEOUT_MS);
                dispatch(DesktopCmd::Connect { target, timeout: t }).await
            }
            "active" => dispatch(DesktopCmd::Active).await,
            "disconnect" => dispatch(DesktopCmd::Disconnect).await,
            "tree" => {
                dispatch(DesktopCmd::Tree {
                    max_depth: self.max_depth,
                })
                .await
            }
            "dump" => {
                dispatch(DesktopCmd::Dump {
                    max_depth: self.max_depth,
                })
                .await
            }
            "read" => {
                dispatch(DesktopCmd::Read {
                    selector: self.selector.clone(),
                    format: self.format.clone().unwrap_or_else(|| "tree".into()),
                    max_depth: self.max_depth,
                })
                .await
            }
            "find" => {
                let sel = self
                    .selector
                    .clone()
                    .ok_or_else(|| "find requires 'selector'".to_string())?;
                dispatch(DesktopCmd::Find {
                    selector: sel,
                    limit: self.limit,
                })
                .await
            }
            "screenshot" => {
                let target = self.build_shot_target()?;
                dispatch(DesktopCmd::Screenshot { target }).await
            }
            "click" => {
                let sel = self
                    .selector
                    .clone()
                    .ok_or_else(|| "click requires 'selector'".to_string())?;
                dispatch(DesktopCmd::Click { selector: sel }).await
            }
            "type" => {
                let sel = self
                    .selector
                    .clone()
                    .ok_or_else(|| "type requires 'selector'".to_string())?;
                let text = self
                    .text
                    .clone()
                    .ok_or_else(|| "type requires 'text'".to_string())?;
                dispatch(DesktopCmd::Type {
                    selector: sel,
                    text,
                    clear: self.clear.unwrap_or(true),
                    submit: self.submit.unwrap_or(false),
                })
                .await
            }
            "fill" => {
                let fields = self
                    .fields
                    .clone()
                    .ok_or_else(|| "fill requires 'fields'".to_string())?;
                if fields.is_empty() {
                    return Err("fill requires at least one field".into());
                }
                dispatch(DesktopCmd::Fill {
                    fields,
                    submit: self.submit.unwrap_or(false),
                })
                .await
            }
            "press" => {
                let key = self
                    .key
                    .clone()
                    .ok_or_else(|| "press requires 'key'".to_string())?;
                dispatch(DesktopCmd::Press { key }).await
            }
            "scroll" => {
                dispatch(DesktopCmd::Scroll {
                    selector: self.selector.clone(),
                    direction: self.direction.clone().unwrap_or_else(|| "down".into()),
                    amount: self.amount.unwrap_or(DEFAULT_SCROLL_TICKS),
                    to_top: self.to_top,
                })
                .await
            }
            "wait" => {
                let sel = self
                    .selector
                    .clone()
                    .ok_or_else(|| "wait requires 'selector'".to_string())?;
                let state = self.state.clone().unwrap_or_else(|| "visible".into());
                let timeout = self.capped_timeout(DEFAULT_WAIT_TIMEOUT_MS);
                dispatch(DesktopCmd::Wait {
                    selector: sel,
                    state,
                    timeout,
                })
                .await
            }
            "select" => {
                let sel = self
                    .selector
                    .clone()
                    .ok_or_else(|| "select requires 'selector'".to_string())?;
                let value = self
                    .value
                    .clone()
                    .ok_or_else(|| "select requires 'value'".to_string())?;
                dispatch(DesktopCmd::Select {
                    selector: sel,
                    value,
                })
                .await
            }
            "subscribe" => dispatch(DesktopCmd::Subscribe).await,
            "next_event" => {
                let t = self.capped_timeout(DEFAULT_EVENT_TIMEOUT_MS);
                dispatch(DesktopCmd::NextEvent {
                    timeout: t,
                    filter: self.event_filter.clone(),
                })
                .await
            }
            _ => unreachable!("validated above"),
        }
    }

    fn capped_timeout(&self, default_ms: u64) -> Duration {
        Duration::from_millis(self.timeout_ms.unwrap_or(default_ms).min(MAX_TIMEOUT_MS))
    }

    fn build_shot_target(&self) -> Result<ShotTarget, String> {
        if let Some(sel) = &self.selector {
            return Ok(ShotTarget::Element(sel.clone()));
        }
        if let Some(r) = self.region.as_deref() {
            if r.len() != 4 {
                return Err("region must have exactly 4 elements [x, y, width, height]".to_string());
            }
            return Ok(ShotTarget::Region(Rect {
                x: r[0],
                y: r[1],
                width: r[2] as u32,
                height: r[3] as u32,
            }));
        }
        Ok(ShotTarget::Full)
    }

    async fn action_status() -> Result<ToolOutput, String> {
        let started = SESSION.get().is_some();
        let mut out = format!(
            "Desktop worker: {}\n",
            if started { "running" } else { "not started" }
        );
        if started {
            out.push_str("(use `desktop active` to inspect the connected app)\n");
        }
        out.push_str(
            "\nOn macOS, grant Accessibility (and Screen Recording for screenshots) to the process running this tool. \
             On Linux, ensure an AT-SPI2 session is active.",
        );
        Ok(ToolOutput::Markdown(out))
    }
}

fn err_str(e: xa11y::Error) -> String {
    e.to_string()
}

// ── Tree / element rendering ─────────────────────────────────────────────

fn render_tree(node: &TreeNode, depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    let name = node.name.as_deref().unwrap_or("");
    let label = if name.is_empty() {
        node.role.clone()
    } else {
        format!("{} \"{}\"", node.role, name)
    };
    out.push_str(&label);
    if let Some(v) = &node.value {
        out.push_str(&format!(" = {v}"));
    }
    out.push('\n');
    for child in &node.children {
        render_tree(child, depth + 1, out);
    }
}

fn flatten_tree_text(node: &TreeNode, out: &mut String) {
    if let Some(name) = &node.name
        && !name.is_empty()
    {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(name);
    }
    for child in &node.children {
        flatten_tree_text(child, out);
    }
}

fn element_json(el: &Element) -> Value {
    let bounds = el
        .bounds
        .map(|b| json!({"x": b.x, "y": b.y, "width": b.width, "height": b.height}));
    json!({
        "role": el.role.to_string(),
        "name": el.name,
        "value": el.value,
        "bounds": bounds,
        "actions": el.actions,
        "states": states_json(&el.states),
    })
}

fn states_json(s: &xa11y::StateSet) -> Value {
    let mut arr = Vec::new();
    if s.enabled {
        arr.push("enabled");
    }
    if s.visible {
        arr.push("visible");
    }
    if s.focused {
        arr.push("focused");
    }
    if s.selected {
        arr.push("selected");
    }
    if s.editable {
        arr.push("editable");
    }
    if s.focusable {
        arr.push("focusable");
    }
    if s.modal {
        arr.push("modal");
    }
    if s.required {
        arr.push("required");
    }
    if s.busy {
        arr.push("busy");
    }
    if let Some(c) = s.checked {
        arr.push(match c {
            xa11y::Toggled::On => "checked",
            xa11y::Toggled::Off => "unchecked",
            xa11y::Toggled::Mixed => "mixed",
        });
    }
    if let Some(true) = s.expanded {
        arr.push("expanded");
    }
    if let Some(false) = s.expanded {
        arr.push("collapsed");
    }
    json!(arr)
}

fn event_kind_name(kind: &xa11y::EventKind) -> &'static str {
    use xa11y::EventKind::*;
    match kind {
        FocusChanged => "focus_changed",
        ValueChanged => "value_changed",
        NameChanged => "name_changed",
        StateChanged { .. } => "state_changed",
        StructureChanged => "structure_changed",
        WindowOpened => "window_opened",
        WindowClosed => "window_closed",
        WindowActivated => "window_activated",
        WindowDeactivated => "window_deactivated",
        SelectionChanged => "selection_changed",
        MenuOpened => "menu_opened",
        MenuClosed => "menu_closed",
        TextChanged => "text_changed",
        Announcement => "announcement",
    }
}

// ── Shared helpers (mirrors browser.rs) ──────────────────────────────────

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

fn check_png_size(png: &[u8]) -> Result<(), String> {
    if png.len() > MAX_PNG_BYTES {
        return Err(format!(
            "screenshot too large ({} bytes, max {}). Capture a region or an element instead.",
            png.len(),
            MAX_PNG_BYTES
        ));
    }
    Ok(())
}

// Parses a browser-style key chord ("cmd+a", "ctrl+shift+t", "Enter") into the
// final key plus the held modifiers, mapped to xa11y's `Key`. Unlike browser,
// we return structured values instead of a re-joined string.
fn parse_chord(input: &str) -> Result<(Key, Vec<Key>), String> {
    let parts: Vec<&str> = input.split('+').map(str::trim).collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return Err(format!("invalid key chord: '{input}'"));
    }
    let (modifiers, key) = parts.split_at(parts.len() - 1);
    let held: Result<Vec<Key>, String> = modifiers.iter().map(|m| parse_modifier(m)).collect();
    let held = held?;
    let main = parse_key(key[0])?;
    Ok((main, held))
}

fn parse_modifier(m: &str) -> Result<Key, String> {
    match m.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Ok(Key::Ctrl),
        "alt" | "option" => Ok(Key::Alt),
        "shift" => Ok(Key::Shift),
        "cmd" | "meta" | "command" | "win" => Ok(Key::Meta),
        other => Err(format!("unknown modifier: '{other}'")),
    }
}

fn parse_key(k: &str) -> Result<Key, String> {
    match k {
        "Enter" | "enter" | "Return" | "return" => Ok(Key::Enter),
        "Tab" | "tab" => Ok(Key::Tab),
        "Escape" | "escape" | "Esc" | "esc" => Ok(Key::Escape),
        "Backspace" | "backspace" => Ok(Key::Backspace),
        "Delete" | "delete" | "Del" | "del" => Ok(Key::Delete),
        "Insert" | "insert" => Ok(Key::Insert),
        "ArrowUp" | "up" | "Up" => Ok(Key::ArrowUp),
        "ArrowDown" | "down" | "Down" => Ok(Key::ArrowDown),
        "ArrowLeft" | "left" | "Left" => Ok(Key::ArrowLeft),
        "ArrowRight" | "right" | "Right" => Ok(Key::ArrowRight),
        "Home" | "home" => Ok(Key::Home),
        "End" | "end" => Ok(Key::End),
        "PageUp" | "pageup" => Ok(Key::PageUp),
        "PageDown" | "pagedown" => Ok(Key::PageDown),
        "Space" | "space" => Ok(Key::Space),
        "Shift" | "shift" => Ok(Key::Shift),
        "Ctrl" | "ctrl" | "Control" | "control" => Ok(Key::Ctrl),
        "Alt" | "alt" | "Option" | "option" => Ok(Key::Alt),
        "Cmd" | "cmd" | "Meta" | "meta" | "Command" | "command" => Ok(Key::Meta),
        f if f.starts_with(['F', 'f']) && f.len() >= 2 => {
            let n: u8 = f[1..].parse().map_err(|_| format!("unknown key: '{f}'"))?;
            if (1..=12).contains(&n) {
                Ok(Key::F(n))
            } else {
                Err(format!("function key out of range: F{n}"))
            }
        }
        single if single.chars().count() == 1 => {
            let c = single.chars().next().expect("one char");
            Ok(Key::Char(c.to_ascii_lowercase()))
        }
        other => Err(format!("unknown key: '{other}'")),
    }
}

super::impl_tool!(
    Desktop,
    audience = super::ToolAudience::MAIN,
    tier = super::ToolTier::Extended
);

impl super::ToolInvocation for Desktop {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(self.start_header()))
    }

    fn permission_scopes(&self) -> super::BoxFuture<'_, Option<super::PermissionScopes>> {
        let scope = match (&self.app, self.pid, self.action.as_str()) {
            (Some(name), _, "connect") => format!("desktop:app:{name}"),
            (None, Some(pid), "connect") => format!("desktop:pid:{pid}"),
            (
                _,
                _,
                "screenshot" | "apps" | "tree" | "dump" | "read" | "find" | "active" | "connect"
                | "click" | "type" | "fill" | "select" | "press" | "scroll" | "subscribe" | "wait"
                | "next_event",
            ) => format!("desktop:{}", self.action),
            _ => "desktop:*".to_string(),
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

    #[test_case("Enter", Key::Enter, &[] ; "single_key")]
    #[test_case("enter", Key::Enter, &[] ; "lowercase_key")]
    #[test_case("Escape", Key::Escape, &[] ; "escape")]
    #[test_case("F5", Key::F(5), &[] ; "function_key")]
    #[test_case("cmd+a", Key::Char('a'), &[Key::Meta] ; "cmd_modifier")]
    #[test_case("ctrl+shift+t", Key::Char('t'), &[Key::Ctrl, Key::Shift] ; "chord")]
    #[test_case("alt+F4", Key::F(4), &[Key::Alt] ; "alt_function")]
    #[test_case("Control+Alt+Delete", Key::Delete, &[Key::Ctrl, Key::Alt] ; "mixed_case")]
    fn parse_chord_maps(input: &str, expected_main: Key, expected_held: &[Key]) {
        let (main, held) = parse_chord(input).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(main, expected_main);
        assert_eq!(held, expected_held);
    }

    #[test_case("" ; "empty")]
    #[test_case("ctrl+" ; "trailing_plus")]
    #[test_case("F99" ; "function_out_of_range")]
    #[test_case("nonsense" ; "unknown")]
    fn parse_chord_invalid(input: &str) {
        assert!(parse_chord(input).is_err());
    }

    #[test_case("ctrl", Key::Ctrl ; "ctrl")]
    #[test_case("control", Key::Ctrl ; "control_full")]
    #[test_case("cmd", Key::Meta ; "cmd")]
    #[test_case("meta", Key::Meta ; "meta")]
    #[test_case("alt", Key::Alt ; "alt")]
    #[test_case("option", Key::Alt ; "option")]
    #[test_case("shift", Key::Shift ; "shift")]
    fn parse_modifier_maps(input: &str, expected: Key) {
        assert_eq!(parse_modifier(input).unwrap(), expected);
    }

    #[test]
    fn parse_key_uppercase_char_becomes_lowercase() {
        let k = parse_key("A").unwrap();
        assert_eq!(k, Key::Char('a'));
    }

    #[test]
    fn parse_key_arrows() {
        assert_eq!(parse_key("ArrowUp").unwrap(), Key::ArrowUp);
        assert_eq!(parse_key("up").unwrap(), Key::ArrowUp);
        assert_eq!(parse_key("down").unwrap(), Key::ArrowDown);
    }

    #[test]
    fn parse_key_space_and_modifiers_as_keys() {
        assert_eq!(parse_key("Space").unwrap(), Key::Space);
        assert_eq!(parse_key("cmd").unwrap(), Key::Meta);
    }

    #[test]
    fn check_png_size_rejects_oversized() {
        let big = vec![0u8; MAX_PNG_BYTES + 1];
        assert!(check_png_size(&big).is_err());
    }

    #[test]
    fn check_png_size_accepts_limit() {
        let ok = vec![0u8; MAX_PNG_BYTES];
        assert!(check_png_size(&ok).is_ok());
    }

    #[test]
    fn valid_actions_table_is_complete() {
        assert_eq!(VALID_ACTIONS.len(), 19);
        for a in [
            "status",
            "apps",
            "connect",
            "tree",
            "screenshot",
            "click",
            "type",
            "fill",
            "press",
            "scroll",
            "wait",
            "select",
            "subscribe",
            "next_event",
        ] {
            assert!(VALID_ACTIONS.contains(&a), "missing action: {a}");
        }
    }

    #[test]
    fn start_header_formats_each_action() {
        let mk = |action: &str, app: Option<&str>, selector: Option<&str>| Desktop {
            action: action.into(),
            app: app.map(String::from),
            pid: None,
            selector: selector.map(String::from),
            text: None,
            key: None,
            fields: None,
            format: None,
            max_depth: None,
            region: None,
            direction: None,
            amount: None,
            to_top: None,
            limit: None,
            timeout_ms: None,
            state: None,
            value: None,
            clear: None,
            submit: None,
            event_filter: None,
        };
        assert_eq!(mk("apps", None, None).start_header(), "desktop apps");
        assert_eq!(
            mk("connect", Some("Calculator"), None).start_header(),
            "desktop connect Calculator"
        );
        assert_eq!(
            mk("click", None, Some("button[name='=']")).start_header(),
            "desktop click button[name='=']"
        );
        assert_eq!(mk("tree", None, None).start_header(), "desktop tree");
        assert_eq!(mk("press", None, None).start_header(), "desktop press ?");
    }

    #[test]
    fn start_header_type_preview() {
        let desktop = Desktop {
            action: "type".into(),
            app: None,
            pid: None,
            selector: Some("textfield".into()),
            text: Some("hello world this is long".into()),
            key: None,
            fields: None,
            format: None,
            max_depth: None,
            region: None,
            direction: None,
            amount: None,
            to_top: None,
            limit: None,
            timeout_ms: None,
            state: None,
            value: None,
            clear: None,
            submit: None,
            event_filter: None,
        };
        let header = desktop.start_header();
        assert!(header.starts_with("desktop type textfield \""));
    }

    #[test]
    fn render_tree_indents_children() {
        let node = TreeNode {
            role: "window".into(),
            name: Some("Main".into()),
            value: None,
            children: vec![TreeNode {
                role: "button".into(),
                name: Some("OK".into()),
                value: None,
                children: vec![],
            }],
        };
        let mut out = String::new();
        render_tree(&node, 0, &mut out);
        assert!(out.contains("window \"Main\""));
        assert!(out.contains("  button \"OK\""));
    }

    #[test]
    fn flatten_tree_text_concatenates_names() {
        let node = TreeNode {
            role: "window".into(),
            name: Some("A".into()),
            value: None,
            children: vec![TreeNode {
                role: "button".into(),
                name: Some("B".into()),
                value: None,
                children: vec![],
            }],
        };
        let mut out = String::new();
        flatten_tree_text(&node, &mut out);
        assert_eq!(out, "A B");
    }

    #[test]
    fn build_shot_target_parses_region() {
        let d = Desktop {
            action: "screenshot".into(),
            app: None,
            pid: None,
            selector: None,
            text: None,
            key: None,
            fields: None,
            format: None,
            max_depth: None,
            region: Some(vec![10, -20, 300, 400]),
            direction: None,
            amount: None,
            to_top: None,
            limit: None,
            timeout_ms: None,
            state: None,
            value: None,
            clear: None,
            submit: None,
            event_filter: None,
        };
        match d.build_shot_target().unwrap() {
            ShotTarget::Region(r) => {
                assert_eq!(r.x, 10);
                assert_eq!(r.y, -20);
                assert_eq!(r.width, 300);
                assert_eq!(r.height, 400);
            }
            other => panic!("expected region, got {other:?}"),
        }
    }

    #[test]
    fn build_shot_target_rejects_bad_region() {
        let d = Desktop {
            action: "screenshot".into(),
            app: None,
            pid: None,
            selector: None,
            text: None,
            key: None,
            fields: None,
            format: None,
            max_depth: None,
            region: Some(vec![1, 2, 3]),
            direction: None,
            amount: None,
            to_top: None,
            limit: None,
            timeout_ms: None,
            state: None,
            value: None,
            clear: None,
            submit: None,
            event_filter: None,
        };
        assert!(d.build_shot_target().is_err());
    }

    #[test_case("focus_changed", &xa11y::EventKind::FocusChanged ; "focus")]
    #[test_case("value_changed", &xa11y::EventKind::ValueChanged ; "value")]
    #[test_case("window_opened", &xa11y::EventKind::WindowOpened ; "window_open")]
    fn event_kind_name_maps(expected: &str, kind: &xa11y::EventKind) {
        assert_eq!(event_kind_name(kind), expected);
    }
}
