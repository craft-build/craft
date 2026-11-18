# Tools

All tools use strict typed JSON arguments and produce literal text for the model.

## Filesystem

- `read` — numbered lines, `offset`/`limit` paging, continuation notices.
- `grep` — regex or literal, ignore-aware, bounded scan budget.
- `edit` / `inplace_edit` / `multiedit` / `fuzzy_replace` / `apply_patch` — replace-based editing with fuzzy matching; `edit_lines`-style line edits and multi-edit batches ride these.
- `write`, `delete`, `move_file` — the canonical mutations (`move_file` is the sanctioned move; see [permissions](./permissions.md)).
- `glob`, `list` — discovery.

Editing stages beside the destination and atomically replaces it, preserving permissions and bytes outside the replacement.

## Execution

- `bash` — with `bash_status`, `bash_watch`, `bash_kill` for background tasks; sandbox profiles; tree-sitter command parsing for [permission scoping](./permissions.md).
- `batch` — parallel dispatch of independent tool calls under the parent's budgets.
- `worktree` — linked worktree creation for isolated mutations.

## Interaction and support

- `question` — structured user prompts during a run.
- `todo_write` — plan-mode todo cards.
- `view_image` — images as vision input.
- `webfetch` — SSRF-guarded URL fetching (private/loopback IP and redirect checks).
- `websearch` — web search.
- `sessions` — session listing.
- `skill` / `list_skills` — skill discovery and loading.
- `review`, `task`, `retrieve`, `list_tools` — see [Review](./review.md), [The Harness](./harness.md), and [Context Management](./context.md).

## Argosy knowledge

Linked in-process (no subprocess, no MCP transport): semantic `search` and `ask`, document/memory/rule reads and writes, and the review session toolset. See [Argosy](./argosy.md).

## MCP tools

MCP servers register as `server__tool` portable tools. See [MCP](./mcp.md).

## Paths and limits

Paths are workspace-relative or absolute beneath the canonical workspace root; `..`, `.git`, and symlink components are refused.

Text tools support UTF-8 files up to 8 MiB, including CRLF. Reads and searches bound excerpts to 64 KiB (individual lines to 2048 bytes) and report incomplete results rather than implying exhaustiveness.

Workspace checks are guardrails for a trusted local project, **not** an OS security sandbox; the host supplies approval hooks and [sandbox policy](./sandbox.md) before exposing the agent to untrusted workspaces.
