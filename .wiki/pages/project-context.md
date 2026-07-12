---
type: Note
description: One-paragraph overview of Craft with links to the tech stack, architecture, design decisions, and glossary pages.
tags:
- overview
- onboarding
- index
timestamp: 2026-07-11T20:27:14Z
---
# Project Context

Craft is an AI coding agent written in Rust. It is optimized to minimize tokens and cost while keeping performance competitive with tools like Claude Code and opencode. Users interact through a ratatui terminal UI (or headless/SDK mode), an async agent loop streams responses from pluggable LLM providers, and tools run in a permissioned sandbox including a Python interpreter and Lua plugins. State persists across runs as sessions, costs, and wiki notes. This starter wiki captures the essentials for new contributors and links to the detail pages below.

## Where to go next

- [Tech Stack](tech-stack) - languages, workspace crates, key dependencies, entry points.
- [Architecture](architecture) - crate responsibilities, request data flow, concurrency model, integration boundaries.
- [Design Decisions](design-decisions) - coding conventions, error handling, testing, logging, dependency policy.
- [Glossary](glossary) - domain terms and acronyms used across docs and source.

## Quick orientation

- Entry point: `src/main.rs:12` dispatches clap subcommands from `src/cmd/`.
- Agent loop: `craft-agent/src/agent/run.rs`.
- UI: `craft-ui/src/app/mod.rs` (Elm architecture).
- Providers: `craft-providers/src/provider.rs`.
- Config: `craft-config/src/lib.rs`.
- Storage: `craft-storage/src/lib.rs`.
- Built-in Lua plugins: `./plugins/`.

## Build and test

- Lint: `cargo clippy --all-features --all --tests -- -D warnings`
- Test: `cargo nextest run --all-features --workspace`
- Docs: `just gen-docs`

See [Design Decisions](design-decisions) for the full convention set.