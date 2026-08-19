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
| `register_command({ ... })` | A slash command shown in the palette. Takes `name`, `description`, optional `nargs`, and a handler |
| `register_prompt_hint({ ... })` | Extra context injected into the prompt based on a trigger |
| `register_recency_source({ ... })` | A per-turn volatile fact (e.g. live repo state) appended to the latest user message at request time. Takes `name` and a `callback` returning a string or nil; rebuilt every turn and never persisted |
| `register_options({ ... })` | Declare the options your plugin accepts under `plugins.<name>` in `craft.setup`. Returns the user's values merged with your defaults |
| `set_prompt({ ... })` | Override a singleton prompt slot (`identity` or `tone`). Takes `slot`, `content` (string or callback), and optional `prompt` |
| `register_permission_rule({ ... })` | Declare an agent permission rule for a native tool, so paths your plugin owns do not prompt |

Slash commands accept an `nargs` option that controls how many whitespace-separated arguments they take, spelled like Neovim's `nargs`: `0` (the default), `1`, `"?"` (zero or one), `"*"` (any number), or `"+"` (one or more). Type more than allowed and the command quietly stops matching, sending the input to the model as a normal message. Only the upper bound is checked, so with `"+"` you still handle an empty `opts.args` yourself. The handler receives one `opts` table: `opts.args` is the raw argument string (whitespace kept, may be empty) and `opts.fargs` is the same string split into words.

### `craft.api.run_command()`

```lua
craft.api.run_command({cmdline})
```

Runs a slash command by name, exactly as typing it in the input would. Works for built-ins, custom `/project:` and `/user:` commands, MCP prompts, and commands other plugins registered. Use it to alias a command you like under a name you prefer, instead of reimplementing what it does. See `craft.ui.action` for the same idea applied to keybound UI actions.

Pass the whole command line, arguments included: `"/cd ~/src"`. The leading slash is optional. Names match exactly apart from case, so a typo reports an error instead of running the closest command, and a cycle of aliases stops with one too. It returns as soon as the command has been dispatched, not when it finishes, so aliasing something long-running like `/compact` does not block your handler.

**Parameters:**

- `{cmdline}` (`string`) Command line, e.g. `"/new"` or `"/cd ~/src"`.

**Returns:** (`boolean|nil`, `string|nil`) `true` once dispatched, or nil and an error message for an unknown command.

```lua
-- /resume as an alias for the built-in session picker:
craft.api.register_command({
  name = "/resume",
  description = "Alias for /sessions",
  handler = function()
    local ok, err = craft.api.run_command("/sessions")
    if not ok then
      craft.ui.flash("could not run /sessions: " .. err)
    end
  end,
})
```

### `craft.api.register_permission_rule()`

```lua
craft.api.register_permission_rule({spec})
```

Declare an agent permission rule for a native tool. Use it to pre-allow (or pre-deny) tool calls on paths your plugin owns, like a storage directory outside the working dir, so the user is not prompted for them.

Rules live as long as the plugin is loaded: a reload replaces them, and a reload that registers none clears the old ones. User config and session deny rules always win over a plugin allow.

**Parameters:**

- `{spec}` (`table`) Rule specification:
  - `tool` (`string`) Required. Native tool name (e.g. "edit", "write"). MCP tools and the "*" wildcard are not allowed.
  - `scope` (`string`) Required. Scope pattern the rule applies to, e.g. "/abs/dir/**" for a directory subtree.
  - `effect` (`string`) Optional. "allow" (default) or "deny".

**Example:**

```lua
craft.api.register_permission_rule({
  tool = "write",
  scope = notes_dir .. "/**",
})
```

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
