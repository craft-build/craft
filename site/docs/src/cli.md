# CLI

Run `craft` with no subcommand to launch the interactive TUI. Subcommands and flags cover auth, models, headless runs, and editor/automation integrations.

## Global Flags

These work on the default (TUI / headless) invocation:

| Flag | Description |
|------|-------------|
| `-p`, `--print` | Non-interactive: run the prompt and exit |
| `-m`, `--model` | Model spec, e.g. `anthropic/claude-sonnet-4-6` |
| `--verbose` | Include full turn-by-turn messages in `--print` output |
| `-c`, `--continue` | Resume the most recent session in this directory |
| `-s`, `--session` *(alias `--resume`)* | Resume a specific session by ID |
| `--output-format` | `text` (default), `json`, or `stream-json` |
| `--input-format` | `text` (default) or `stream-json` (SDK mode) |
| `--yolo` *(alias `--dangerously-skip-permissions`)* | Skip all permission prompts |
| `--allowed-tools` | Pre-approve a comma-separated tool list |
| `--disallowed-tools` | Forbid a comma-separated tool list |
| `--max-turns` | Cap the number of agent turns |
| `--system-prompt` | Replace the system prompt |
| `--append-system-prompt` | Append to the system prompt |
| `--exit-on-done` | Exit after the agent finishes |
| `--fork-session` | Fork the resumed session under a new ID |
| `--no-commands` | Skip custom commands from `.craft/commands`, etc. |
| `--no-plugins` | Disable the Lua plugin system |
| `--no-rtk` | Disable rtk command rewriting |
| `--permission-mode` | Permission mode (SDK) |
| `--include-partial-messages` | Stream partial messages in SDK output |

The initial prompt can be passed as the last positional argument, or piped over stdin.

## Subcommands

### `auth`

Manage stored provider credentials.

```bash
craft auth login <provider>    # OAuth / device flow (e.g. openai, copilot, or a dynamic provider slug)
craft auth logout <provider>   # remove stored credentials
```

`login` runs an interactive flow. Supported out of the box: `openai`, `copilot`, and any [dynamic provider](./providers.md#dynamic-providers) slug. API-key providers are configured with environment variables instead.

### `models`

List every available model across all configured providers, with tier and pricing.

```bash
craft models
```

### `index`

Run the built-in `index` tool on a file and print the tree-sitter skeleton. Useful for previewing what the model sees.

```bash
craft index src/main.rs
```

### `mcp`

Manage OAuth credentials for HTTP-transport MCP servers.

```bash
craft mcp auth <server-name>     # trigger browser OAuth
craft mcp logout <server-name>   # remove stored tokens
```

### `acp`

Run Craft as an [ACP](./acp.md) server over stdio for editor integration. Accepts `--yolo`.

### `update`

Update Craft to the latest release. `-y` skips the confirmation prompt; `--no-color` disables highlighting.

### `rollback`

Roll back to the previously installed version.

### `migrate`

Data migration utilities. Currently supports `xdg`, which moves a legacy `~/.craft/` directory into the proper [XDG locations](./configuration.md#directory-layout).

```bash
craft migrate xdg
```
