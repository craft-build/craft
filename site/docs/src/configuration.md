# Configuration

Craft uses [BML](https://crates.io/crates/barkml) configuration files. Start with either layout:

- `~/.config/craft.bml` — the main file, or
- `~/.config/craft/*.bml` — split files that merge together.

A legacy `~/.craft/` location is still searched. A missing file is an empty configuration; malformed files, unknown fields, invalid URLs, and invalid limits are errors. Loading never writes files or contacts a provider.

On first run, providers are auto-detected from credential environment variables when the config declares none.

A fully commented example ships as [`agent/agent.example.bml`](https://github.com/craft-build/craft/blob/main/agent/agent.example.bml). Copy only the blocks you need; omitted settings keep their defaults.

## Minimal configuration

If provider auto-detection is enough, you can skip this step. To configure a provider explicitly, create `~/.config/craft.bml`:

```bml
provider "anthropic" {
  kind = "anthropic"
  api_key_env = "ANTHROPIC_API_KEY"
}
```

Set the named variable in your shell before starting Craft. See [Providers](./providers.md) for local servers and other services.

BML uses named blocks such as `provider "anthropic" { ... }` and nested blocks such as `model "id" { ... }`. The examples below belong in the same configuration file unless a different path is given.

## Agent settings

```bml
agent {
  preamble = "Run the relevant tests after changing code."
  max_concurrent_tools = 8
  tool_display = "compact"
}
```

### Instructions and generation

- `preamble` adds instructions to the system prompt. The default is an empty string, which keeps the built-in instructions.
- `prompt_format` selects `"markdown"` (default) or `"xml"` for system prompt rendering.
- `temperature` overrides the provider/model's sampling default when set.
- `max_tokens` sets a positive output-token cap for requests. Omit it to keep the provider/model default; this is different from model catalog metadata.
- `thinking` sets the run default and the preference inherited by subagents. See [Thinking controls](./cli.md#thinking-controls) for accepted values and provider limitations.

### Run limits

- `max_turns` limits model calls in a run, not the number of saved messages. Omit it to keep the execution mode's default: the TUI is unbounded, while headless runs use their own default. `--max-turns` overrides it for a run.
- `max_concurrent_tools` limits tools executing at once, including `batch` children. The default is `8`.

### Memory and display

- `memory_extraction` defaults to `true`. Set it to `false` to stop writing durable facts to the project's [Argosy memory](./argosy.md#memory-extraction) after successful runs.
- `tool_display` controls the TUI only: `"cards"` (default) gives each tool call a card; `"compact"` groups calls and shows read-only tools as one-line entries.

### Post-turn advisor

The advisor reviews each reply and emits at most one note. It is separate from the permission reviewer described in [Auto-review mode](./permissions.md#auto-review-mode).

```bml
agent {
  advisor {
    enabled = true
    auto_act = "concern"
    max_act_turns = 2
  }
}
```

The advisor is disabled by default. `auto_act` sets the minimum note severity that triggers an automatic follow-up: `"nit"`, `"concern"` (default), or `"blocker"`. Use `"off"` to report notes without acting on them. `max_act_turns` bounds automatic follow-ups; its default is `2`.

## New-session thinking

`always_thinking` is a **top-level** setting, outside the `agent` block:

```bml
always_thinking = "low"
```

It seeds new-session thinking with `"off"`, `"adaptive"`, `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`, or a positive token budget. Use `/thinking` to change the TUI session preference. See [Thinking controls](./cli.md#thinking-controls).

## Compaction stages

```bml
compaction "vcc" {
  context = "0.6"
}
compaction "llm" {
  context = "0.8"
}
```

Stages run ascending by context fill ratio. This pair is the default — omit the blocks to keep it. Details in [Context Management](./context.md).

The top-level `compaction_buffer` reserves space so compaction starts before the window fills. Set it to an absolute token count of at least `1000`, or a quoted percentage. The default is `"20%"`:

```bml
compaction_buffer = "20%"
```

## Tool-output compression

Applied to the model's request view; history keeps raw outputs. These are the defaults:

```bml
compression {
  enabled = true
  code_compression_rate = 0.3
  max_log_lines = 50
  max_search_files = 20
  max_matches_per_file = 5
  max_diff_lines = 100
  max_json_items = 15
  json_first_keep = 5
  json_last_keep = 3
  protect_recent_tool_outputs = 2
}
```

`enabled` turns compression on or off. `code_compression_rate` controls code compression; the `max_*` settings bound retained excerpts by content type. `json_first_keep` and `json_last_keep` control how many items to keep at either end of a compressed JSON array. `protect_recent_tool_outputs` leaves the most recent N tool outputs uncompressed.

## Sandbox

```bml
sandbox {
  enabled = true
  mode = "workspace_write"
  network = true
}
```

These are the defaults. `mode` also accepts `"read_only"`, `"danger_full_access"`, or `"off"`. Set `network = false` to block sandboxed network access. See [Sandbox](./sandbox.md) for platform requirements and the limits of confinement.

## Codemode

Codemode lets the agent call tools from a Python script using the `code` tool. It is disabled by default. To make it available alongside normal tool calls:

```bml
codemode {
  mode = "alongside"
  timeout_secs = 30
  wall_clock_secs = 600
  max_tool_calls = 200
  max_output_kib = 32
  telemetry = true
}
```

- `mode` is `"off"` (default; no `code` tool), `"alongside"` (both entry styles), or `"exclusive"` (only the code entry style).
- `timeout_secs` sets the Python execution timeout, from `1` to `120` seconds; default `30`.
- `wall_clock_secs` bounds the whole invocation, including tool calls; default `600`.
- `max_tool_calls` caps tool calls from one script; default `200`.
- `max_output_kib` caps script output in KiB; default `32`.
- `telemetry` enables codemode telemetry; default `true`.

`--codemode <off|alongside|exclusive>` overrides the mode for a run; `/codemode` changes it for a TUI session. `type_check` is reserved and cannot currently be enabled. See [Tools](./tools.md#execution).

## Tool timeouts

The timeouts `bash` and `webfetch` use when a call omits `timeout`. Per-call values still win. These are the defaults:

```bml
bash {
  default_timeout_secs = 120
  default_watch_timeout_secs = 60
}
webfetch {
  timeout_secs = 30
  max_timeout_secs = 120
  max_retries = 3
}
```

`bash.default_timeout_secs` and `bash.default_watch_timeout_secs` must each be at least `5` seconds.

`webfetch.timeout_secs` is the default request timeout. `max_timeout_secs` caps both the default and per-call overrides. `max_retries` applies to transport errors and HTTP 5xx responses, and cannot exceed `10`.

## Keybinds

Use a named `keybind` block to assign one or more keyboard shortcuts to a TUI action. This example lets either Ctrl-P or Ctrl-K open the command palette:

```bml
keybind "palette" {
  keys = ["ctrl+p", "ctrl+k"]
}
```

Set `keys = []` to disable an action. Unknown action IDs produce a warning and are ignored.

Available action IDs:

- **Navigation:** `quit`, `help`, `palette`, `sidebar`, `model_menu`, `search`, `file_picker`, `task_chat_picker`.
- **Agent controls:** `plan_toggle`, `cycle_effort`.
- **Input editing:** `open_editor`, `edit_input`, `delete_word`, `kill_line`, `line_start`, `line_end`, `paste_image`.
- **Transcript scrolling:** `scroll_half_up`, `scroll_half_down`.

## Providers

Each `provider "alias"` block chooses a `kind` and can set credentials, a base URL, and model metadata. The alias is the name used in `--model alias/model-id`; multiple aliases may use the same kind. See [Providers](./providers.md):

```bml
provider "openai" {
  kind = "openai"
  api_key_env = "OPENAI_API_KEY"

  model "gpt-5.2" {
    name = "GPT-5.2"
    context_length = 400000
    max_output_tokens = 32768
  }
}
```

## MCP servers

Stdio and HTTP transports, merged from the global config and the project's `.craft/mcp.bml` (project wins on name conflicts). `${VAR}` in environment/headers values expands from the process environment. See [MCP](./mcp.md):

```bml
mcp "filesystem" {
  command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
  environment = { API_TOKEN = "${MY_MCP_TOKEN}" }
  timeout = 30000
}
```

## Permissions

Global rules belong in a `permissions` block in the global configuration; project rules go in `.craft/permissions.bml` with tool blocks at the top level.

Set `auto_review = true` inside the global `permissions` block, or at the top level of the project permissions file, to enable automated permission review for TUI and headless sessions. ACP still sends approval requests to the client. See [Permissions & Snapshots](./permissions.md) for examples and limitations.
