# Craft

Craft is a terminal-based coding agent. Point it at a codebase, pick an LLM provider, and let it read, edit, search, and run code for you.

It is a fork of [maki](https://github.com/tontinton/maki) by Tony Solomonik, written in Rust and built to keep cost and token usage low without losing capability.

## Features

- **TUI** built on ratatui with syntax highlighting, inline image rendering, and fuzzy search.
- **Built-in tools** for file ops, search, code execution, web access, and more. See [Tools](./tools.md).
- **Multiple providers.** Anthropic, OpenAI, Google, Copilot, Z.AI, DeepSeek, Mistral, Ollama, llama.cpp, OpenRouter, Synthetic, and a dynamic provider system for plugging in your own. See [Providers](./providers.md).
- **MCP support.** Connect external tool servers over stdio or HTTP. See [MCP](./mcp.md).
- **Permissions.** Fine-grained allow/deny rules, plus a YOLO mode. See [Permissions](./permissions.md).
- **Sub-agents.** Spin up read-only research agents or full-access workers that run in parallel.
- **Session persistence.** Pick up where you left off, context and permissions intact. See [Sessions](./sessions.md).
- **Python sandbox.** A minimal interpreter for running Python snippets safely inside the agent loop.
- **Code indexing.** Tree-sitter powered file skeletons for 15+ languages, so the model can understand structure without reading every line.
- **Skills & plugins.** Reusable workflows as Markdown skills, and a Lua plugin API that mirrors Neovim. See [Skills](./skills.md) and [Plugins](./plugins.md).
- **Headless mode.** Run non-interactively with `--print` for scripts and CI. Output is Claude Code-compatible. See [Headless Mode](./headless.md).
- **ACP server.** Use Craft from your editor (e.g. [Zed](https://zed.dev/)) over the Agent Client Protocol with `craft acp`. See [ACP](./acp.md).

Ready to try it? Head to the [Quick Start](./quick-start.md).
