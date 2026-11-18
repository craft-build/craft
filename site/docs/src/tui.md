# TUI

The terminal UI is built on **superlighttui**: native, no JavaScript runtime, fast startup and a high frame rate even on small laptop screens. `craft` starts it by default.

## Daily driving

- **Prompt** — type and press Enter. A positional `PROMPT` argument submits as the first message; piped stdin (`echo "do x" | craft`) is read the same way.
- **Plan mode** — Tab toggles build/plan. The plan form and todo cards keep the plan first-class; plan-mode turns run read-only (see [Sandbox](./sandbox.md)). The mode persists in the session and is restored on resume.
- **`!` and `!!`** — run a bash command with or without the agent seeing the output.
- **Editor integration** — Ctrl-O / Alt+O opens the input (or a file) in `$EDITOR`.

## Keys

| Key | Action |
| --- | --- |
| Ctrl-P | command palette |
| Ctrl-F | fuzzy search over the transcript |
| Ctrl-N / Ctrl-P | subagent chat windows (also `/tasks`) |
| Ctrl-V | paste clipboard image |
| Ctrl-H | keybinding help |
| Alt+E | cycle thinking/effort |
| Tab | plan / build toggle |
| Esc | cancel streaming / close overlays |

Keymaps are rebindable via `keybind` blocks in the config (see [Configuration](./configuration.md#keybinds)) — action ids like `palette`, `plan_toggle`, `paste_image`, `task_chat_picker`; an empty chord list disables the action.

## Slash commands

`/new`, `/resume`, `/continue`, `/sessions`, `/model`, `/clear`, `/undo`, `/mcp`, `/compact`, `/usage`, `/stats`, `/auto-review`, `/thinking`, `/theme`, `/recipe`, `/review`, `/scan`, `/dream`, `/memory` — plus custom namespaced commands (see [Skills & Recipes](./skills-recipes.md)).

`--no-commands` skips custom slash-command discovery.

## Themes

30 built-in themes via `/theme`; the choice persists. Tool-call rendering switches between `cards` (one card per call) and `compact` (collapsible groups; read-only tools as one-line entries) via `agent.tool_display`.

## Thinking

`/thinking` opens the selector; `/thinking low`, `/thinking adaptive`, `/thinking off`, or `/thinking 4096` apply directly. Alt+E cycles. The preference is persisted with the session, restored on resume, and inherited by subagents. Details in [CLI → Thinking controls](./cli.md#thinking-controls).
