# Commands

Type `/` in the input box to open the command palette.

## Built-in commands

| Command | Description |
|---------|-------------|
| `/tasks` | Browse and search tasks |
| `/compact` | Summarize and compact conversation history |
| `/new` | Start a new session |
| `/help` | Show keybindings |
| `/usage` | Show token usage breakdown |
| `/stats` | Show cost and usage stats |
| `/queue` | Remove items from queue |
| `/model` | Switch model |
| `/theme` | Switch color theme |
| `/mcp` | Configure MCP servers |
| `/login` | Authenticate with an LLM provider |
| `/cd` | Change working directory |
| `/btw` | Ask a quick question (no tools, no history pollution) |
| `/yolo` | Toggle YOLO mode (skip all permission prompts) |
| `/auto-review` | Toggle auto-review (an LLM decides permission prompts instead of asking) |
| `/thinking` | Set thinking level (opens a picker; or pass off, adaptive, effort level, or budget) |
| `/fast` | Toggle Anthropic fast mode (Opus only) |
| `/exit` | Exit the application |
| `/reload` | Reload plugins and config |
| `/goal` | Set a goal the agent must meet before stopping (blank to clear) |
| `/recipe` | Browse and run a recipe |
| `/dream` | Consolidate and curate project memory |
| `/distill` | Discover reusable workflows and propose skills |
| `/checkpoint` | Write a session checkpoint for smooth resume |
| `/set-context-window` | Override a model's context window (tokens) |
| `/clear-context-window` | Clear a context-window override |
| `/wiki` | Init the project wiki, ingest a file, list entries, or show a page |
| `/map` | Show the current repo map (ranked symbol context) |
| `/map-refresh` | Force rebuild the repo map cache |
| `/map-toggle` | Toggle repo map injection on/off |
| `/watch` | Toggle watch mode (AI comments in editor drive the agent) |
| `/memory` | View, edit, and delete memory files |
| `/rename` | Rename the current session |
| `/sessions` | Browse and switch sessions |

## Sessions

Sessions run concurrently. `/new` starts a fresh session while the old one keeps working in the background, and `/sessions` shows the live status of each (working, needs input, idle) so you can jump between them. When a background session finishes or needs input, Craft flashes a note in the status bar.

## Custom commands

You can define your own slash commands as Markdown files.

### Project commands

Place `.md` files in `.craft/commands/` in your project root.
They appear in the palette as `/project:<filename>`.

### User commands

Place `.md` files in `~/.config/craft/commands/`.
They appear in the palette as `/user:<filename>`.

Project commands override user commands with the same name.

`.claude/commands/` directories are also supported for compatibility.

### Metadata

You can add optional metadata at the top of the file between `---` lines to set `name`, `description`, and `argument-hint`:

```markdown
---
description: Review code for issues
argument-hint: <file>
---
Review $ARGUMENTS and suggest improvements.
```

### Arguments

Use `$ARGUMENTS` in the command body. It gets replaced with whatever you type after the command name.

For example, `/project:review main.rs` replaces `$ARGUMENTS` with `main.rs`.

## Aliasing commands

Prefer a different name for a command? `craft.api.run_command` runs any slash command exactly as typing it would, so an alias is a one-line handler in your `init.lua` instead of a reimplementation.

```lua
-- ~/.config/craft/init.lua
local aliases = {
    { name = "/clear", target = "/new", description = "Alias for /new" },
    { name = "/resume", target = "/sessions", description = "Alias for /sessions" },
}

for _, alias in ipairs(aliases) do
    craft.api.register_command({
        name = alias.name,
        description = alias.description,
        handler = function()
            local ok, err = craft.api.run_command(alias.target)
            if not ok then
                craft.ui.flash("could not run " .. alias.target .. ": " .. err)
            end
        end,
    })
end
```

Both names stay in the palette: aliasing adds a name, it does not rename or hide the original. It works for any command listed above, plus plugin commands and MCP prompts. See the plugins page for matching and error handling, or `craft.ui.action` to bind a key instead of a name.