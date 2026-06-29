Navigates a headless Chromium browser to a URL and returns the page's URL, title, and visible text as markdown. Use this to confirm a route loaded, capture the document title, or read page text without taking a screenshot.

The browser is launched once and reused across calls, so repeated navigation is fast. Chromium must be installed on the system (Chrome or Chromium); it is auto-detected. Only `http` and `https` URLs are supported.

Inputs:
- `url` (required): the absolute http(s) URL to navigate to.
- `width`, `height` (optional): viewport size in pixels. Defaults to 1280x720.
- `wait_ms` (optional): extra milliseconds to wait after navigation before reading, to let SPA hydration finish. Defaults to 1500.

The returned text is the page's `document.body.innerText`, capped at 64 KiB with a `[truncated …]` marker when larger. For visual inspection use `browser_screenshot`; to scope text to one element use `browser_text`.
