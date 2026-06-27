Renders a web page in headless Chromium and returns a full-page PNG screenshot so you can visually inspect the current state of a frontend. Use this for visual feedback when working on UI/CSS/layout, verifying a dev server, or checking what a page actually looks like.

The screenshot is sent to the model as an image tool result, so this tool requires a vision-capable model. Only `http` and `https` URLs are supported.

The browser is launched once and reused across calls, so the second screenshot is fast. Chromium must be installed on the system (Chrome or Chromium); it is auto-detected.

Inputs:
- `url` (required): the absolute http(s) URL to render.
- `width`, `height` (optional): viewport size in pixels. Defaults to 1280x720. Useful for testing responsive layouts.
- `wait_ms` (optional): extra milliseconds to wait after navigation before capturing, to let SPA hydration finish. Defaults to 1500.
- `full_page` (optional): capture the entire scrollable page (default true). Set to false for just the viewport.

The returned image is the tool output. If you need the page's text content or HTML instead of an image, this tool does not provide that, consider a different approach.
