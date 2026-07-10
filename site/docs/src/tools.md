# Tools

Craft ships with 45 built-in tools. This is the full reference.

## File Operations

### `bash` *(lua plugin)*

Execute a bash command.
Commands run in <cwd> by default.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `background` | boolean | no |  | Run in background, return task_id for later polling |
| `command` | string | yes |  | The bash command to execute |
| `description` | string | no |  | Short description (3-5 words) of what the command does |
| `timeout` | integer | no | 120 | Timeout in seconds |
| `workdir` | string | no | cwd | Working directory |

### `bash_kill` *(lua plugin)*

Terminate a background bash task.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `task_id` | string | yes | The task_id returned by bash |

### `bash_watch` *(lua plugin)*

Wait for a pattern (substring or Lua pattern) in a background bash task's output, or for the task to exit. Polls until match found, task exits, or timeout.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `pattern` | string | no |  | Substring or Lua pattern to wait for in task output |
| `task_id` | string | yes |  | The task_id returned by bash |
| `timeout` | integer | no | 60 | Max seconds to wait |

### `bash_status` *(lua plugin)*

Check status and current output of a background bash task.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `task_id` | string | yes | The task_id returned by bash |

### `read` *(native)*

Read a file or directory. Returns contents with line numbers (1-indexed).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `limit` | integer | no | Max number of lines to read. Omitting the limit reads up to 2000 lines. |
| `offset` | integer | no | Line number to start from (1-indexed) |
| `path` | string | yes |  |

### `write` *(native)*

Write content to a file, replacing existing content.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `content` | string | yes | The complete file content to write |
| `path` | string | yes |  |

### `edit` *(native)*

Replace an exact string match in a file.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `line_anchor_hash` | string | no |  | Optional 12-char hex content-hash anchor of the target line(s). When set, the applier verifies the hash against the current matched lines before writing; a stale anchor is rejected and the current content is returned so you can retry. Compute it by hashing the trim-normalized target lines (blank lines ignored). |
| `new_string` | string | yes |  | Replacement string |
| `occurrence` | integer | no |  | When multiple matches exist, select the Nth occurrence (1-indexed). Without this, multiple matches cause an error. |
| `old_string` | string | yes |  | Exact string to find (must match uniquely unless replace_all is true) |
| `path` | string | yes |  |  |
| `replace_all` | boolean | no | false | Replace all occurrences |

### `edit_lines` *(native, opt-in)*

Edit lines by number. Omit `end` to insert before `start` without removing lines. Set `end` to replace or delete (empty `new_string`) a range.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `end` | integer | no | Last line, inclusive. Omit to insert before start without removing lines. |
| `new_string` | string | yes | Replacement text. Empty deletes the range. |
| `path` | string | yes |  |
| `start` | integer | yes | First line (1-indexed) |

### `multiedit` *(native)*

Make multiple find-and-replace edits to a single file atomically.
Prefer this over edit when making multiple changes to the same file.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `edits` | array | yes | Array of edit operations to apply sequentially |
| `path` | string | yes |  |

### `apply_patch` *(native)*

Apply a Codex-style patch to one or more files.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `patch_text` | string | yes | Codex-style patch text with *** Begin Patch / *** End Patch markers |

### `delete` *(native)*

Delete files or directories. Text file contents are auto-backed up (use `safety undo` to recover).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `files` | array | yes | Files or directories to delete |
| `recursive` | boolean | no | Delete directories recursively (required for non-empty dirs) |

### `move` *(native)*

Move/rename a file or directory and update import references across the project.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `destination` | string | yes | Destination path |
| `source` | string | yes | Source file or directory path |

### `glob` *(lua plugin)*

Find files by glob pattern.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `path` | string | no | cwd | Directory to search in |
| `pattern` | string | no |  | Glob pattern (e.g. **/*.rs, src/**/*.ts) |

### `grep` *(lua plugin)*

Search file contents using regex.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `context_after` | integer | no |  | Context lines after match |
| `context_before` | integer | no |  | Context lines before match |
| `include` | string | no |  | File glob filter (e.g. *.c) |
| `limit` | integer | no |  | Max match groups to return |
| `path` | string | no | cwd | Directory to search in |
| `pattern` | string | yes |  | Regex pattern |

### `outline` *(native)*

Return a structural outline of a file or directory.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `files` | boolean | no | When path is a directory, return a flat file table instead of nested symbols |
| `path` | string | yes |  |

### `view_image` *(lua plugin)*

View an image file (png, jpeg, gif, webp) so you can actually see it; it is returned as vision input alongside the tool result. Use instead of `read` for images.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Path to the image file |

## Navigation & Analysis

### `zoom` *(native)*

Zoom into a specific symbol or line range in a file.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `context_lines` | integer | no | 3 | Lines of context around the symbol body |
| `end_line` | integer | no |  | End line (1-indexed) for line-range mode |
| `path` | string | yes |  |  |
| `start_line` | integer | no |  | Start line (1-indexed) for line-range mode |
| `symbol` | string | no |  | Symbol name to zoom into (function, struct, class, heading, etc.) |

### `ast_grep` *(native)*

Search and replace code using AST patterns. More precise than regex for code.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `apply` | boolean | no | dry-run, show diffs only | Apply replacement |
| `globs` | array | no |  | Glob patterns to include (e.g. ["*.rs", "src/**"]) |
| `lang` | string | yes |  | Language: rust, typescript, tsx, python, go |
| `path` | string | no | cwd | Directory or file to search |
| `pattern` | string | yes |  | AST pattern with $VAR and $$$BODY metavariables |
| `rewrite` | string | no |  | Replacement pattern (omitting = search-only mode). Uses $VAR refs from pattern. |

### `ast_edit` *(native)*

Propose a structural AST rewrite, then commit or discard it with `resolve`. Safer than a global `edit` when many call sites need the same change.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `globs` | array | no |  | Glob patterns to include (e.g. ["*.rs", "src/**"]) |
| `lang` | string | yes |  | Language: rust, typescript, tsx, python, go |
| `path` | string | no | cwd | Directory or file to search |
| `pattern` | string | yes |  | AST pattern with $VAR and $$$BODY metavariables |
| `rewrite` | string | yes |  | Replacement pattern, using $VAR refs from the pattern |

### `resolve` *(native)*

Commit or discard a pending `ast_edit` proposal by its `edit_id`.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `action` | string | yes | "apply" to commit the staged edit, or "discard" to drop it |
| `edit_id` | string | yes | edit_id returned by a prior ast_edit call |

### `callgraph` *(native)*

Intra-file call graph analysis. Traces function/method call relationships within a single file.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `depth` | integer | no | 5 | Max depth for call_tree |
| `op` | string | yes |  | Operation: call_tree, callers, or impact |
| `path` | string | yes |  | File path |
| `symbol` | string | yes |  | Symbol name (function/method/struct) |

### `inspect` *(native)*

Quick project health check. Scans for TODOs, FIXMEs, HACKs, and git status.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `scope` | string | no | cwd | File or directory to scope |
| `sections` | string | no | all | Sections: todos, git_status, or all |

### `conflicts` *(native)*

Find and resolve git merge conflicts in the project.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `index` | integer | no |  | Resolve only the Nth conflict (1-indexed) in each file. Omit to resolve all conflicts in scope. |
| `path` | string | no | cwd | Directory to scan |
| `resolve` | string | no |  | Resolve conflicts instead of listing. Values: "@theirs" (incoming/their branch), "@ours" (current/our branch), "@base" (remove both sides). Omit to list. |

## Safety

### `safety` *(native)*

Create and restore file-system checkpoints, undo file edits, and view backup history.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `action` | string | yes | Action: checkpoint, restore, list, undo, or history |
| `name` | string | no | Checkpoint name (for checkpoint and restore actions) |
| `path` | string | no | File path (for undo and history actions) |

## Execution & Control

### `batch` *(native)*

Executes multiple independent tool calls concurrently to reduce round-trips.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `tool_calls` | array | yes | Array of tool calls to execute in parallel |

### `code_execution` *(native)*

Execute Python code in a sandboxed interpreter with tools as callable functions.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `code` | string | yes |  | Python code to execute. Tools are async functions that return strings (not objects). You MUST await every call: `result = await read(path='/file')`. Use `await asyncio.gather(...)` for concurrency. |
| `timeout` | integer | no | 30, max 300 | Timeout in seconds |

### `question` *(lua plugin)*

Use this tool when you need to ask the user questions during execution. This allows you to:
- Gather user preferences or requirements
- Clarify ambiguous instructions
- Get decisions on implementation choices as you work
- Offer choices to the user about what direction to take

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `questions` | array | yes | List of questions to ask the user |

### `list_tools` *(native)*

List the tools available in this session, or enable and inspect a specific tool.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `detail` | string | no | Optional tool name to inspect. Returns the full input schema and enables the tool for the rest of the session. Omit to list every tool with a short description. |

### `retrieve` *(native)*

Retrieve the original (uncompressed) content for a previously compressed tool output. Use the hash value from a compression marker in the conversation. Compression markers appear as [N lines compressed from M. Retrieve original: hash=HASH] or in stale/superseded read markers that include a hash.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `hash` | string | yes | Hash of the compressed content to retrieve |

### `vcc_recall` *(native)*

Search the current session's full history (across compactions) losslessly. Supports regex queries, paging, and full-content expansion. Use to recall prior work, decisions, or context that was summarized away. Omit the query to browse recent entries.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `expand` | array | no | Entry indices to return full untruncated content for. Works alone or alongside a query. |
| `page` | integer | no | Page number (1-based) for paginated search results. Default: 1. |
| `query` | string | no | Search terms or regex pattern (e.g. 'auth\|login', 'fail.*build'). Multi-word queries are OR-ranked by relevance. Omit to browse recent history. |

### `todo_write` *(lua plugin)*

Track and update progress on multi-step tasks. Use this tool to plan and track tasks (must be 3+ steps). Update after EACH completed step, not only all at once. Each task needs an id (e.g. T1, T1.1), content, and status. Parent-child relationships are supported via the parent field.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `todos` | array | yes | List of tasks to track |

## Review & Findings

### `review` *(native)*

Spawn a code review subagent that reads files, checks against styleguide rules, and reports structured findings with priorities (P0-P3) and a verdict.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `focus_files` | array | no | Files to focus on (optional) |
| `task` | string | yes | What to review (e.g., 'Review the auth module for security issues') |

### `report_finding` *(native)*

Report a code review finding with priority, location, and optional rule references.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `body` | string | yes | Markdown body: what, why, rule, fix |
| `confidence` | number | yes | Confidence 0.0-1.0 |
| `file_path` | string | yes | Absolute file path |
| `line_end` | integer | yes | End line number |
| `line_start` | integer | yes | Start line number |
| `priority` | string | yes | Priority: P0, P1, P2, or P3 |
| `rule_ids` | array | no | Styleguide rule IDs |
| `suggestion` | string | no | Suggested fix or code snippet |
| `title` | string | yes | Imperative title, prefixed with priority (e.g. '[P1] Add error handling') |

### `read_findings` *(native)*

Retrieve detailed code review findings recorded by review subagents during this session. Use this when you need the original priority, file path, line numbers, body, suggested fix, and rule IDs after a review tool has finished.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `file_path_contains` | string | no |  | Optional substring match against file_path |
| `limit` | integer | no | 50 | Maximum findings to return |
| `priority` | string | no |  | Optional priority filter (P0, P1, P2, P3) |

## Styleguide

### `styleguide_list` *(native)*

List available styleguide categories for a language. Use this to discover what styleguides are available before fetching specific rules.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `language` | string | yes | Language to list styleguides for (e.g., 'rust', 'general') |

### `styleguide_search` *(native)*

Search for styleguide rules by keywords, rule IDs, or tags. Returns matching rules sorted by relevance.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `category` | string | no |  | Filter by category (e.g., 'naming'). Omit to search all. |
| `language` | string | no |  | Filter by language (e.g., 'rust'). Omit to search all. |
| `limit` | integer | no | 10 | Maximum results |
| `query` | string | yes |  | Search query — rule ID, keyword, or phrase |
| `tags` | array | no |  | Filter by tags. |

### `styleguide_get` *(native)*

Fetch specific styleguide rules or entire categories. Can fetch by category, rule IDs, or auto-detect from file path.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `category` | string | no | Category to fetch (e.g., 'naming'). Required unless using rule_ids or file_path. |
| `file_path` | string | no | File path to auto-detect language and get minimal context. |
| `language` | string | yes | Language code (e.g., 'rust', 'general') |
| `rule_ids` | array | no | Specific rule IDs to fetch. |

## Agent & Knowledge

### `task` *(native)*

Launch an autonomous subagent to perform tasks independently. Best combined with batch.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `context_mode` | string | no | Parent context to pass to the subagent:<br>- "none" (default): fresh, no parent history.<br>- "summary": last few parent messages for context.<br>- "full": full parent conversation history. |
| `description` | string | yes | Short (3-5 words) description of the task |
| `isolation` | string | no | Isolation mode for a general subagent:<br>- "none" (default): run in the current working tree.<br>- "worktree": run inside a fresh linked git worktree so file mutations do not touch the parent tree (sibling subagents cannot clobber each other). Requires a git repo; falls back to none otherwise. |
| `model_role` | string | no | Model role (optional, mutually exclusive with model_tier). When set, resolves the subagent's model from model_roles.toml by role name (e.g. "scout", "advisor"). Unset roles fall back to the current model. Cannot be combined with model_tier. |
| `model_tier` | string | no | Model tier (optional, omit to use current model, capped at current tier):<br>- "strong" (e.g. Opus): Deep reasoning, complex architecture, subtle bugs, most critical sections. ~5x cost of medium.<br>- "medium" (e.g. Sonnet): Balanced. Refactors, features, multi-file changes.<br>- "weak" (e.g. Haiku): Fast/cheap. Search, summarize, boilerplate, simple edits. |
| `output_schema` | string | no | Optional JSON Schema (object) describing the structured object the subagent must return as its final message. When set, the subagent is told to emit a final JSON object matching the schema; that object is validated and returned to you as structured data instead of prose. On validation failure the subagent is re-prompted (bounded), then a clean error is surfaced. |
| `prompt` | string | yes | Detailed task prompt for the agent |
| `subagent_type` | string | no | Subagent type: "research" (read-only, default) or "general" (can modify files) |

### `memory` *(lua plugin)*

Persistent, project-scoped scratchpad for learnings, patterns, decisions, and gotchas across sessions.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | yes | Command: view, write, delete |
| `content` | string | no | File content for 'write' |
| `path` | string | no | Relative path (e.g. 'architecture.md'). Omit to list all. |

### `skill` *(lua plugin)*

Load a skill that provides instructions and workflows for specific tasks.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | string | yes | Name of the skill to load |

### `flow_search` *(native)*

Search the current Flow workstream's persisted documents (goal, plan, requirements, QA, integration, verification) by semantic relevance to a natural-language query. Returns the top-k matching document paths with similarity scores.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `k` | integer | no | 5, max 20 | Maximum results to return |
| `query` | string | yes |  | Natural-language query describing what you need |

## Web & Desktop

### `browser` *(native)*

Drives a headless browser (Chromium via Playwright) so you can inspect frontends, fill forms, click elements, extract content, and run JavaScript. The browser session persists across calls: pages stay open, tabs are reused, and cookie/localStorage state carries over until you close a tab or the agent run ends.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `action` | string | yes |  | Browser action to perform |
| `amount` | integer | no |  | Scroll amount in pixels (for 'scroll' action, default: 500) |
| `clear` | boolean | no |  | Clear the field before typing (for 'type' action, default: true) |
| `direction` | string | no |  | Scroll direction 'up' or 'down' (for 'scroll' action, default: 'down') |
| `fields` | array | no |  | Form fields to fill (for 'fill_form' action). Array of {selector, value, type?, checked?} objects. |
| `filter` | string | no | 'all' | Filter for 'interactables': 'all', 'links', 'inputs', 'buttons' |
| `format` | string | no | 'text' | Content format for 'get_content': 'text', 'html', 'markdown', or 'title' |
| `full_page` | boolean | no | true, for 'screenshot' action | Full-page screenshot |
| `height` | integer | no | 720 | Viewport height in pixels |
| `key` | string | no |  | Key chord to press, e.g. 'Enter', 'Tab', 'Escape', 'ctrl+shift+t' (for 'press' action) |
| `region` | array | no |  | Screenshot region [x, y, width, height] in CSS pixels (for 'screenshot' action) |
| `script` | string | no |  | JavaScript to execute in the page context (for 'eval' action) |
| `selector` | string | no |  | CSS selector of the element to interact with. Used by click, type, select, scroll, screenshot, wait. |
| `submit` | boolean | no |  | Press Enter after typing (for 'type' action, default: false) |
| `submit_selector` | string | no |  | Submit button CSS selector (for 'fill_form' when submit=true) |
| `tab` | integer | no | active tab | Tab index to operate on |
| `text` | string | no |  | Text to type into the element (for 'type' action) |
| `timeout_ms` | integer | no | 10000 | Timeout in milliseconds for 'wait' action |
| `to_top` | boolean | no |  | Scroll to top (true) or bottom (false) of page (for 'scroll' action) |
| `url` | string | no |  | Absolute http(s) URL to navigate to. Required for 'open', optional for most others (navigates first if provided). |
| `value` | string | no |  | Value to select in a dropdown (for 'select' action) |
| `visible` | boolean | no |  | Wait for element to be visible, not just present (for 'wait' action, default: true) |
| `wait_ms` | integer | no | 1500 | Extra milliseconds to wait after navigation |
| `width` | integer | no | 1280 | Viewport width in pixels |

### `desktop` *(native)*

Drives native desktop applications through the platform accessibility tree (AXUIElement on macOS, AT-SPI2 on Linux, UI Automation on Windows). It is the desktop counterpart to the `browser` tool: where `browser` drives Chromium via Playwright, `desktop` drives real apps, including Tauri/webview apps whose content the OS exposes as an ARIA-mapped tree.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `action` | string | yes |  | Desktop action to perform |
| `amount` | integer | no | 3 | Scroll amount in ticks. |
| `app` | string | no |  | App name (for 'connect'). One of 'app' or 'pid' is required for connect. |
| `clear` | boolean | no |  | Clear the field before typing (for 'type', default true). |
| `direction` | string | no | 'down' | Scroll direction 'up' or 'down'. |
| `event_filter` | string | no |  | Optional EventKind name filter for 'next_event' (e.g. 'focus_changed'). |
| `fields` | array | no |  | Fields to fill (for 'fill'). Array of {selector, value, type?, checked?} objects. |
| `format` | string | no |  | Format for 'read': 'tree' (default), 'json', or 'text'. |
| `key` | string | no |  | Key chord for 'press' ('Enter', 'cmd+a', ...). |
| `limit` | integer | no |  | Limit number of results for 'find'. |
| `max_depth` | integer | no | 4 | Max tree depth for 'tree'/'dump'/'read'. |
| `pid` | integer | no |  | Process id alternative to 'app' for 'connect'. |
| `region` | array | no |  | Region [x, y, width, height] in logical screen pixels (for 'screenshot'). |
| `selector` | string | no |  | xa11y CSS-like selector (e.g. button[name='OK']). Used by find/click/type/fill/scroll/wait/select/read/screenshot(element). |
| `state` | string | no | visible | Desired state for 'wait': visible/attached/enabled/hidden/disabled. |
| `submit` | boolean | no | false | Press Enter after typing/filling. |
| `text` | string | no |  | Text to type ('type') or value to set ('select'/'fill'). |
| `timeout_ms` | integer | no | 10000, max 60000 | Timeout in ms for connect/wait/next_event. |
| `to_top` | boolean | no |  | Scroll to top (true) or bottom (false) of content. |
| `value` | string | no |  | Value for 'select'. |

### `webfetch` *(lua plugin)*

Fetch a URL and return its contents.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `format` | string | no |  | Output format: markdown (default), text, or html |
| `timeout` | integer | no | 30, max 120 | Timeout in seconds |
| `url` | string | yes |  | URL to fetch (http:// or https://) |

### `websearch` *(lua plugin)*

Search the web for real-time information using Exa AI.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `num_results` | integer | no | 8 | Number of results to return |
| `query` | string | yes |  | Search query |