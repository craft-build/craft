<img src="./banner.png">

**Craft** is an AI coding agent built from the ground up in Rust to spend as few tokens as possible, without giving up the speed and ergonomics that make a coding agent feel good to use. Native terminal UI, instant startup, 60 FPS, and a small memory footprint.

## Why craft

Most coding agents burn context reactively, waiting until they hit the limit, then summarizing everything in one expensive pass. Craft is built around a different idea: keep the context lean at every step, so the model spends tokens on your work instead of on stale reads, verbose tool output, and duplicated history.

That shows up across the whole agent:

- A multi-stage compression pipeline trims context continuously, not just when it overflows.
- Optional semantic intelligence scores history by relevance so the important parts survive compaction.
- Tools are designed to return skeletons, summaries, and filtered results instead of raw dumps.
- Subagents carry their own context windows, so delegation does not pollute the main session.

The goal is simple: longer effective sessions, lower cost per turn, and an agent that stays fast and readable as a task grows.

## Features

### Whats different from maki

While the below sections call out all the features of craft both inherited from maki and unique to craft. The following features are unique to craft:

- Multi-stage compression pipeline
- Semantic intelligence
- Keyword scoring
- Tool deduplication
- Most of Reliability and Guardrails

Runtime difference - Craft also significantly retools the async runtime and network stack of maki by converting it from smol and isahc to tokio and reqwest. This was done primarily for the comfort of the craft developer, but also for reqwest's robustness and stability.

### Context efficiency

- **`index` tool** - uses [tree-sitter](https://tree-sitter.github.io/tree-sitter) to parse supported languages into a high-level skeleton with exact line ranges for every item. Encouraged before reads, since a compact outline replaces scrolling through a full file.
- **`code_execution` tool** - runs [monty](https://github.com/pydantic/monty), a minimal Python sandbox with every other tool available as an async function. Use it to filter, summarize, transform, and pipe data between tools without that data ever reaching the context window. Bounded by time and memory.
- **`task` tool** - delegate work to subagents and let the agent pick the right tier for the job (weak, medium, or strong model). Each subagent runs in its own context window.
- **Multi-stage compression pipeline** (inspired by [Headroom](https://github.com/knuffic/headroom)):
  - **Read lifecycle** - stale and superseded file reads are replaced with compact markers before each turn.
  - **Tool output pre-compression** - code, logs, search results, diffs, and JSON arrays are compressed before entering history, using content-type detection and keyword-aware line scoring.
  - **Progressive compaction** - at 60% of the window, old tool outputs are compressed in place, avoiding an expensive summarization call until it is truly needed.
  - **Prefix-cache awareness** - messages confirmed to be in the provider KV cache (via `cache_read` tokens) are skipped, preserving cache-read discounts.
  - **Reversible compression** - originals are kept in an in-memory LRU store, and a `retrieve` tool lets the model fetch them back on demand via content hashes.
- **Semantic intelligence** (optional, `onnx` feature) - local ONNX embeddings via [fastembed](https://github.com/Anush008/fastembed-rs) for smarter context management:
  - Relevance scoring builds an intent embedding from recent messages and ranks history by relevance.
  - Semantic context curation picks the most relevant messages within budget instead of a flat window.
  - Overlap detection finds old tool results that duplicate newer ones and compresses the older copies.
  - Auto-retrieve restores compressed content from the LRU store when it becomes relevant again.
  - Stagnation detection flags high-similarity consecutive turns.
  - [Magika](https://github.com/google/magika) content detection routes compression more accurately than file extensions.
  - Models download eagerly at startup, before the TUI takes over the terminal.
- **Keyword scoring** - a shared aho-corasick line classifier with explicit categories (error, warning, security, code definition, import, module, comment, closing brace) replaces hand-rolled scoring for code and log compression.
- **Tool dedup cache** - caches read-only tool results (`read`, `grep`, `glob`, `index`) keyed by argument hash, bounded to 64 entries with FIFO eviction and cleared on compaction. Cache hits are prefixed with `[cached]`.
- **Lua embed API** - `embed(text)` and `similarity(a, b)` are exposed to the Lua plugin host through a channel-based bridge.
- Compact system prompt, tool descriptions, and examples throughout.
- Optional [rtk](https://github.com/rtk-ai/rtk) support to cut bash output tokens (disable with `--no-rtk`).

### Reliability and guardrails

- **Trust decay** - tracks per-tool consecutive failures and demotes or drops tools after configurable thresholds (`warn_after=3`, `drop_after=5`), with a `min_tools` safeguard. Configurable via `[agent.trust_decay]`.
- **Snapshot and rollback** - auto-snapshots files before `write`, `edit`, and `multiedit`, commits on agent Done, and supports rollback via `/undo`. Files larger than 5 MB or outside the workdir are skipped.
- **Post-write validation** - detects the project type (Rust, TypeScript, Go, Python) from config files and runs validation commands after writes. Disabled by default, configurable via `[agent.validation]`.
- **Small model mode** - auto-detects models with a context window under 32k and adapts: reduces tools to a core set, uses a compact system prompt, triggers compaction at 50% instead of 80%, and applies aggressive JSON repair on parse failures. Configurable via `[agent.small_model]`.
- **Model escalation** - tracks per-model failure rates and emits a `ModelEscalation` event at 60% after 5 calls, prompting an automatic tier upgrade (haiku/flash to sonnet to opus).
- **Review workflow** - YAML-based styleguide files drive a code review pass that enforces project conventions.
- **Permissions** - when the agent runs `git diff && rm -rf /`, other agents may treat that as `git *`. Craft parses the bash command with tree-sitter and requests `git *` and `rm *` separately. Disable with `--yolo`.
- **SSRF protection** on `webfetch` calls.

### Experience

- Fast startup, 60 FPS, and a small memory footprint. No JavaScript anywhere, just [ratatui](https://ratatui.rs) for the TUI. Even the splash animation uses SIMD.
- Philosophy of not hiding anything. Where other agents stop showing details as models improve (for example, number of lines read), craft leaves you in control.
- Layouts that fit comfortably on a small laptop screen.
- Full subagent visibility - each subagent gets its own chat window. Navigate them with `/tasks` (Ctrl-X) or Ctrl-N / Ctrl-P.
- `memory` tool for long-term context. Tell craft to remember something, or let it decide on its own. Manage memories with `/memory`.
- Fuzzy search with Ctrl-F.
- `/btw` runs a command against the chat history without disturbing the current session.
- Rewind on Escape-Escape (chat history only for now).
- Attach images in prompts.
- 26 of the most popular themes.
- Resume sessions.
- Skills and MCPs.
- Plan mode.
- Run bash with `!`, or `!!` to run it without telling the agent.
- `/cd` to change directory.
- `--print --output-format stream-json` for a UI-less run, with output compatible with Claude Code.

## Supported providers

- Anthropic - `ANTHROPIC_API_KEY` only (OAuth is against the TOS).
- OpenAI - `OPENAI_API_KEY`, or OAuth via `craft auth login openai`.
- Copilot - `GH_COPILOT_TOKEN`, or an existing sign-in at `~/.config/github-copilot/`.
- Ollama - `OLLAMA_HOST` for local (for example `http://localhost:11434`), or `OLLAMA_API_KEY` for cloud.
- Mistral - `MISTRAL_API_KEY`.
- Synthetic - `SYNTHETIC_API_KEY`.

**Dynamic providers** - drop an executable script into `~/.config/craft/providers/` to add a custom provider or proxy.

## Installation

```sh
cargo install --git https://github.com/craft-build/craft.git craft
```

## Documentation

More info in the [official docs](https://craft-build.github.io/craft/index.html).

## Extending with Lua

Craft ships a Lua plugin host with an API mirrored from Neovim, so existing plugin authors feel at home. Tools, UI elements, and storage can all be driven by plugins, which lets you customize craft heavily.

Today the `index`, `bash`, `glob`, `question`, `skill`, `memory`, `webfetch`, and `websearch` tools are Lua plugins living in [`./plugins`](./plugins).

## Attribution

Craft is a fork of [maki](https://github.com/tontinton/maki) by **Tony Solomonik**, and the vast majority of the foundation is his work. Full credit and thanks to the original project. Craft builds on that base with a tokio and reqwest runtime, a multi-stage compression pipeline, semantic intelligence, a Lua plugin system, an ACP server, and a range of other additions.

> Honesty note: a large share of the codebase was written by an AI, guided by humans. It is not hand-rolled artisanal code, but it is not vibe-coded slop either. In this era, being upfront about how software is made matters.
