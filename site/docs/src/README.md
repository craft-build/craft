# Craft

**craft** is Craft's terminal coding agent: a TUI (or `--print` headless) harness written from scratch in Rust on top of the [animist](https://crates.io/crates/animist) provider/agent/history stack, with the `argosy` knowledge crate linked in-process.

It is designed to spend as few tokens as possible without giving up speed or ergonomics. Most coding agents burn context reactively — wait until the window is full, then summarize everything in one expensive pass. Craft keeps context lean continuously, so the model spends tokens on your work instead of on stale reads and verbose tool output:

- Tool output is pre-compressed before it ever enters history — code, logs, diffs, JSON arrays, and search results each get a tailored compressor.
- Stale and superseded file reads are replaced with compact markers before each turn.
- Deterministic (no-LLM) compaction kicks in at 60% of the context window; an LLM summarization pass only runs at 80% if the cheap stage wasn't enough.
- Compressed originals stay retrievable — durably, from the history entry itself — in any later turn or after a session reload.
- Subagents run in their own context windows, so delegation never pollutes the main session.

## What's in the box

- **A native terminal UI** (superlighttui): fast startup, 30 themes, command palette, fuzzy search, plan mode, subagent chat windows.
- **Subagents and parallel dispatch**: the `task` tool with model tiers and optional worktree isolation; `batch` fans children out concurrently.
- **MCP** servers over stdio and HTTP.
- **Skills and recipes**: reusable workflows as markdown skills, parameterized YAML/JSON blueprints.
- **An ACP server** (`craft acp`) for editor/IDE integration.
- **Permissions and snapshots**: tree-sitter bash parsing, allow/deny/ask rules, `/undo`, and an OS sandbox.
- **Argosy knowledge**: semantic search and ask, review sessions with P0–P3 findings, and post-turn memory extraction — in-process, local embeddings.

> This repository is mid-migration. The active agent lives in [`agent/`](https://github.com/craft-build/craft/tree/main/agent) (crate `craft`, binary `craft`); the root CraftCode app is being reworked and is not documented here yet. These docs cover the CLI harness only.

## Honesty note

A large share of the codebase was written by an AI, guided by humans. Being upfront about how software is made matters.

Ready to try it? Head to the [Quick Start](./quick-start.md).
