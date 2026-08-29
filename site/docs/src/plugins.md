# Plugins

Craft has a Lua plugin system whose API mirrors Neovim (`craft.fs`, `craft.uv`, `craft.treesitter`, `craft.env`). Plugins can register tools, slash commands, and prompt hints.

## `init.lua`

The same Lua runtime that runs plugins also loads your config. Settings live in `init.lua`, which calls `craft.setup()`:

- **Global**: `~/.config/craft/init.lua`
- **Project**: `.craft/init.lua` (relative to your working directory)

Project settings override global ones. See [Configuration](./configuration.md).

`init.lua` is also the natural place to register your own tools and commands with the `craft.api` functions below.

## Your own plugins

`init.lua` can `require()` Lua modules, and that is where your own plugins live. The module root is the `lua/` directory next to `init.lua`, both globally and per project:

- `~/.config/craft/lua/<name>.lua` (a legacy `~/.craft/lua/` works too, and loads first)
- `<project>/.craft/lua/<name>.lua`

A module name is its path under `lua/` without the extension: `lua/hello.lua` is loaded with `require("hello")`, `lua/acme/tools.lua` with `require("acme.tools")`. `require` is sandboxed to the `lua/` directory and cannot reach files outside it. Nothing under `lua/` loads on its own, so `init.lua` stays the entry point.

A minimal plugin, in `~/.config/craft/lua/hello.lua`:

```lua
craft.api.register_tool({
    name = "hello",
    description = "Say hello to a name.",
    schema = {
        required = { "name" },
        properties = { name = { type = "string" } },
    },
    handler = function(args)
        return { llm_output = "hello " .. args.name }
    end,
})
```

Loaded from `~/.config/craft/init.lua`:

```lua
require("hello")
```

Run `/reload` to pick up changes. If something fails to load, read `craft.log` in the directory `craft.env.logs_dir()` returns (Linux: `~/.local/logs/craft/`); `craft.log.info(...)` calls from your plugin land there too. Keep a plugin's settings in a local table or export a `setup(opts)` function for `init.lua` to call, rather than using `craft.api.register_options`: a `plugins.<name>` table for a plugin Craft does not ship fails startup with an unknown-plugin error.

### Minimum version

A plugin that needs a newer Lua API can say so with `min_craft_version` in the `plugin.toml` next to its Lua:

```toml
min_craft_version = "0.4.12"

[permissions]
net = true
```

`min_craft_version` is optional and takes a plain semantic version as a lower bound, so ranges do not work. When the field is invalid or the running version is older, Craft skips the Lua in that directory and warns at startup instead of failing. The same floor applies to an installed package, which is skipped while the rest keep loading. `--no-plugins` still skips every user plugin at once.

The builtin `plugin-dev` skill carries the same guide, so asking Craft to write a plugin for you works without pasting any of this.

## Packages

A Lua package adds tools, commands, keybindings, and event handlers without copying its code into `init.lua`. You can clone one into a Neovim-style package directory under the data directory yourself, or let Craft install it from a Git repository and lock it to one commit.

A package directory holds sorted `plugin/*.lua` entry files, modules at `lua/<module>.lua` or `lua/<module>/init.lua`, and a `plugin.toml` manifest. The entry files share one environment and use the API this guide describes.

### Install from Git

Declare managed packages in the global `init.lua`, normally `~/.config/craft/init.lua`:

```lua
craft.pack.add({
  "https://github.com/example/craft-goal",
  {
    src = "https://github.com/example/craft-review",
    version = "v1.2.0",
  },
})
```

Each entry is a source string, or a table with `src`, `version`, `name`, and `data`. Craft derives the directory and owner name from `src`, and `name` overrides it when two sources end in the same repository name.

Craft shows all new packages in one install prompt, on the terminal, before the UI starts. It writes the selected Git commit to `pack-lock.json` in your config directory. Commit this file if you want another machine to install the same revisions.

Every project shares one lockfile and package directory, so a project `.craft/init.lua` cannot add packages. It can still read state with `craft.pack.get` and activate a package with `craft.packadd`.

Craft refuses a package name that matches a builtin plugin or a package you placed by hand, and reports the conflict at startup.

Set `confirm = false` only when the package source is already trusted and Craft must run without a terminal:

```lua
craft.pack.add({ "https://github.com/example/craft-goal" }, {
  confirm = false,
})
```

This option skips the install prompt. It does not approve package permissions. Craft rejects an HTTP source carrying a username, password, or token, since Git and the lockfile would store it. Use a credential helper or an SSH agent.

Set `load = false` to install a package without loading it at startup. Set `load` to a function when the package needs a custom entry point:

```lua
craft.pack.add({
  {
    src = "https://github.com/example/craft-review",
    data = { module = "review" },
  },
}, {
  load = function(package)
    require(package.spec.data.module).setup()
  end,
})
```

The function runs as the package owner. It receives the package `spec` and its installed `path`. The `data` field can contain any Lua value.

See `craft.pack.add` and `craft.pack.get` below for the full signatures.

### Pinned revisions

A lockfile entry wins over `version`: once Craft records a commit, it installs that commit everywhere, and a later `version` in `init.lua` changes nothing. To move a package, delete its entry from `pack-lock.json` and start Craft again. Craft resolves `version` and records the commit it picked.

A changed `src` makes the recorded revision meaningless, so Craft installs the new source and records it. That is also a new trust decision, so the install and permission prompts come back.

Removing a `craft.pack.add` entry stops the package from loading. Its lockfile entry and its checkout stay on disk until you delete them.

### Package permissions

A managed package can ask for guarded APIs in `plugin.toml`:

```toml
[permissions]
fs_read = true
fs_write = true
net = true
run = true
env = true
```

The manifest states what the package wants, and Craft asks about new permissions in a separate prompt. A package with no `plugin.toml` asks for nothing, and every guarded call it makes fails.

An approval applies only to the same package name and source. Craft keeps approvals in `<craft-data>/site/pack-approvals.json`, where `<craft-data>` is the data directory from the directory layout in the configuration docs. Approvals describe trust on this machine and must not be committed with `pack-lock.json`.

Only the interactive UI can ask. `--print`, SDK mode, the ACP server, and the other subcommands never prompt, so a package waiting for a decision comes back as a startup warning instead of loading.

### Install by hand

Clone a package into a Neovim-style package directory under the data directory:

```text
<craft-data>/site/pack/<group>/start/<name>/
<craft-data>/site/pack/<group>/opt/<name>/
```

A `start` package loads at startup. An `opt` package stays installed until something activates it.

Pick any `<group>` name except `core`, which Craft reserves for its own installs and never scans.

Placing a package by hand means you trust its code the way you trust your own `init.lua`. Craft grants it exactly the permissions its `plugin.toml` requests, with no approval step:

```toml
[permissions]
net = true
```

### Activate an installed package

An `opt` directory, or a managed package declared with `load = false`, starts with `craft.packadd`. Call it from `init.lua` or from another package:

```lua
craft.packadd("my-package")
```

Craft loads the named package after the calling Lua task returns, so it works from `init.lua` and from another package. A package activated this way can activate another in turn. Craft reports a name no installed package matches, and refuses a package that `plugins.<name>.enabled = false` disabled.

A package name also works in `craft.setup`: `plugins.<name>` can pass options to it, and `plugins.<name>.enabled = false` keeps it from loading.

### Managed checkouts

Craft restores a missing checkout only when the global config still declares the package and its source matches the lockfile entry. Installs and approval writes share one file lock, so two Craft processes cannot interleave them. The kernel releases that lock when the process exits, so a crash leaves nothing to clean up.

Each revision gets its own directory under `<craft-data>/site/pack/core/<name>/<commit>/`. At startup Craft deletes stale revisions, skipping any that a running process still holds.

### `craft.pack.add()`

Declare global packages. Available only in the global `init.lua`:

```lua
craft.pack.add({
  { src = "https://github.com/example/craft-goal", version = "main" },
}, {
  confirm = true,
  load = true,
})
```

`specs` is a list of sources or tables with `src`, `name`, `version`, and `data`. In `opts`, `confirm` controls the source confirmation prompt and `load` is a boolean or a custom loader function.

### `craft.pack.get()`

Read package state without changing the installed set. Available everywhere, including project config and packages:

```lua
local packages = craft.pack.get() -- all managed packages
local one = craft.pack.get({ "craft-goal" })[1]
```

Each record has `spec`, `path`, `rev`, and `active`.

### `craft.packadd()`

Activate an installed `opt` package, like `:packadd`. See [Activate an installed package](#activate-an-installed-package).

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

## `craft.model`

The model behind the focused session. Good for a keybind that flips between your two go-to models, or lifts thinking for one hard question. Without an interactive UI every function returns `nil, "no interactive UI attached"`.

- `craft.model.get()` reads the focused session's model, thinking level, and fast mode. Returns `{spec, id, provider, thinking, fast, supports_thinking, supports_fast}`, or nil and an error. The `thinking` string uses the same spelling `set` accepts, so a table from `get` can go straight back into `set`.
- `craft.model.available()` lists the model specs you can switch to: what the providers you are logged into offer, minus what your model policy blocks. The list fills in the background at startup, so right after launch it can still be empty.
- `craft.model.set(opts)` switches the model, thinking level, or fast mode. Pass a bare spec string, or a table with any of `spec` (`"provider/id"`), `thinking` (`"off"`, `"adaptive"`, an effort level like `"high"`, a token budget, or `""` to toggle), and `fast` (boolean). Fields you leave out stay as they are, so this doubles as a thinking-only switch. Returns the new state, or nil and an error.

```lua
craft.model.set("anthropic/claude-opus-4-6")
craft.model.set({ spec = "zai/glm-5", thinking = "high" })
craft.keymap.set("n", "<M-t>", function() craft.model.set({ thinking = "" }) end)
```

### `ModelChanged`

`craft.create_autocmd("ModelChanged", { callback = ... })` fires whenever the session switches model, whoever asked for it: the picker, `/model`, `craft.model.set`, a provider fallback, or loading another session. The payload carries `data.model` in the shape `craft.model.get()` returns, plus `data.previous_spec`. Picking the model already in use stays quiet, and so does startup.

```lua
craft.create_autocmd("ModelChanged", {
    callback = function(ev)
        craft.ui.set_window_title("craft: " .. ev.data.model.spec)
    end,
})
```

### `SessionEnd`

`craft.create_autocmd("SessionEnd", { callback = ... })` fires whenever a session goes away: a `/reset`, loading or opening another session, deleting a tab, quitting the TUI, closing an ACP session, and the end of a headless run. The payload carries `data.session_id` naming the session that ended. Use it to drop caches or state tied to that session. The session that replaces it announces itself with `SessionStart`.

## `craft.ui`

- `craft.ui.set_window_title(title)` sets the terminal emulator's window title. Pass an empty string to clear it. The title passes through tmux, GNU screen, and zellij untouched, and control characters are stripped, so model text cannot inject escape sequences into the terminal. On exit Craft hands the title back to the shell, on terminals that support the title stack.

```lua
craft.ui.set_window_title("craft: " .. session_name)
-- Give the title back to the shell:
craft.ui.set_window_title("")
```

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
