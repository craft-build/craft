Navigates a headless Chromium browser to a URL, clicks the first element matching a CSS selector, then returns either the resulting page text or a screenshot. Use this to drive interactions: open a menu, follow a login link, trigger a tab switch.

The browser is launched once and reused across calls. Chromium must be installed on the system (Chrome or Chromium); it is auto-detected. Only `http` and `https` URLs are supported.

Inputs:
- `url` (required): the absolute http(s) URL to navigate to before clicking.
- `selector` (required): CSS selector of the element to click; the first match is clicked.
- `as` (optional): what to return after clicking, `"text"` or `"screenshot"`. Defaults to `"text"`.
- `width`, `height` (optional): viewport size in pixels. Defaults to 1280x720.
- `wait_ms` (optional): extra milliseconds to wait after clicking before reading/capturing. Defaults to 1500.
- `full_page` (optional): capture the full scrollable page when `as="screenshot"` (default true).

If the selector matches nothing, the tool errors with a hint to inspect the DOM via `browser_screenshot`. Returned text is capped at 64 KiB with a `[truncated …]` marker when larger.
