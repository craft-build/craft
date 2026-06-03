<img src="./banner.png">

**Craft** is a fork of [maki](https://github.com/tontinton/maki) by Tony Solomonik — an AI coding agent optimized for minimal use of context tokens, while providing a great user experience. This fork ported the original project to use tokio and reqwest instead of smol and isahc.

Full attribution and thanks to the original project: [github.com/tontinton/maki](https://github.com/tontinton/maki.git)

## Differences to Maki

* `tokio` runtime instead of `smol`. This change was done for compatibility with async Rust libraries and to take advantage of tokio's performance optimizations.
* `reqwest` instead of `isahc`. This change was done for better synergy with `tokio` and builtin streaming support.
* Review workflow has been added for utilizing yaml based styleguide files to enforce code style.
* Multi-stage compression pipeline inspired by [Headroom](https://github.com/knuffic/headroom), which proactively reduces context usage instead of relying solely on reactive compaction:
  - **Read lifecycle management** — stale and superseded file reads are replaced with compact markers before every LLM turn, so outdated reads don't waste tokens.
  - **Tool output pre-compression** — code, logs, search results, diffs, and JSON arrays are compressed before entering the LLM context (original output is preserved for UI display).
  - **Progressive compaction** — when context is near capacity, old tool outputs are compressed in-place (aggressive compression for old, summary markers for very old) without an expensive LLM summarization call.
  - **Token estimation** — client-side character-based heuristic enables proactive compression at 80% context window, before overflow.
  - **Prefix cache awareness** — messages confirmed to be in the provider's KV cache (via `cache_read` tokens) are skipped during compression to preserve cache read discounts.
  - **Reversible compression (CCR)** — original content is stored in an in-memory LRU store, and a `retrieve` tool lets the LLM fetch originals on demand via content hashes embedded in compressed output.

## Features

### Context efficiency

* `index` tool - uses [tree-sitter](https://tree-sitter.github.io/tree-sitter) to parse supported programming languages to produce a high level skeleton of a file, with exact start-end lines of each item (e.g. a function's implementation is in lines 150-165). Encouraged to be used before reads. For my usage it adds 59 tok/turn but saves 224 tok/turn on read calls, saving 165 tok/turn.
* `code_execution` tool - uses [monty](https://github.com/pydantic/monty) to run an interpreter that has all other tools available as async functions. Craft uses it to filter / summarize / transform / pipe data to other tools as input, without it ever reaching and polluting the context window. Sandbox limited by time & memory.
* `task` tool - when delegating work to subagents, the AI chooses whether to run weak / medium / strong model of used provider. Think haiku / sonnet / opus.
* System prompt, tool descriptions, and tool examples are all concise, I've made sure not to bloat your context.
* Uses [rtk](https://github.com/rtk-ai/rtk) if you have it installed, disable with `--no-rtk`. Saves ~50% of bash output tokens. Remember bash is just 12% of total token usage, so 6% is nice, but saving on reads (65% of total) by using `index` gave me more benefit. I think I'll do bash output filtering like this myself in a future release.

### User experience

* SUPER fast startup, 60 FPS, and light on memory. Not running any JavaScript, using [ratatui](https://ratatui.rs) for TUI. Even the splash screen animation uses SIMD.
* Philosophy of not hiding anything - while other coding agents hide information as models improve (e.g. not showing number of lines read), craft leaves you in control.
* UI fits everything well on my small screen laptop.
* Full visibility of subagents - each subagent gets their own "chat window" you can easily navigate between using `/tasks` (Ctrl-X), or Ctrl-N/P.
* Sensible permission system - when the agent runs `git diff && rm -rf /`, what do you think will happen in your current coding agent? It will treat it as `git *`. Craft uses tree-sitter to parse the bash command and figure out the permissions requested are `git *` and `rm *`. Disable using `--yolo`.
* SSRF protection on `webfetch` calls.
* A `memory` tool to keep long term context, just tell craft to remember something (sometimes it uses it automatically). Managed via `/memory` (view / edit / delete memories).
* Fuzzy search with Ctrl-F.
* `/btw` to run a command with the chat history without interfering with the current session.
* Rewind on Escape-Escape (no code rewind yet, only chat history).
* Attach images in prompts.
* 26 of the most popular themes.
* Resume sessions.
* Skills & MCPs.
* Plan mode.
* Run bash commands using `!`, or `!!` if you want craft to not know about it.
* `/cd` to change dir.
* Use `--print --output-format stream-json` to run UI-less. Output is compatible with Claude Code, so you can easily replace your existing solutions (although I wouldn't recommend that, craft is very new).

## Supported providers

* Anthropic - `ANTHROPIC_API_KEY` only (using OAuth is against TOS).
* OpenAI - `OPENAI_API_KEY` and OAuth via `craft auth login openai`.
* Copilot - `GH_COPILOT_TOKEN` or an existing GitHub Copilot sign-in at `~/.config/github-copilot/`.
* Ollama - `OLLAMA_HOST` for local (e.g. `http://localhost:11434`), or `OLLAMA_API_KEY` for cloud.
* Mistral - `MISTRAL_API_KEY`.
* Z.AI - `ZHIPU_API_KEY`.
* Synthetic - `SYNTHETIC_API_KEY`.

**Dynamic providers** - drop an executable script into `~/.craft/providers/` to add custom providers or proxies.

## Installation

```sh
cargo install --locked --git https://gitlab.com/craft-build/craft.git craft
```

Or download a pre-built binary from [GitLab Releases](https://gitlab.com/craft-build/craft/-/releases).

## Documentation

More info at the [official docs](https://gitlab.com/craft-build/craft).

> DISCLAIMER: >90% of code in maki was written by maki, guided by humans. The code is not as good as what I would've made in the artisanal hand-made style. But it's also not slop / vibe coded. I just think people should be honest about their use of AI in projects in this era.

## Extending with Lua

Currently working on a refactor so craft is a core agent UI loop with features like tools, UI elements, and storage all controlled by Lua plugins.
This will allow you to customize the hell out of craft.

Status: webfetch, websearch, index, bash, skill, and memory tools are Lua plugins (in the `./plugins` dir).
