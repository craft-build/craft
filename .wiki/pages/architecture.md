---
type: Reference
description: Crate responsibilities, request data flow, concurrency model, and integration boundaries.
tags:
- architecture
- subsystems
- data-flow
- async
- tokio
timestamp: 2026-07-11T20:26:57Z
---
# Architecture

Craft is a modular Rust workspace. A main binary dispatches subcommands; the TUI drives an async agent loop that talks to LLM providers, runs tools (including a Python sandbox and Lua plugins), and persists state.

## Crate responsibilities

| Crate | Responsibility |
|---|---|
| craft-agent | Async agent loop, tool dispatch, state machine, history compaction, snapshots, skills, semantic search (`craft-agent/src/lib.rs`) |
| craft-ui | Ratatui TUI with Elm-like model-update-view architecture (`craft-ui/src/app/mod.rs`) |
| craft-providers | LLM provider integrations: Anthropic, OpenAI, Google, Bedrock, custom/dynamic. Streaming + token usage (`craft-providers/src/provider.rs`) |
| craft-storage | Persistent state: sessions (JSONL), auth, cost ledger, wiki, flow docs, rotating logs (`craft-storage/src/lib.rs`) |
| craft-config | TOML config (global + project), permissions, provider defs, model tiers, UI settings (`craft-config/src/lib.rs`) |
| craft-interpreter | Python sandbox via monty for `code_execution`, with resource limits and streaming output (`craft-interpreter/src/runner.rs`) |
| craft-lua | Lua plugin runtime mirroring neovim API; built-in plugins; tree-sitter integration (`craft-lua/src/runtime.rs`) |
| craft-acp | Agent Client Protocol server over stdio; exposes Fs/Terminal backends; MCP translation (`craft-acp/src/server.rs`) |
| craft-flow | Multi-stage workstream pipelines with approval gates |
| craft-tool-macro | Derive macro generating tool JSON schemas for craft-agent |
| craft-config-macro | Proc macro used by craft-config |
| craft-markdown | Theme-free markdown parser and width-aware renderer (shared by UI and Lua) |
| craft-highlight | Thin wrapper around syntect/two-face |
| craft-docgen | Generates user docs in `site/docs/src` from workspace metadata |
| craft-sandbox | Sandbox execution environment for tools |

## Data flow: TUI request

1. User types prompt; `App::update()` processes the key event (`craft-ui/src/app/mod.rs:237`).
2. Submission sends `AgentCommand::UserMessage` over a `flume::Sender<AgentCommand>` (`craft-ui/src/agent/mod.rs:36`).
3. `AgentLoop::process_entry()` spawns a tokio task running `Agent::run()` (`craft-ui/src/agent/agent_loop.rs:57`).
4. `Agent::turn()` calls `provider.stream_message()` with tool descriptions (`craft-agent/src/agent/run.rs:425`).
5. Provider SSE events are converted to `AgentEvent` via `forward_provider_events()` (`craft-agent/src/agent/streaming.rs:15`).
6. Tool calls dispatched through `tool_dispatch::run()` (`craft-agent/src/agent/tool_dispatch.rs:72`).
7. Tool results return through `EventSender`; UI renders via `Chat::handle_event()` (`craft-ui/src/chat.rs:62`).

## Headless / SDK path

`cmd::dispatch()` routes to `headless::run_headless()` or SDK mode, feeding input directly to `Agent::run()` without the UI layer (`src/cmd/mod.rs:15`, `src/cmd/headless.rs:58`). Results print via `src/print.rs`.

## Cross-cutting concerns

- **Logging**: structured `tracing` with a rotating file writer (`craft-storage/src/log.rs:27`); initialized in `setup::init_logging()` (`src/setup.rs:79`).
- **Error handling**: `Result<T, E>` throughout; `color_eyre` for generic errors, `thiserror` for domain errors in craft-providers (`craft-providers/src/error.rs:6`).
- **Config**: global + project TOML, merge semantics in `Config::load()` (`craft-config/src/lib.rs:1519`); permission rules enforced by `PermissionManager` (`craft-agent/src/permissions.rs`).
- **Storage**: `StateDir` over XDG paths, atomic writes, versioned JSONL sessions (`craft-storage/src/lib.rs:33`); cost ledger (`craft-storage/src/stats.rs:54`).
- **Compaction**: token-aware history compaction via `compact_history()` with caching and aggressive mode for long sessions (`craft-agent/src/agent/compaction.rs:72`).

## Async and concurrency

- Tokio multi-threaded runtime (`#[tokio::main]`, `src/main.rs:12`). UI event loop on main thread; agent tasks spawned on tokio.
- `flume` MPMC channels for UI <-> agent communication, and for Lua plugin semantic-search requests (`craft-lua/src/api/embed.rs:4`).
- `tokio::spawn` for agent turns, tool execution, background jobs.
- `CancelMap` + `CancelToken` for run-scoped cancellation (`craft-agent/src/cancel.rs:15`); `InterruptSource` trait for event-driven stops.
- `InflightGate` limits concurrent async tools in the Lua runtime (`craft-lua/src/runtime.rs:470`).

## External integration points

- **LLM APIs**: reqwest HTTP; provider trait with `stream_message()` (`craft-providers/src/provider.rs:270`). Anthropic via SSE streaming; Bedrock via Converse API (`craft-providers/src/providers/bedrock/mod.rs:237`).
- **ACP server**: JSON-RPC over stdio with `FsBackend` / `TerminalBackend`; MCP server config translation (`craft-acp/src/server.rs:364`, `craft-acp/src/mcp.rs:13`).
- **Lua plugins**: mlua runtime, `PluginHost`, tool registration via `register_tool()` (`craft-lua/src/runtime.rs:634`). Built-in plugins in `./plugins`.
- **Python sandbox**: monty interpreter, `run()` / `run_streaming()` with memory/recursion/timeout limits (`craft-interpreter/src/runner.rs:71`, `craft-interpreter/src/runner.rs:84`).
- **Flow pipeline**: `FlowStore` persistence (`craft-storage/src/flow.rs:46`); chunked execution with approval gates.
- **Semantic search**: `EmbeddingService` with cosine similarity (`craft-agent/src/agent/semantic.rs:24`).