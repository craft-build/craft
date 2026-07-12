---
type: Reference
description: Rust edition, workspace crates, key dependencies, and entry points for Craft.
tags:
- rust
- workspace
- dependencies
- tokio
- ratatui
- lua
timestamp: 2026-07-11T20:26:57Z
---
# Tech Stack

Craft is a Rust workspace for an AI coding agent, built around a tokio async core, a ratatui terminal UI, a Lua plugin runtime, and a Python sandbox.

## Languages and toolchain

- **Rust**, edition 2024, MSRV 1.89 (`Cargo.toml:4`, `Cargo.toml:72`)
- **Lua** (Luau dialect via mlua) for built-in plugins in `./plugins`
- **Python** (via monty/pydantic sandbox) for the `code_execution` tool

## Workspace members (`Cargo.toml:51-67`)

craft-acp, craft-agent, craft-config, craft-config-macro, craft-docgen, craft-flow, craft-highlight, craft-interpreter, craft-lua, craft-markdown, craft-providers, craft-sandbox, craft-storage, craft-tool-macro, craft-ui

## Key dependencies (`Cargo.toml:78-214`)

| Dependency | Purpose |
|---|---|
| tokio (v1, full) | Async runtime |
| ratatui (v0.30) | Terminal UI framework |
| mlua (v0.11, luau) | Lua embedding with async |
| reqwest (v0.13, rustls) | HTTP client for provider APIs |
| serde / serde_json | Serialization |
| flume (v0.12, async) | Async MPMC channels |
| tree-sitter (v0.26) + 30+ grammars | Source parsing for `index` plugin |
| syntect + two-face | Syntax highlighting |
| tracing / tracing-subscriber | Structured logging |
| thiserror (v2) | Domain error types |
| color-eyre (v0.6) | Generic error reporting |
| crossterm (v0.29) | Terminal I/O |
| clap (v4, derive) | CLI parsing |
| aws-config + aws-sdk-bedrock | Bedrock provider (feature-gated) |
| fastembed / ort | ONNX semantic embeddings (feature-gated) |

## Entry points

- Binary: `src/main.rs:12` (`#[tokio::main]`, clap dispatch into `cmd::dispatch()`)
- Command dispatch: `src/cmd/mod.rs:15`
- TUI lib root: `craft-ui/src/lib.rs`
- Agent loop: `craft-agent/src/lib.rs`
- Provider integrations: `craft-providers/src/lib.rs`
- Python sandbox runner: `craft-interpreter/src/runner.rs`

## Non-Rust components

- Lua plugins in `./plugins/`: index, bash, glob, question, skill, memory, webfetch, websearch. Shared Lua library code under `plugins/lib/craft/` (text_input, list_picker, image_uri, color, shorten_path, truncate, tool_view).
- Python sandbox: monty (`pydantic/monty`) used by craft-interpreter.
- Static site assets under `site/`: generated docs (`site/docs/src/`), HTML, CSS, JS for the project website.
- Shell scripts: `install.sh`, `site/build.sh`.

## Build and test commands (`justfile`)

- Lint: `cargo clippy --all-features --all --tests -- -D warnings`
- Test: `cargo nextest run --all-features --workspace`
- Doc generation: `just gen-docs` (driven by `craft-docgen`)
- Lua formatting: `stylua`