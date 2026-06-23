# Tools

Craft ships with 37 built-in tools. This is the full reference.

## File Operations

### `bash` *(lua plugin)*

Execute a bash command.
Commands run in

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `background` | boolean | no |  | Run in background, return task_id for later polling |
| `command` | string | yes |  | The bash command to execute |
| `description` | string | no |  | Short description (3-5 words) of what the command does |
| `timeout` | integer | no | 120 | Timeout in seconds |
| `workdir` | string | no | cwd | Working directory |

### `read`

Read a file or directory. Returns contents with line numbers (1-indexed).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `limit` | integer | no | Max number of lines to read. Omitting the limit reads up to 2000 lines. |
| `offset` | integer | no | Line number to start from (1-indexed) |
| `path` | string | yes |  |

### `write`

Write content to a file, replacing existing content.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `content` | string | yes | The complete file content to write |
| `path` | string | yes |  |

### `edit`

Replace an exact string match in a file.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `new_string` | string | yes |  | Replacement string |
| `occurrence` | integer | no |  | When multiple matches exist, select the Nth occurrence (1-indexed). Without this, multiple matches cause an error. |
| `old_string` | string | yes |  | Exact string to find (must match uniquely unless replace_all is true) |
| `path` | string | yes |  |  |
| `replace_all` | boolean | no | false | Replace all occurrences |

### `multiedit`

Make multiple find-and-replace edits to a single file atomically.
Prefer this over edit when making multiple changes to the same file.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `edits` | array | yes | Array of edit operations to apply sequentially |
| `path` | string | yes |  |

### `apply_patch`

Apply a Codex-style patch to one or more files.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `patch_text` | string | yes | Codex-style patch text with *** Begin Patch / *** End Patch markers |

### `delete`

Delete files or directories. Text file contents are auto-backed up (use `safety undo` to recover).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `files` | array | yes | Files or directories to delete |
| `recursive` | boolean | no | Delete directories recursively (required for non-empty dirs) |

### `move`

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

### `outline`

Return a structural outline of a file or directory.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `files` | boolean | no | When path is a directory, return a flat file table instead of nested symbols |
| `path` | string | yes |  |

## Navigation & Analysis

### `zoom`

Zoom into a specific symbol or line range in a file.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `context_lines` | integer | no | 3 | Lines of context around the symbol body |
| `end_line` | integer | no |  | End line (1-indexed) for line-range mode |
| `path` | string | yes |  |  |
| `start_line` | integer | no |  | Start line (1-indexed) for line-range mode |
| `symbol` | string | no |  | Symbol name to zoom into (function, struct, class, heading, etc.) |

### `ast_grep`

Search and replace code using AST patterns. More precise than regex for code.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `apply` | boolean | no | dry-run, show diffs only | Apply replacement |
| `globs` | array | no |  | Glob patterns to include (e.g. ["*.rs", "src/**"]) |
| `lang` | string | yes |  | Language: rust, typescript, tsx, python, go |
| `path` | string | no | cwd | Directory or file to search |
| `pattern` | string | yes |  | AST pattern with $VAR and $$$BODY metavariables |
| `rewrite` | string | no |  | Replacement pattern (omitting = search-only mode). Uses $VAR refs from pattern. |

### `callgraph`

Intra-file call graph analysis. Traces function/method call relationships within a single file.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `depth` | integer | no | 5 | Max depth for call_tree |
| `op` | string | yes |  | Operation: call_tree, callers, or impact |
| `path` | string | yes |  | File path |
| `symbol` | string | yes |  | Symbol name (function/method/struct) |

### `inspect`

Quick project health check. Scans for TODOs, FIXMEs, HACKs, and git status.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `scope` | string | no | cwd | File or directory to scope |
| `sections` | string | no | all | Sections: todos, git_status, or all |

### `conflicts`

Find git merge conflicts in the project.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `path` | string | no | cwd | Directory to scan |

## Safety

### `safety`

Create and restore file-system checkpoints, undo file edits, and view backup history.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `action` | string | yes | Action: checkpoint, restore, list, undo, or history |
| `name` | string | no | Checkpoint name (for checkpoint and restore actions) |
| `path` | string | no | File path (for undo and history actions) |

## Execution & Control

### `batch`

Executes multiple independent tool calls concurrently to reduce round-trips.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `tool_calls` | array | yes | Array of tool calls to execute in parallel |

### `code_execution`

Execute Python code in a sandboxed interpreter. Tools are available as callable functions.

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

### `list_tools`

List the tools available in this session, or enable and inspect a specific tool.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `detail` | string | no | Optional tool name to inspect. Returns the full input schema and enables the tool for the rest of the session. Omit to list every tool with a short description. |

### `retrieve`

Retrieve the original (uncompressed) content for a previously compressed tool output. Use the hash value from a compression marker in the conversation. Compression markers appear as [N lines compressed from M. Retrieve original: hash=HASH] or in stale/superseded read markers that include a hash.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `hash` | string | yes | Hash of the compressed content to retrieve |

## Review & Findings

### `review`

Spawn a code review subagent that reads files, checks against styleguide rules, and reports structured findings with priorities (P0-P3) and a verdict.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `focus_files` | array | no | Files to focus on (optional) |
| `task` | string | yes | What to review (e.g., 'Review the auth module for security issues') |

### `report_finding`

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

### `read_findings`

Retrieve detailed code review findings recorded by review subagents during this session. Use this when you need the original priority, file path, line numbers, body, suggested fix, and rule IDs after a review tool has finished.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `file_path_contains` | string | no |  | Optional substring match against file_path |
| `limit` | integer | no | 50 | Maximum findings to return |
| `priority` | string | no |  | Optional priority filter (P0, P1, P2, P3) |

## Styleguide

### `styleguide_list`

List available styleguide categories for a language. Use this to discover what styleguides are available before fetching specific rules.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `language` | string | yes | Language to list styleguides for (e.g., 'rust', 'general') |

### `styleguide_search`

Search for styleguide rules by keywords, rule IDs, or tags. Returns matching rules sorted by relevance.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `category` | string | no |  | Filter by category (e.g., 'naming'). Omit to search all. |
| `language` | string | no |  | Filter by language (e.g., 'rust'). Omit to search all. |
| `limit` | integer | no | 10 | Maximum results |
| `query` | string | yes |  | Search query — rule ID, keyword, or phrase |
| `tags` | array | no |  | Filter by tags. |

### `styleguide_get`

Fetch specific styleguide rules or entire categories. Can fetch by category, rule IDs, or auto-detect from file path.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `category` | string | no | Category to fetch (e.g., 'naming'). Required unless using rule_ids or file_path. |
| `file_path` | string | no | File path to auto-detect language and get minimal context. |
| `language` | string | yes | Language code (e.g., 'rust', 'general') |
| `rule_ids` | array | no | Specific rule IDs to fetch. |

## Agent & Knowledge

### `task`

Launch an autonomous subagent to perform tasks independently. Best combined with batch.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `context_mode` | string | no | Parent context to pass to the subagent:<br>- "none" (default): fresh, no parent history.<br>- "summary": last few parent messages for context.<br>- "full": full parent conversation history. |
| `description` | string | yes | Short (3-5 words) description of the task |
| `model_tier` | string | no | Model tier (optional, omit to use current model, capped at current tier):<br>- "strong" (e.g. Opus): Deep reasoning, complex architecture, subtle bugs, most critical sections. ~5x cost of medium.<br>- "medium" (e.g. Sonnet): Balanced. Refactors, features, multi-file changes.<br>- "weak" (e.g. Haiku): Fast/cheap. Search, summarize, boilerplate, simple edits. |
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

## Web

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