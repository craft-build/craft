Drives a headless browser (Chromium via Playwright) so you can inspect frontends, fill forms, click elements, extract content, and run JavaScript. The browser session persists across calls: pages stay open, tabs are reused, and cookie/localStorage state carries over until you close a tab or the agent run ends.

The `action` field selects the operation. Most actions accept an optional `url` (navigate there first on the active tab) and `tab` (operate on a specific tab index instead of the active one). Only `http` and `https` URLs are allowed.

A note on setup: the browser needs the Playwright driver installed. If a browser action fails with a driver/launch error, run `npm install -g playwright@1.60.0` (or set `PLAYWRIGHT_DRIVER_PATH`) and try again.

## Actions

| Action | Key parameters | Returns |
|---|---|---|
| `status` | none | Markdown: launch state + tab count |
| `list_tabs` | none | Markdown table of tabs (index, URL, title) |
| `new_tab` | `url?`, `width?`, `height?` | New tab index |
| `select_tab` | `tab` | Confirmation |
| `get_active_tab` | none | Active tab index, URL, title |
| `close_tab` | `tab` | Confirmation |
| `open` | `url`, `tab?`, `width?`, `height?`, `wait_ms?` | Page summary: `# title`, final URL, body text |
| `screenshot` | `url?`, `tab?`, `width?`, `height?`, `wait_ms?`, `full_page?`, `selector?`, `region?` | PNG image |
| `get_content` | `url?`, `tab?`, `selector?`, `format?`, `wait_ms?` | Page text / html / markdown / title |
| `interactables` | `url?`, `tab?`, `filter?` | JSON array of interactive elements |
| `click` | `selector`, `url?`, `tab?`, `wait_ms?` | Page text after click |
| `type` | `selector`, `text`, `url?`, `tab?`, `clear?`, `submit?` | Confirmation |
| `fill_form` | `fields[]`, `url?`, `tab?`, `submit?`, `submit_selector?` | Summary |
| `press` | `key`, `url?`, `tab?` | Confirmation |
| `eval` | `script`, `url?`, `tab?` | JSON-serialized JS return value |
| `scroll` | `url?`, `tab?`, `direction?`, `amount?`, `selector?`, `to_top?` | Confirmation |
| `wait` | `selector?`, `url?`, `tab?`, `timeout_ms?`, `visible?` | Confirmation |
| `select` | `selector`, `value`, `url?`, `tab?` | Confirmation |

## Details

**Tabs.** `list_tabs` shows every open tab (the active one is marked `*`). `new_tab` opens a fresh tab and makes it active. `select_tab`/`close_tab` take a tab index. Actions without `tab` operate on the active tab.

**Navigation.** `open` navigates the active tab and returns a page summary (title, final URL after redirects, body text). Any action that takes `url` will navigate first if a URL is supplied, otherwise it acts on the current page.

**Screenshots** require a vision-capable model. `full_page` (default true) captures the entire scrollable page; set false for the viewport. `selector` screenshots a single element. `region` is `[x, y, width, height]` in CSS pixels.

**Content extraction.** `get_content` with `format`:
- `text` (default): `document.body.innerText` (or element `innerText` when `selector` is given), capped at 64 KiB.
- `html`: the page's (or element's) HTML, capped at 64 KiB.
- `markdown`: HTML converted to markdown via `htmd`.
- `title`: just the page title.

**Element discovery.** `interactables` runs a query over links, buttons, inputs, selects, and textareas and returns a JSON array of `{index, tag, type, name, text, href, placeholder, selector, visible}`. Use `filter` (`all`/`links`/`inputs`/`buttons`) to narrow it. Use this to find the right CSS selector before `click`/`type`/`fill_form`.

**Forms.** `type` fills one field (use `submit: true` to press Enter after). `fill_form` fills many at once; each field is `{selector, value, type?, checked?}` where `type` is `text` (default), `select`, `checkbox`, or `radio`. For checkbox/radio, `checked` sets the target state. Set `submit: true` with `submit_selector` to click a submit button afterward.

**Keyboard.** `press` takes a key chord: a single key (`Enter`, `Tab`, `Escape`, `ArrowDown`) optionally prefixed with modifiers joined by `+` (`ctrl+shift+t`, `cmd+a`). Recognized modifiers: `ctrl`/`control`, `alt`/`option`, `shift`, `cmd`/`meta`.

**JavaScript.** `eval` runs arbitrary JS in the page context (inside the browser sandbox; no file or host-network access beyond the page's own origin). The return value is JSON-serialized and capped at 256 KiB.

**Scrolling.** `scroll` moves by `amount` pixels in `direction` (`up`/`down`, default down 500px). `to_top: true`/`false` jumps to the top/bottom. `selector` scrolls a specific element into view (overrides direction/amount).

**Waiting.** `wait` blocks until an element matches `selector` (or, with no selector, until the page reaches a stable load state). `timeout_ms` (default 10000) caps the wait; `visible` (default true) waits for the element to be visible rather than merely attached.

**Dropdowns.** `select` picks `value` in the `<select>` matched by `selector`.

## Tauri and webview apps

The browser tool can inspect and drive the **web frontend** of a Tauri app when that frontend is served over HTTP, such as `http://localhost:1420` in `craft-desktop` dev mode. Point the browser at the dev URL, match the app window size, and use the normal browser actions:

```json
{"action": "open", "url": "http://localhost:1420", "width": 1440, "height": 900}
```

After the page loads you can screenshot the viewport, query interactables, click, type, or run JavaScript just like any other web page.

What the browser tool **cannot** do without modifying the target app:

- Drive the **native app shell** (menus, native dialogs, OS permissions, deep links).
- Exercise **Tauri native commands** (`invoke` calls). These only exist inside the webview runtime, not in the served HTML.
- Work when the webview is not served over HTTP, such as a built app loading `tauri://localhost` on macOS.

This means the browser tool is a good fit for automated checks of the frontend UI, but it is not a full replacement for native UI automation.

## Migration from the old tools

This single tool replaces `browser_screenshot`, `browser_navigate`, `browser_click`, and `browser_text`:
- `browser_screenshot {url}` -> `browser {action:"screenshot", url}`
- `browser_navigate {url}` -> `browser {action:"open", url}`
- `browser_click {url, selector}` -> `browser {action:"click", url, selector}`
- `browser_text {url}` -> `browser {action:"get_content", url}`

Pages are no longer closed after each call; use `close_tab` for explicit cleanup.
