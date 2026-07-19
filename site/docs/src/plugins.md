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
| `register_options({ ... })` | Declare the options your plugin accepts under `plugins.<name>` in `craft.setup`. Returns the user's values merged with your defaults |
| `set_prompt({ ... })` | Override a singleton prompt slot (`identity` or `tone`). Takes `slot`, `content` (string or callback), and optional `prompt` |

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

### Plugin options

`register_options` is how a plugin reads its `plugins.<name>` table. Call it once at the top level of your plugin file. An unknown key, a wrong type, or a value below `min` fails the plugin load with a clear message, so users catch typos right away. The specs also feed the generated configuration docs.

```lua
local opts = craft.api.register_options({
  timeout_secs = { default = 120, min = 5, desc = "Kill the command after this many seconds." },
  label = { type = "string", desc = "Label shown in the UI." },
})
```

Each spec table accepts:

- `default` (boolean, number, or string): used when the user sets nothing. Its Lua type becomes the option type.
- `type` (`"boolean"`, `"integer"`, `"number"`, or `"string"`): required when there is no `default`.
- `min` (number): minimum accepted value, numeric options only.
- `desc` (string, required): one line shown in the configuration docs.

The returned table is the merged options: the user's value where set, otherwise the default, or `nil` when neither exists.

## Prompt slots

The system prompt is assembled from named slots. Two kinds exist:

- **Singleton** slots (`identity`, `tone`) hold a single value. `set_prompt` replaces the built-in default. Last plugin to call it wins.
- **Aggregate** slots (`tool_usage`, `efficient_tools`, `conventions`, `after_instructions`) append. Use `register_prompt_hint` for those.

Override the agent's identity or tone from `init.lua`:

```lua
craft.api.set_prompt({
    slot = "identity",
    content = "Custom identity",
})

craft.api.set_prompt({
    slot = "tone",
    content = function() return "Short, direct answers." end,
})
```

`content` can be a string or a function returning a string. Omit `prompt` to apply to every prompt that has the slot. Singleton slots only exist in the `system` prompt, so targeting `research` or `general` with them is an error. Use `craft prompt` to preview the result.

## Built-in Plugins

These ship with Craft and are enabled by default (turn any off under [plugins](./configuration.md#plugins)):

`bash`, `glob`, `grep`, `memory`, `question`, `sessions`, `skill`, `todo_write`, `view_image`, `webfetch`, `websearch`

They live in the `plugins/` directory of the source tree and are bundled into the binary.

## memory

The `memory` plugin is a persistent, project-scoped scratchpad. It stores files under:

```
~/.local/state/craft/projects/<project-id>/memories/
```

Tell Craft to remember something and it writes a file; sometimes it picks learnings up on its own. Use the [Tools](./tools.md#memory-lua-plugin) reference for parameters, the `/memory` command to browse them, and `/dream` to consolidate and curate them.

## Disabling Plugins

Pass `--no-plugins` to start a session with the entire plugin system disabled. The native (non-Lua) tools still work.
