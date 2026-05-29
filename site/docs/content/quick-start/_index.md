+++
title = "Quick Start"
weight = 1
[extra]
group = "Getting Started"
+++

# Quick Start

## Install

Craft is a fork of [maki](https://github.com/tontinton/maki) by Tony Solomonik. Install from our repository:

```sh
cargo install --locked --git https://gitlab.com/craft-build/craft.git craft
```

Or download a pre-built binary from [GitLab Releases](https://gitlab.com/craft-build/craft/-/releases).

## API Keys

Export a key for at least one provider:

| Provider | Environment Variable |
|----------|---------------------|
| Anthropic | `ANTHROPIC_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Z.AI | `ZHIPU_API_KEY` |
| Synthetic | `SYNTHETIC_API_KEY` |

OpenAI also supports OAuth via device flow. Run `craft auth login openai` and it will walk you through setting it up.

## Run

From your project directory:

```bash
craft
```

Type a prompt, press **Enter**, and the agent starts working.

## Keybindings

- **Newline in input**: \+Enter, Ctrl+J, or Alt+Enter
- **Scroll output**: Ctrl+U / Ctrl+D (half page), Ctrl+Y / Ctrl+E (line)
- **Cancel streaming**: Esc Esc
- **Quit**: Ctrl+C
- **All keybindings**: Ctrl+H

## Choosing a Model

Set a default in your config:

```lua
-- ~/.config/craft/init.lua
craft.setup({
    provider = {
        default_model = "anthropic/claude-sonnet-4-6",
    },
})
```

You can also switch models mid-session with the built-in model picker.

## Project Configuration

Add a `.craft/` directory to your project root for per-project settings:

```
.craft/
├── init.lua           # Overrides global config
├── permissions.toml   # Permission rules
└── mcp.toml           # MCP server config
AGENTS.md              # Loaded into agent context automatically
AGENTS.local.md        # Personal per-project instructions (gitignored)
```

`AGENTS.md` is loaded at the start of every session. Put coding conventions, repo quirks & gotchas, or off-limits directories in here. Craft will automatically load `AGENTS.md` files inside subdirs when doing a `read` in the subdir.

See [Configuration](/docs/configuration/) for all options.
