---
type: Note
description: Domain terms, acronyms, and project-specific jargon used across Craft.
tags:
- glossary
- terminology
- acronyms
timestamp: 2026-07-11T20:26:57Z
---
# Glossary

Terms and acronyms used across Craft's docs and source.

- **ACP (Agent Client Protocol)** - JSON-RPC protocol over stdio; `craft acp` exposes Craft as a server that ACP-capable editors (e.g. Zed) can drive (`site/docs/src/acp.md`, `craft-acp/src/server.rs`).
- **Agent loop** - The async turn cycle in craft-agent that streams provider responses, dispatches tools, and emits events (`craft-agent/src/agent/run.rs`).
- **Compaction** - Compressing tool outputs and history to keep context lean when it nears the window threshold (`craft-agent/src/agent/compaction.rs:72`, `site/docs/src/configuration.md`).
- **Compaction tier** - A model tier dedicated to summarizing context when conversations grow long, distinct from weak/medium/strong (`site/docs/src/providers.md`).
- **Compression marker** - A placeholder like `[N lines compressed from M. Retrieve original: hash=HASH]` indicating retrievable compressed content (`site/docs/src/tools.md`).
- **craft-interpreter** - Crate implementing the `code_execution` tool via the monty Python sandbox (`craft-interpreter/src/runner.rs`).
- **craft-ui** - Crate providing the ratatui terminal UI using Elm-like model-update-view (`craft-ui/src/app/mod.rs`).
- **Elm architecture** - Unidirectional model-update-view pattern used by craft-ui.
- **Flow mode** - Multi-stage pipeline (Scout, Plan, Execute, Review, QA, Verifier, ...) that breaks larger features into sub-agent-run stages with approval gates (`site/docs/src/flow.md`, `craft-flow`).
- **MCP (Model Context Protocol)** - Protocol for connecting external tool servers over stdio/HTTP; their tools appear as `server__tool_name` (`site/docs/src/mcp.md`, `craft-acp/src/mcp.rs`).
- **Model tier** - Categorization of models into weak (cheap/fast), medium (balanced), strong (highest capability), and compaction (`craft-providers/src/model_registry.rs`, `site/docs/src/providers.md`).
- **Monty** - Minimal Python interpreter (from pydantic) powering craft-interpreter's sandbox; tools are exposed as async functions.
- **OKF (Open Knowledge Fragment)** - YAML frontmatter format (v0.1) used by the wiki for durable markdown notes with `type`, `description`, and `tags` (`craft-storage/src/wiki.rs`).
- **Plugin** - Lua extension loaded from `init.lua` that registers tools, slash commands, or prompt hints via `craft.api` (`site/docs/src/plugins.md`, `craft-lua/src/runtime.rs`).
- **Provider** - An LLM service (Anthropic, OpenAI, Copilot, Ollama, Mistral, Bedrock, ...) that Craft talks to over HTTP with configurable models and auth (`craft-providers/src/provider.rs`).
- **Ratatui** - Rust terminal UI library used by craft-ui.
- **Sandbox** - Restricted execution environment for tools like `code_execution`, with configurable permissions (network, filesystem) (`craft-sandbox`, `site/docs/src/configuration.md`).
- **Semantic intelligence** - Optional ONNX feature (via fastembed) providing local embeddings for relevance scoring, context curation, overlap detection, and auto-retrieval of compressed content (`craft-agent/src/agent/semantic.rs`, `README.md`).
- **Session** - Persisted append-only JSONL file under the state dir containing the full conversation, tool outputs, sub-agent chats, permissions, and mode (`craft-storage/src/lib.rs`, `site/docs/src/sessions.md`).
- **Skill** - Reusable workflow written as markdown (with optional YAML frontmatter), loaded on demand via the `skill` tool for task-specific instructions (`site/docs/src/skills.md`).
- **Snapshot** - Immutable fuzzy-matcher state (nucleo) capturing the pattern and matched items during search (`craft-ui/src/components/command.rs`).
- **Subagent** - Autonomous agent spawned by the `task` tool with its own context window; can be read-only (`research`) or write-capable (`general`), optionally isolated in a git worktree (`craft-agent/src/tools/task.rs`).
- **Trust decay** - Per-tool consecutive-failure tracking that warns (`warn_after=3`) then drops (`drop_after=5`) flaky tools (`craft-config/src/lib.rs`, `README.md`).
- **Wiki** - Project-scoped knowledge store under `.wiki/` using OKF frontmatter for decisions, notes, and glossary entries (`craft-storage/src/wiki.rs`).
- **YOLO mode** - Mode that skips permission prompts (explicit deny rules still apply), toggled via `/yolo` or `--yolo` (`site/docs/src/permissions.md`).