Drives native desktop applications through the platform accessibility tree (AXUIElement on macOS, AT-SPI2 on Linux, UI Automation on Windows). It is the desktop counterpart to the `browser` tool: where `browser` drives Chromium via Playwright, `desktop` drives real apps, including Tauri/webview apps whose content the OS exposes as an ARIA-mapped tree.

The active app (set with `connect`) and any event subscription persist across calls until you `disconnect` or the agent run ends. Unlike `browser`, desktop has a single active app at a time, not a tab list.

The `action` field selects the operation. Most actions take a `selector` (a CSS-like xa11y selector such as `button[name='OK']`) and operate on the active app.

A note on permissions: desktop automation needs the OS to grant this process accessibility access. On macOS that means Accessibility (System Settings > Privacy & Security > Accessibility) and, for screenshots, Screen Recording (on macOS 26 and later, Screen & System Audio Recording). If an action fails with a permission error, the result text includes the exact setup steps to follow.

## Actions

| Action | Key parameters | Returns |
|---|---|---|
| `status` | none | Markdown: worker state + permission setup reminder |
| `apps` | none | Markdown table of running apps (name, pid, focused) |
| `connect` | `app` or `pid`, `timeout_ms?` | Confirmation |
| `active` | none | Active app name, pid, focused state |
| `disconnect` | none | Confirmation |
| `tree` | `max_depth?` | Indented accessibility tree (default depth 4) |
| `dump` | `max_depth?` | Plain-text dump of the tree (xa11y formatting) |
| `read` | `selector?`, `format?`, `max_depth?` | Element subtree text / json / tree |
| `find` | `selector`, `limit?` | JSON array of matching elements (role, name, value, bounds, states, actions) |
| `screenshot` | `selector?`, `region?` | PNG image (full screen, element, or region) |
| `click` | `selector` | Confirmation (semantic activate) |
| `type` | `selector`, `text`, `clear?`, `submit?` | Confirmation |
| `fill` | `fields[]`, `submit?` | Summary |
| `press` | `key` | Confirmation (global key chord) |
| `scroll` | `selector?`, `direction?`, `amount?`, `to_top?` | Confirmation |
| `wait` | `selector`, `state?`, `timeout_ms?` | Confirmation |
| `select` | `selector`, `value` | Confirmation |
| `subscribe` | none | Confirmation (event stream on active app) |
| `next_event` | `timeout_ms?`, `event_filter?` | JSON event or timeout message |

## Details

**Discovery first.** Native app trees vary wildly. Always inspect before acting: run `tree` or `find` to learn the roles and names a selector must match. For webview apps (Tauri/Electron), frontend semantics matter. A `<div onclick>` does not expose a `button` role, so prefer real `<button>` elements in the target app.

**Active app.** `apps` lists running applications (the focused one is marked `*`). `connect` selects one by `app` name or `pid` and makes it the active target; `timeout_ms` (default 10000) lets it wait for an app that is still launching. `active` reports the current target. `disconnect` drops it. All `tree`, `find`, `click`, `type`, etc. operate on the active app.

**Selectors.** xa11y selectors look like CSS over the accessibility tree: `button`, `button[name='=']`, `textfield`, `checkbox[name='Subscribe']`. Use `find` to see what matches before you act on it.

**Inspection.** `tree` prints a bounded indentation view (default `max_depth` 4, since webview subtrees can be huge). `dump` returns the crate's own formatting. `read` extracts text from a subtree: `format` is `tree` (default), `json` (the serialized `TreeNode`), or `text` (flattened names). All text output is capped at 64 KiB.

**Screenshots** require a vision-capable model. With no `selector` or `region`, the full primary display is captured. `selector` captures an element's current bounds. `region` is `[x, y, width, height]` in logical screen pixels (signed for multi-monitor). PNG output is capped at 8 MiB. On Retina displays the captured pixels are physical resolution.

**Interaction.** `click` triggers the element's primary a11y action (invoke/press), which is the correct way to activate a native control. `type` types into a field after optionally clearing it; `submit: true` presses Enter after. `fill` sets several fields at once; each field is `{selector, value, type?, checked?}` where `type` is `text` (default), `select`, or `check`. `select` picks a value in a list/combo. All of these auto-wait for the element to be visible and enabled.

**Keyboard.** `press` types a global key chord (`Enter`, `cmd+a`, `ctrl+shift+t`). Recognized modifiers: `ctrl`/`control`, `alt`/`option`, `shift`, `cmd`/`meta`. Single keys: `Enter`, `Tab`, `Escape`, `Backspace`, `Delete`, `ArrowUp`/`ArrowDown`/`ArrowLeft`/`ArrowRight`, `Home`, `End`, `PageUp`, `PageDown`, `Space`, `F1`-`F12`, or a single character.

**Scrolling.** `scroll` moves by `amount` ticks in `direction` (`up`/`down`, default down). `to_top`/`to_top: false` jumps to the top/bottom. `selector` scrolls a specific element into view first.

**Waiting.** `wait` blocks until `selector` matches an element in the requested `state` (`visible` default, `attached`, `enabled`, `hidden`, `disabled`), capped by `timeout_ms` (default 10000, max 60000).

**Events.** `subscribe` starts an event stream on the active app (focus changes, value changes, window opens, etc.). `next_event` pulls one event from the stream with a timeout. Use `event_filter` to narrow by kind (`focus_changed`, `value_changed`, `window_opened`, `window_closed`, ...). This is the cheapest way to debug reactive UI without polling.

`connect`, `wait`, and `next_event` block the single desktop worker thread for their duration, so they serialize all other desktop actions. A long `timeout_ms` on one stalls any queued desktop call. The timeout is capped at 60 seconds.
