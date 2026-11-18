# Tools

Tools are the operations the agent can call while working on your prompt. This page groups the built-in tools by purpose; you do not need to invoke them manually. Tool calls use typed JSON arguments and return results to the model.

## Filesystem

- `read` — numbered lines, `offset`/`limit` paging, continuation notices.
- `grep` — regex or literal, ignore-aware, bounded scan budget.
- `edit` / `inplace_edit` / `multiedit` / `fuzzy_replace` / `apply_patch` — edit existing files, including fuzzy replacements, line edits, and multi-edit batches.
- `write`, `delete`, `move_file` — create or replace, delete, and move files, subject to [permissions](./permissions.md).
- `glob`, `list` — discovery.

Editing stages beside the destination and atomically replaces it, preserving permissions and bytes outside the replacement.

## Execution

- `bash` — run shell commands, with `bash_status`, `bash_watch`, and `bash_kill` for background tasks. Commands use sandbox profiles and tree-sitter parsing for [permission checks](./permissions.md). Set default timeouts in the `bash` configuration block ([Configuration](./configuration.md#tool-timeouts)).
- `batch` — parallel dispatch of independent tool calls under the parent's budgets.
- `code` — orchestrate tool calls from one Python script (codemode). Off by default; enable it with `--codemode`, `/codemode`, or the `codemode` configuration block.
- `worktree` — linked worktree creation for isolated mutations.

## Interaction and support

- `question` — structured user prompts during a run.
- `todo_write` — plan-mode todo cards.
- `view_image` — images as vision input.
- `webfetch` — fetch URLs with checks on private/loopback addresses and redirects to reduce server-side request forgery (SSRF) risks. Set timeout and retry defaults in the `webfetch` configuration block ([Configuration](./configuration.md#tool-timeouts)).
- `websearch` — web search.
- `sessions` — session listing.
- `skill` / `list_skills` — skill discovery and loading.
- `review` — run a read-only [code review](./review.md).
- `task` — delegate work to [subagents](./harness.md#subagents).
- `retrieve` — recover original content omitted by [context compression](./context.md).
- `list_tools` — discover available tools.

## Argosy knowledge

Linked in-process (no subprocess, no MCP transport): semantic `search` and `ask`, document/memory/rule reads and writes, and the review session toolset. See [Argosy](./argosy.md).

## MCP tools

MCP servers register as `server__tool` portable tools. See [MCP](./mcp.md).

## Paths and limits

Paths are workspace-relative or absolute beneath the canonical workspace root; `..`, `.git`, and symlink components are refused.

Text tools support UTF-8 files up to 8 MiB, including CRLF. Reads and searches bound excerpts to 64 KiB (individual lines to 2048 bytes) and report incomplete results rather than implying exhaustiveness.

Workspace checks are guardrails for a trusted local project, **not** an OS security sandbox; the host supplies approval hooks and [sandbox policy](./sandbox.md) before exposing the agent to untrusted workspaces.
