# Quick Start

## Install

Craft is a fork of [maki](https://github.com/tontinton/maki) by Tony Solomonik. Install from our repository:

```sh
cargo install --locked --git https://github.com/craft-build/craft.git craft
```

Or download a pre-built binary from [GitHub Releases](https://github.com/craft-build/craft/releases).

## API Keys

Export a key for at least one provider:

| Provider | Environment Variable |
|----------|----------------------|
| Anthropic | `ANTHROPIC_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Google | `GEMINI_API_KEY` |
| Copilot | `GH_COPILOT_TOKEN` |
| Mistral | `MISTRAL_API_KEY` |
| DeepSeek | `DEEPSEEK_API_KEY` |
| OpenRouter | `OPENROUTER_API_KEY` |
| Synthetic | `SYNTHETIC_API_KEY` |

OpenAI and Copilot also support OAuth via device flow. Run `craft auth login openai` and it will walk you through setting it up. See [CLI](./cli.md#auth) for all `auth` options.

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

See [Keybindings](./keybindings.md) for the full list.

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

You can also switch models mid-session with the built-in model picker (`/model`).

## Project Configuration

Add a `.craft/` directory to your project root for per-project settings:

```
.craft/
├── init.lua           # Overrides global config
├── permissions.toml   # Permission rules
├── mcp.toml           # MCP server config
├── commands/          # Custom slash commands
└── skills/            # Reusable skill workflows
AGENTS.md              # Loaded into agent context automatically
AGENTS.local.md        # Personal per-project instructions (gitignored)
```

`AGENTS.md` is loaded at the start of every session. Put coding conventions, repo quirks & gotchas, or off-limits directories in here. Craft will automatically load `AGENTS.md` files inside subdirs when doing a `read` in the subdir.

See [Configuration](./configuration.md) for all options.

## Input Tips

- Start a line with `!` to run a shell command and feed the output to the agent. `!ls` runs `ls` and sends the output as context.
- Use `!!` to run a shell command silently. The output shows in the UI but is **not** sent to the model.
- Paste an image (Ctrl+V), or type/paste an absolute path or `file://` URI to a `.png`/`.jpg`/`.gif`/`.webp` file, and Craft attaches it to your message.
- Press `/` to open the command palette.

See [Usage](./usage.md) for more.
