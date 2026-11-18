# Craft

Craft is a terminal coding agent. Run `craft` for an interactive terminal UI, or use `craft --print` to run a prompt from a script. It can read and edit files, run commands, search project knowledge, and delegate work to subagents.

It is designed to spend as few tokens as possible without giving up speed or ergonomics. Most coding agents burn context reactively — wait until the window is full, then summarize everything in one expensive pass. Craft keeps context lean continuously, so the model spends tokens on your work instead of on stale reads and verbose tool output:

- Tool output is pre-compressed before it ever enters history — code, logs, diffs, JSON arrays, and search results each get a tailored compressor.
- Stale and superseded file reads are replaced with compact markers before each turn.
- Deterministic (no-LLM) compaction kicks in at 60% of the context window; an LLM summarization pass only runs at 80% if the cheap stage wasn't enough.
- Compressed originals stay retrievable — durably, from the history entry itself — in any later turn or after a session reload.
- Subagents run in their own context windows, keeping their intermediate work out of the main session.

## What's in the box

- **An interactive terminal UI**: command palette, transcript search, themes, plan mode, and subagent chat windows.
- **Subagents and parallel dispatch**: the `task` tool with model tiers and optional worktree isolation; `batch` fans children out concurrently.
- **MCP** servers over stdio and HTTP.
- **Skills and recipes**: reusable workflows as markdown skills, parameterized YAML/JSON blueprints.
- **An ACP server** (`craft acp`) for editor/IDE integration.
- **Permissions and snapshots**: tree-sitter bash parsing, allow/deny/ask rules, `/undo`, and an OS sandbox.
- **Project knowledge**: semantic search, code review with prioritized findings, and memory that carries useful facts into later sessions. Argosy provides the knowledge layer, with local embeddings.

## Start here

- [Quick Start](./quick-start.md) — install Craft, connect a provider, and send your first prompt.
- [TUI](./tui.md) — everyday commands, shortcuts, and plan mode.
- [Configuration](./configuration.md) — change defaults and connect tools.
- [Permissions & Snapshots](./permissions.md) — control approvals and understand what `/undo` can restore.
- [Headless Mode](./headless.md) — run prompts from scripts and CI.

For implementation details, see [The Harness](./harness.md) and [Development](./development.md).

## Honesty note

A large share of the codebase was written by an AI, guided by humans. Being upfront about how software is made matters.
