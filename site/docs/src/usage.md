# Usage

The everyday patterns for driving Craft interactively.

## Modes

Craft runs in two modes:

- **Build** (full access): the agent can read, edit, and run code.
- **Plan** (read-only): the agent can gather context but cannot modify anything. Use it to scope out a change before committing to it.

Switch modes with **Tab** in the input box, or open the plan/todo panel with **Ctrl+T**.

## Shell Commands from the Input

Prefix a line to run a shell command without leaving the prompt:

| Prefix | Behavior |
|--------|----------|
| `!command` | Runs `command`, sends the output to the agent as context |
| `!!command` | Runs `command` silently. Output shows in the UI but is **not** sent to the model |

`!ls -la` and `! ls -la` both work (the space after the sigil is optional). The prefix must be at the start of the line.

## Images

Attach images to a message in three ways:

- **Paste from clipboard** with Ctrl+V.
- **Type or paste a path** to an image file: an absolute path, a `~/` path, or a `file://` URI. Supported types are `.png`, `.jpg`, `.jpeg`, `.gif`, and `.webp`.
- Limits: 8 megapixels and 20MB per image.

## Command Palette

Press `/` to open the palette. It lists [built-in commands](./commands.md), your [custom commands](./commands.md#custom-commands), MCP prompts, and Lua plugin commands. Start typing to filter.

## Common Commands

| Command | What it does |
|---------|--------------|
| `/model` | Switch model or reassign a tier (`1`/`2`/`3` on a row) |
| `/theme` | Switch color theme |
| `/sessions` | Browse, switch, or delete sessions |
| `/tasks` | Browse and search tasks |
| `/mcp` | Toggle MCP servers |
| `/compact` | Summarize and compact history |
| `/btw` | Ask a side question with no tools and no history pollution |
| `/goal` | Set a goal the agent must meet before stopping |
| `/yolo` | Toggle skipping permission prompts |
| `/thinking` | Toggle extended thinking (off, adaptive, or a token budget) |
| `/fast` | Toggle Anthropic fast mode (Opus only) |
| `/dream` | Consolidate and curate [memory](./plugins.md#memory) |
| `/distill` | Discover reusable workflows and propose [skills](./skills.md) |
| `/checkpoint` | Write a session checkpoint for smooth resume |
| `/memory` | View, edit, and delete memory files |

## Rewind

Press **Esc Esc** to rewind: pick an earlier point in the conversation and branch from there.

## Queue

Queue up multiple prompts while the agent is busy. They run in order once the current task finishes. **Ctrl+Q** pops an item off the queue; `/queue` manages the queue.

## Visibility

Craft keeps cost and usage in the open. The status bar always shows token count, cost, and the active model. Each sub-agent gets its own chat you can flip through with **Ctrl+N** / **Ctrl+P**, and **Ctrl+F** searches the conversation.
