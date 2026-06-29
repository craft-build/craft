Reads the visible text of a web page in headless Chromium and returns it as markdown. Use this to extract page content into the conversation for reading, searching, or summarizing without an image. Optionally scope extraction to a single element via a CSS selector.

The browser is launched once and reused across calls. Chromium must be installed on the system (Chrome or Chromium); it is auto-detected. Only `http` and `https` URLs are supported.

Inputs:
- `url` (required): the absolute http(s) URL of the page to read.
- `selector` (optional): CSS selector to scope extraction to the first matching element. When omitted, the text of `document.body` is returned.
- `width`, `height` (optional): viewport size in pixels. Defaults to 1280x720.
- `wait_ms` (optional): extra milliseconds to wait after navigation before reading, to let SPA hydration finish. Defaults to 1500.

Returned text is capped at 64 KiB with a `[truncated …]` marker when larger. For a screenshot use `browser_screenshot`; to also capture the URL and title use `browser_navigate`.
