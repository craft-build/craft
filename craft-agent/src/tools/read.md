Read a file or directory. Returns contents with line numbers (1-indexed).

Internal URL schemes (resolved through this one interface, no extra tools):
- `skill://<name>`: read a discovered skill body (e.g. `read skill://audit`).
- `rule://<name>`: read a discovered rule body from `.craft/rules/<name>.md`. `rule://*` lists all.
- `conflict://N`: read the Nth (1-indexed) merge-conflict hunk in the repo, numbered globally.
- `conflict://*`: list every conflict hunk across the repo with its global index.
- `agent://findings`: list structured review findings recorded by subagents.
- `agent://findings.<i>`: read the ith finding in full (title, file, body).
- `flow://<path>`: read a Flow workstream document (the `path` returned by `flow_search`). `flow://*` lists every document in the active workstream.
Use these to cut down the number of tools you reach for.

- Supports absolute, relative, and ~/ paths.
- Image files (png, jpg, jpeg, gif, webp) are returned inline; offset/limit do not apply to them.
- **offset** and **limit** are required. Use offset=1 to read from the first line.
- Use limit=0 to read until the end of file (capped at 2000 lines).
- Use the **outline** tool or **grep** tool first to find the offset and limit.
- Only read the sections you actually need.
- Use `wc -l` to check total number of lines before reading to decide a reasonable limit.
- Use truncation hints (e.g. "truncated lines X-Y") to continue with the correct offset.
- Do not reread the same range (same file and same offset).
- Prefer grep to locate content instead of scanning full files.
- Call in parallel when reading multiple files.
- Avoid tiny repeated slices - read a larger window if you need more context.
