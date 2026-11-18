# TUI

Run `craft` from your project directory to open the terminal UI. Type a prompt and press **Enter** to send it.

## Everyday tasks

- **Prompt** — type and press Enter. A positional `PROMPT` argument submits as the first message; piped stdin (`echo "do x" | craft`) is read the same way.
- **Plan mode** — Tab toggles build/plan. The plan form and todo cards keep the plan first-class; plan-mode turns run read-only (see [Sandbox](./sandbox.md)). The mode persists in the session and is restored on resume.
- **Run a shell command** — `! command` adds the output to the agent's history; `!! command` runs it without adding the output to that history.
- **Editor integration** — Ctrl-O / Alt+O opens the input (or a file) in `$EDITOR`.

## Keys

| Key | Action |
| --- | --- |
| Ctrl-P | command palette |
| Ctrl-F | fuzzy search over the transcript |
| Ctrl-N | open the subagent chat picker when task chats exist (also `/tasks`) |
| Ctrl-V | paste clipboard image |
| Ctrl-H | keybinding help |
| Alt+E | cycle thinking/effort |
| Tab | plan / build toggle |
| Esc | cancel streaming / close overlays |

Keymaps are rebindable via `keybind` blocks in the config (see [Configuration](./configuration.md#keybinds)) — action ids like `palette`, `plan_toggle`, `paste_image`, `task_chat_picker`; an empty chord list disables the action.

## Slash commands

Use the command palette (**Ctrl-P**) to discover commands. Common commands are grouped below.

### Sessions and models

- `/new` starts a session; `/sessions`, `/resume`, and `/continue` help you return to saved work. See [Sessions](./sessions.md).
- `/model` opens model selection.
- `/thinking` selects a reasoning preference.

### Context and changes

- `/compact` compacts the conversation now, rather than waiting for a threshold.
- `/clear` clears the conversation context.
- `/undo` restores snapshotted file contents. It is not a general undo for shell commands; see [Permissions & Snapshots](./permissions.md#snapshots-and-undo).
- `/usage` and `/stats` show usage information.

### Tools and workflows

- `/mcp` lists connected MCP servers.
- `/auto-review` toggles the permission reviewer, not code review.
- `/review`, `/scan`, and `/dream` start [review workflows](./review.md).
- `/memory` opens the project memory workflow.
- `/recipe` picks a recipe; custom namespaced commands are also supported. See [Skills & Recipes](./skills-recipes.md).
- `/theme` selects a theme.

`--no-commands` skips custom slash-command discovery.

## Themes

30 built-in themes via `/theme`; the choice persists. Tool-call rendering switches between `cards` (one card per call) and `compact` (collapsible groups; read-only tools as one-line entries) via `agent.tool_display`.

## Thinking

`/thinking` opens the selector; `/thinking low`, `/thinking adaptive`, `/thinking off`, or `/thinking 4096` apply directly. Alt+E cycles. The preference is persisted with the session, restored on resume, and inherited by subagents. Details in [CLI → Thinking controls](./cli.md#thinking-controls).
