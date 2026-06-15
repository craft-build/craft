# Plugins

Craft has a Lua plugin system whose API mirrors Neovim (`craft.fs`, `craft.uv`, `craft.treesitter`, `craft.env`). Plugins can register tools, slash commands, and prompt hints.

## `init.lua`

The same Lua runtime that runs plugins also loads your config. Settings live in `init.lua`, which calls `craft.setup()`:

- **Global**: `~/.config/craft/init.lua`
- **Project**: `.craft/init.lua` (relative to your working directory)

Project settings override global ones. See [Configuration](./configuration.md).

`init.lua` is also the natural place to register your own tools and commands with the `craft.api` functions below.

## `craft.api`

| Function | Registers |
|----------|-----------|
| `register_tool({ ... })` | A tool the model can call. Takes `name`, `kind`, `description`, `schema`, and a handler |
| `register_command({ ... })` | A slash command shown in the palette. Takes `name`, `description`, and a handler |
| `register_prompt_hint({ ... })` | Extra context injected into the prompt based on a trigger |

A minimal custom tool:

```lua
craft.api.register_tool({
    name = "wordcount",
    kind = "read",
    description = "Count the words in a file.",
    schema = {
        required = { "path" },
        properties = {
            path = { type = "string", description = "Path to the file" },
        },
    },
    handler = function(args)
        local ok, content = pcall(craft.fs.read, args.path)
        if not ok then
            return nil, content
        end
        local _, n = content:gsub("%S+", "")
        return "word count: " .. n + 1
    end,
})
```

## Built-in Plugins

These ship with Craft and are enabled by default (turn any off in [configuration](./configuration.md#tools)):

`bash`, `glob`, `index`, `memory`, `question`, `skill`, `webfetch`, `websearch`

They live in the `plugins/` directory of the source tree and are bundled into the binary.

## memory

The `memory` plugin is a persistent, project-scoped scratchpad. It stores files under:

```
~/.local/state/craft/projects/<project-id>/memories/
```

Tell Craft to remember something and it writes a file; sometimes it picks learnings up on its own. Use the [Tools](./tools.md#memory-lua-plugin) reference for parameters, the `/memory` command to browse them, and `/dream` to consolidate and curate them.

## Disabling Plugins

Pass `--no-plugins` to start a session with the entire plugin system disabled. The native (non-Lua) tools still work.
