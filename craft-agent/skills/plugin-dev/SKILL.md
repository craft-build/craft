---
name: plugin-dev
description: Write or modify craft plugins or init.lua config in Lua. Custom tools, slash commands, keymaps, prompt hints, UI. Authoring guide and a worked example built from the bundled glob tool. Load before any craft plugin work.
when_to_use: When the user asks you to write, edit, debug, or review a craft Lua plugin, an init.lua config, a custom tool, slash command, keymap, prompt hint, or any code that calls the craft.* Lua API.
---

# Writing craft plugins

Craft plugins are plain Lua files (Luau) that run inside craft. A plugin can register tools the LLM calls, slash commands, keymaps, and prompt hints. Everything lives under the global `craft` table.

## Where plugin code goes

- `~/.config/craft/init.lua` - global, loaded for every project
- `<project>/.craft/init.lua` - project-local

Either file can call `craft.setup({ ... })` for configuration and register tools or commands. There are no separate plugin directories yet; bundled plugins ship inside the binary.

## Development loop

You cannot run slash commands or restart craft. After editing, ask the user to run `/reload` (rebuilds plugins and config in place). Until then your changes are not live. If a backtrace comes out useless, suggest restarting with `--no-jit`.

To debug, add `craft.log.info|warn|error(...)` calls. They write to `craft.log` in the directory `craft.env.logs_dir()` returns (Linux: `~/.local/logs/craft/`). Read that file yourself after the user reloads and reproduces.

## Conventions

- Fallible runtime calls return a `(value, err)` pair; check `err` before using `value`.
- Tool handlers report failures with `{ llm_output = "error: ...", is_error = true }`, not by raising.
- The model picks tools by reading `description`, so state precisely what the tool does and when to use it.
- Reusable helpers ship with craft under `craft.<module>`: `craft.truncate`, `craft.tool_view`, `craft.shorten_path`, `craft.output_limits`, `craft.list_picker`, `craft.text_input`, `craft.color`, `craft.image_uri`. `require` them from any plugin.

## The API surface

Top-level modules under the global `craft` table:

- `craft.setup({config})` - apply configuration; call once per init.lua.
- `craft.split({s}, {sep}, {opts?})` - split a string.
- `craft.api.register_tool({spec})` - register a new tool the agent can call.
- `craft.api.register_command({spec})` - register a slash command that appears in the input bar.
- `craft.api.register_prompt_hint({spec})` - add a piece of text to an aggregate prompt slot.
- `craft.api.register_options({spec})` - declare the options your plugin accepts under `plugins.<name>`; returns a table with the resolved values.
- `craft.api.create_autocmd({event}, {opts})` - listen for one or more events.
- `craft.fs` - file-system utilities (`read`, `write`, `glob`, `grep`, `dir`, `metadata`, `joinpath`, `parents`, `root`, ...), modelled after `vim.fs`.
- `craft.uv` - libuv helpers (`cwd`, `os_homedir`, ...), modelled after `vim.uv`.
- `craft.fn` - process helpers (`jobstart`, `jobstop`, `jobwait`, `executable`), modelled after `vim.fn`.
- `craft.env` - craft's own directories: `state_dir()`, `config_dir()`, `logs_dir()`, `legacy_dir()`, `tmpdir()`.
- `craft.ui` - UI primitives: `craft.ui.buf()` builds a body buffer; methods `buf:line(spans)`, `buf:on(event, fn)`, etc. Theme colors via `craft.ui.theme_color(name)`.
- `craft.keymap` - keymap registration, mirrors Neovim's `vim.keymap`.
- `craft.treesitter` - tree-sitter helpers (`parse`, language query API).
- `craft.async` - coroutine helpers (`run`, `await`, `wrap`, `join`, `on_cancel`); `on_cancel(fn)` runs `fn` the moment the current task is cancelled, before the host waits for it to wind down.
- `craft.base64` - Base64 `encode` / `decode`.
- `craft.image` - image probe and decode helpers.
- `craft.json` / `craft.yaml` - encode/decode (`(value, err)` pair).
- `craft.log` - structured logging (`info`, `warn`, `error`).
- `craft.split` - string split helper.

## Tool spec shape

A tool spec passed to `craft.api.register_tool` has:

- `name` (string, required) - unique tool name.
- `kind` (string) - free-form category hint shown in the UI (e.g. `"read"`, `"search"`, `"edit"`, `"bash"`).
- `description` (string, required) - read by the model; state what the tool does and when to use it.
- `schema` (table) - JSON-schema-shaped table; `type = "object"`, `properties = { ... }`.
- `header = function(input) -> buf` - render the tool's header line in the UI.
- `restore = function(input, output, is_error, ctx) -> buf` - rebuild the body from saved output on replay (click-to-expand, etc.).
- `handler = function(input, ctx) -> result` - run the tool; returns `{ llm_output = ..., body = buf?, header = buf?, is_error = true? }`.

`ctx` is the per-call context. Common methods: `ctx:tool_output_lines()` returns the per-tool UI line budget; `ctx:config(name, default)` reads resolved plugin options.

## A complete real example

The bundled `glob` tool, verbatim: options registration, schema, header and restore hooks, error handling, LLM output truncation, collapsible UI view:

```lua
local truncate = require("craft.truncate")
local ToolView = require("craft.tool_view")
local shorten_path = require("craft.shorten_path")
local output_limits = require("craft.output_limits")

local NO_FILES_FOUND = "No files found"

local opts = craft.api.register_options(output_limits.extend({
  search_result_limit = { default = 100, min = 10, desc = "Max files returned per search." },
}))

local function glob_view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.other) or 3, keep = "head" }
end

craft.api.register_tool({
  name = "glob",
  kind = "search",
  description = [[Find files by glob pattern.

- Respects .gitignore.
- Returns absolute paths sorted by modification time (newest first).
- Prefer speculative parallel searches over sequential rounds of glob+grep.]],

  schema = {
    type = "object",
    properties = {
      pattern = { type = "string", description = "Glob pattern (e.g. **/*.rs, src/**/*.ts)" },
      path = { type = "string", description = "Directory to search in (default: cwd)" },
    },
  },

  header = function(input)
    local buf = craft.ui.buf()
    local spans = { { shorten_path(input.pattern or ""), "tool" } }
    if input.path then
      spans[#spans + 1] = { " in ", "dim" }
      spans[#spans + 1] = { shorten_path(input.path), "path" }
    end
    buf:line(spans)
    return buf
  end,

  restore = function(_input, output, _is_error, ctx)
    return ToolView.restore(output, glob_view_opts(ctx))
  end,

  handler = function(input, ctx)
    local pattern = input.pattern
    if not pattern then
      return { llm_output = "error: pattern is required", is_error = true }
    end

    local limit = opts.search_result_limit
    local max_lines, max_bytes = output_limits.resolve(opts, ctx)

    local files, err = craft.fs.glob(pattern, {
      path = input.path,
      gitignore = true,
      sort = "mtime",
      limit = limit,
    })

    if not files then
      return { llm_output = "error: " .. err, is_error = true }
    end

    if #files == 0 then
      return { llm_output = NO_FILES_FOUND }
    end

    local lines = {}
    for i, f in ipairs(files) do
      lines[i] = shorten_path(f)
    end
    local text = table.concat(lines, "\n")
    local llm_output = truncate(text, max_lines, max_bytes)

    local buf = craft.ui.buf()
    local view = ToolView.new(buf, glob_view_opts(ctx))
    for _, line in ipairs(lines) do
      view:append(line)
    end
    view:finish()
    buf:on("click", function()
      view:toggle()
    end)

    return {
      llm_output = llm_output,
      body = buf,
    }
  end,
})
```

## Reference: read the source

The full, authoritative Lua API is the craft source itself. To confirm a signature or option before you use it, do not guess; read the implementation:

- `craft-lua/src/api/*.rs` defines every module and function. For example `craft-lua/src/api/fs.rs` for `craft.fs`, `craft-lua/src/api/tool.rs` for `craft.api.register_tool`, `craft-lua/src/api/env.rs` for `craft.env`.
- `plugins/lib/craft/*.lua` defines the shared helper modules (`craft.truncate`, `craft.tool_view`, `craft.shorten_path`, `craft.output_limits`, `craft.list_picker`, `craft.text_input`, `craft.color`, `craft.image_uri`).
- `plugins/<name>/init.lua` for each bundled plugin is a worked example of a tool, slash command, or prompt hint in context.

Before using a function you are not certain about, grep the source for its name and read the surrounding doc comments. Never guess a signature, option table, or return shape from memory.
