<img src="./banner.png">

**Craft** is an AI coding agent written in Rust, designed to spend as few tokens as possible without giving up speed or ergonomics. Native terminal UI, subagents, MCP, and a multi-stage context-management pipeline that keeps the model working on your task instead of wading through stale reads and verbose tool output.

> This repository is mid-migration. The active agent lives in [`agent/`](./agent) (crate `craft`, binary `crafty`); the root CraftCode app is being reworked and is not documented here yet.

## Why craft

Most coding agents burn context reactively: wait until the window is full, then summarize everything in one expensive pass. Craft keeps context lean continuously, so the model spends tokens on your work instead of on duplicated history:

- Tool output is pre-compressed before it ever enters history (code, logs, diffs, JSON arrays, search results each get a tailored compressor).
- Stale and superseded file reads are replaced with compact markers before each turn.
- Deterministic (no-LLM) compaction kicks in at 60% of the context window; an LLM summarization pass only runs at 80% if the cheap stage wasn't enough.
- Read-only tool results are deduplicated by argument hash, and compressed originals stay retrievable — durably, from the history entry itself — in any later turn or after a session reload.
- Subagents run in their own context windows, so delegation never pollutes the main session.

## Features

### Context management

- **Tool output pre-compression** - heuristic content-type detection (code, log, search result, diff, JSON array, plain text) routes each tool result to a dedicated compressor. Log and code compression use an aho-corasick keyword classifier (error, warning, security, definition, import, ...).
- **Read lifecycle** - fresh reads go stale as the conversation moves on; stale and superseded reads are collapsed into durable marker revisions, with the verbatim originals kept in the entry source for `retrieve`.
- **Reversible compression** - originals live in history entry sources, so a compressed marker survives session reloads and `retrieve` restores the full bytes at any later turn.
- **Two-stage compaction** - `vcc` (deterministic, no LLM) at 60% of `context_length`, then `llm` (one summarization call) at 0.8. Stages, kinds, and thresholds are configurable via `[[compaction]]` blocks. Effectiveness gating skips stages that wouldn't save ≥10% so compaction never cycles.
- **Overflow ladder** - when a request still overflows, tool results collapse progressively (10%/20%/50%/100%) before anything harder is attempted.
- **Tool dedup cache** - repeated read-only tool calls with identical arguments return cached output, invalidated structurally when files change.

### Knowledge (argosy, in-process)

The `argosy` crate is linked in-process (no subprocess, no MCP transport): semantic `search`, `ask`, document/memory reads and writes, skills, and a review session toolset (`start_review`, `review_diff`, `report_finding`, `review_findings`) all register as first-class native tools. Argosy keeps its own state keyed by project root, embeddings run locally (ONNX), and project-stored skills join the `skill` tool's discovery.

- **Reviewer subagent** - `/review` or the `review` tool snapshots the diff and returns P0-P3 findings with a verdict.
- **Post-turn memory extraction** - durable facts from a turn are written into the project's local argosy memory (on by default).

### Reliability and guardrails

- Streaming retries with backoff, API-key rotation, and a model fallback chain.
- Doom-loop detection with grace and hard-stop thresholds.
- Per-tool guardrails that demote or drop tools with decaying failure counters.
- **Permissions** - bash commands are parsed with tree-sitter so `git diff && rm -rf /` requests `git *` and `rm *` approval separately. Rules (allow/deny/ask) live in the BML config. Escape hatch: `--yolo`.
- **Auto-review mode** - an LLM reviewer answers permission prompts, fail-closed on low confidence.
- **Snapshots and `/undo`** - files are snapshotted before writes and edits (first-wins, ≤5 MiB, text only).
- SSRF protection on `webfetch` (private/loopback IP and redirect checks).
- Sandbox profiles for bash commands.

### Experience

- Native terminal UI (superlighttui): fast startup, high frame rate, no JavaScript.
- 30 built-in themes (`/theme`), rebindable keymaps.
- Fuzzy search (Ctrl-F), command palette (Ctrl-P), subagent chat windows (Ctrl-N / Ctrl-P, `/tasks` view), clipboard image paste (Ctrl-V), editor integration (Ctrl-O / Alt+O).
- Plan mode (Ctrl-T) with a plan form and todo cards.
- `!` and `!!` to run bash with or without the agent seeing it.
- Session resume (`-c` / `-s`), input history, crash-safe append-only JSONL sessions.
- Slash commands: `/new`, `/resume`, `/sessions`, `/model`, `/clear`, `/undo`, `/mcp`, `/compact`, `/usage`, `/stats`, `/theme`, `/recipe`, `/review`, `/scan`, `/dream`, `/memory`, and custom namespaced commands.
- Skills (bundled + filesystem, with shadowing precedence) and MCP servers over stdio and HTTP.
- Recipes - parameterized YAML/JSON blueprints rendered with minijinja, run via `recipe run`.
- Shell integration (`craft term init`): logs shell history, `@craft` queries, command-not-found handler.
- Headless runs: `--print --output-format stream-json` (Claude Code-compatible) or plain text.
- ACP server (`craft acp`) for editor/IDE integration.

### Subagents

The `task` tool spawns child agents with their own context windows and cancel tokens, a restricted tool table, model tiers (weak/medium/strong, capped at the parent's tier), optional worktree isolation, and optional JSON-schema-validated output.

## Supported providers

Nine kinds, configured by alias in the BML config with per-alias credentials, base URLs, and model catalogs: `anthropic`, `openai` (Responses, or Chat Completions via `protocol`), `openai-compatible` (any OpenAI-dialect endpoint; keyless local servers ride the keyless llama.cpp client), `synthetic`, `zai-coding-plan`, `ollama`, `llama-cpp`, `omlx`, and `amazon-bedrock` (AWS credential chain; per-model wire protocol — Anthropic Messages, Responses, Chat Completions, or Converse). The predecessor's other vendor kinds (`azure`, `chatgpt`, `cohere`, `copilot`, `deepseek`, `gemini`, `groq`, `openrouter`, `xai`, `zai`, …) are **dropped**: configs naming them fail with a migration hint pointing at `openai-compatible` + `base_url`. Full tables and migration snippets: [`agent/README.md`](./agent/README.md).

Model discovery merges configured models with live provider listings and the [models.dev](https://models.dev) catalog (disk-cached 24 h) for context-window and pricing metadata.

## Configuration

Configuration lives in `~/.config/craft.bml` (or split files under `~/.config/craft/*.bml`) in BML format. `api_key_env` names an environment variable, never the key itself. On first run craft auto-detects providers from credential environment variables. See [`agent/agent.example.bml`](./agent/agent.example.bml).

## Building

```sh
cargo build -p craft --release
```

Subcommands of note: `craft models`, `craft stats`, `craft doctor`, `craft update` / `rollback`, `craft prompt`, `craft recipe`, `craft term`, `craft acp`, `craft completions <shell>`.

Craft is written from scratch in Rust, on top of the [animist](https://crates.io/crates/animist) provider/agent/history stack (async end to end), with an in-process argosy knowledge layer, a durable history compression pipeline, and an ACP server.

> Honesty note: a large share of the codebase was written by an AI, guided by humans. Being upfront about how software is made matters.
