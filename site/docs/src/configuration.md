# Configuration

Configuration lives in [BML](https://crates.io/crates/barkml) format (`barkml`):

- `~/.config/craft.bml` — the main file, or
- `~/.config/craft/*.bml` — split files that merge together.

A legacy `~/.craft/` location is still searched. A missing file is an empty configuration; malformed files, unknown fields, invalid URLs, and invalid limits are errors. Loading never writes files or contacts a provider.

On first run, providers are auto-detected from credential environment variables when the config declares none.

A fully commented example ships as [`agent/agent.example.bml`](https://github.com/craft-build/craft/blob/main/agent/agent.example.bml) — it is the canonical reference for every key below.

## Agent settings

```bml
agent {
  # Extra instructions appended to the system prompt.
  preamble = ""
  # System prompt rendering: "markdown" (default) or "xml".
  # prompt_format = "xml"
  # Generation overrides; omitted keeps provider/model defaults.
  # temperature = 0.2
  # max_tokens = 8192
  # Default thinking for runs and inherited subagents.
  # thinking = "medium"
  # Bounds on a single run.
  # max_turns = 50
  # max_concurrent_tools = 8
  # Write durable facts to the project argosy after each run (default true).
  # memory_extraction = false
  # TUI tool-call rendering: "cards" or "compact".
  # tool_display = "compact"

  # Post-turn advisor: a lightweight reviewer per reply. Off by default.
  # advisor {
  #   enabled = true
  #   auto_act = "concern"   # off | nit | concern | blocker
  #   max_act_turns = 2
  # }
}
```

`always_thinking` (top level) seeds new-session thinking: `"off"`, `"adaptive"`, `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`, or a positive token budget. See [CLI → Thinking controls](./cli.md#thinking-controls).

`compaction_buffer` (top level) reserves context so compaction fires before the window fills: an absolute token count (≥ 1000) or a quoted percent. Default `"20%"`.

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

## Tool-output compression

Applied to the model's request view; history keeps raw outputs. These are the defaults:

```bml
# compression {
#   enabled = true
#   code_compression_rate = 0.3
#   max_log_lines = 50
#   max_search_files = 20
#   max_matches_per_file = 5
#   max_diff_lines = 100
#   max_json_items = 15
#   json_first_keep = 5
#   json_last_keep = 3
#   protect_recent_tool_outputs = 2
# }
```

## Sandbox

```bml
# sandbox {
#   enabled = true
#   mode = "workspace_write"   # read_only | danger_full_access | off
#   network = true
# }
```

See [Sandbox](./sandbox.md).

## Codemode

Python code execution: the `code` tool runs a scripted sub-agent that calls the same tool set from one Python script. These are the defaults — `mode = "off"` keeps the tool unregistered:

```bml
# codemode {
#   mode = "off"            # off | alongside | exclusive
#   timeout_secs = 30
#   wall_clock_secs = 600
#   max_tool_calls = 200
#   max_output_kib = 32
#   telemetry = true
# }
```

`--codemode <off|alongside|exclusive>` overrides the mode per run and the `/codemode` command toggles it per session (see [CLI](./cli.md)); `type_check` is reserved and rejected. The `code` tool itself: [Tools](./tools.md#execution).

## Tool timeouts

The timeouts `bash` and `webfetch` use when a call omits `timeout`. Per-call values still win. These are the defaults:

```bml
# bash {
#   default_timeout_secs = 120       # bash default; minimum 5
#   default_watch_timeout_secs = 60  # bash_watch default; minimum 5
# }
# webfetch {
#   timeout_secs = 30      # default request timeout
#   max_timeout_secs = 120 # cap for per-call timeouts too
#   max_retries = 3        # transport errors and 5xx; maximum 10
# }
```

## Keybinds

TUI keybindings, action id → chord list. An empty list disables the action; unknown ids are warned and dropped.

```bml
# keybind "palette" {
#   keys = ["ctrl+p", "ctrl+k"]
# }
```

Actions: `quit`, `help`, `palette`, `sidebar`, `model_menu`, `search`, `file_picker`, `plan_toggle`, `open_editor`, `scroll_half_up`, `scroll_half_down`, `delete_word`, `kill_line`, `line_start`, `line_end`, `edit_input`, `cycle_effort`, `paste_image`, `task_chat_picker`.

## Providers

Each `[providers.<alias>]` block chooses a `kind` with per-alias credentials, base URLs, and model catalogs. Multiple aliases may use the same kind. See [Providers](./providers.md):

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

Permission rules live in a separate `permissions.bml` next to the config file, or in a project's `.craft/` directory. The same `permissions` block also holds `auto_review = true` — starting every session with the auto-review reviewer on. See [Permissions & Snapshots](./permissions.md).
