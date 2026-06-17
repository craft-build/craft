Zoom into a specific symbol or line range in a file.

- `symbol`: the name of a function, struct, class, heading, etc. Returns the full body with line numbers and optional context.
- `start_line`/`end_line`: 1-indexed line range for when you don't know the symbol name.
- `context_lines`: surrounding lines of context (default 3).
- Ambiguous symbol names (multiple matches) return disambiguation candidates.
- For Markdown/HTML, extracts section content under a heading.
- Prefer this over `read` when you need the body of a specific symbol without reading the whole file.
