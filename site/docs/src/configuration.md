# Configuration

Settings go in `init.lua`, a Lua script that calls `craft.setup()`. Same language as plugins.

Two places, both optional:

- **Global**: `~/.config/craft/init.lua`
- **Project**: `.craft/init.lua` (relative to your working directory)

When both exist, project settings override global ones. Neither file is required.

## Example

```lua
craft.setup({
    ui = {
        splash_animation = true,
        mouse_scroll_lines = 5,
        tool_output_lines = {
            bash = 8,
            read = 5,
        },
    },
    agent = {
        bash_timeout_secs = 180,
        max_output_lines = 3000,
    },
    provider = {
        default_model = "anthropic/claude-sonnet-4-6",
    },
    storage = {
        max_log_files = 5,
    },
})
```

All fields are optional. Typos in field names cause an error right away.

`craft.setup()` can only be called once per init.lua.

## Full Reference

### Top-level

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `always_yolo` | bool | `false` | Start every session with YOLO mode (skip permission prompts, deny rules still apply) |
| `always_fast` | bool | `false` | Start every session with Anthropic fast mode (Opus only; ignored otherwise) |
| `always_thinking` | bool | string | `false` | Start every session with extended thinking (true/"adaptive", "off", or a token budget) |

### `ui`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `splash_animation` | bool | `true` | - | Show splash animation on startup |
| `scrollbar` | bool | `true` | - | Show vertical scrollbar in scrollable areas |
| `flash_duration_ms` | u64 | `1500` | - | Duration of flash messages (ms) |
| `typewriter_ms_per_char` | u64 | `4` | - | Typewriter effect speed (ms/char) |
| `mouse_scroll_lines` | u32 | `3` | 1 | Lines per mouse wheel scroll |

### `ui.tool_output_lines`

How many lines of output to show per tool in the UI. All values are `usize` with a minimum of 1.

| Field | Default |
|-------|---------|
| `bash` | 5 |
| `code_execution` | 5 |
| `task` | 5 |
| `grep` | 3 |
| `read` | 3 |
| `write` | 7 |
| `web` | 3 |
| `other` | 3 |

### `agent`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_output_bytes` | usize | `51200` | 1024 | Max tool output size (bytes) |
| `max_output_lines` | usize | `2000` | 10 | Max tool output lines |
| `max_response_bytes` | usize | `5242880` | 1024 | Max LLM response size (bytes) |
| `max_line_bytes` | usize | `3000` | 80 | Max bytes per line before truncation |
| `bash_timeout_secs` | u64 | `120` | 5 | Bash command timeout (seconds) |
| `code_execution_timeout_secs` | u64 | `30` | 5 | Code execution timeout (seconds) |
| `max_continuation_turns` | u32 | `3` | 1 | Max automatic continuation turns |
| `compaction_buffer` | u32 | `40000` | 1000 | Token buffer reserved during compaction |
| `search_result_limit` | usize | `100` | 10 | Max results from grep/glob searches |
| `interpreter_max_memory_mb` | usize | `50` | 10 | Memory limit for code interpreter (MB) |

### `agent.validation`

Run a project-level compile check after the agent writes files. Disabled by default.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable post-write compile validation |
| `command` | string | `none` | Custom validation command, overriding the detected project command |
| `max_iterations` | u8 | `3` | Max validation retry iterations |
| `timeout_secs` | u64 | `30` | Validation command timeout (seconds) |

### `agent.format`

Auto-format files in place after the agent writes them, before the compile check. Runs the formatter mapped to each file's extension, for example `rustfmt` for `.rs` and `prettier --write` for `.ts` or `.json`. A missing formatter is silently skipped. Set `command` to run one custom command for every formattable file.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable post-write auto-formatting |
| `command` | string | `none` | Custom formatter command, overriding the extension table |
| `timeout_secs` | u64 | `15` | Formatter command timeout (seconds) |

### `provider`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `default_model` | String | `none` | - | Default model identifier (e.g. `anthropic/claude-sonnet-4-6`) |
| `connect_timeout_secs` | u64 | `10` | 1 | HTTP connect timeout (seconds) |
| `low_speed_timeout_secs` | u64 | `120` | 1 | Low speed timeout (seconds with less than 1 byte received) |
| `stream_timeout_secs` | u64 | `300` | 10 | Streaming response timeout (seconds) |

### `storage`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_log_bytes_mb` | u64 | `200` | 1 | Max total log size (MB) |
| `max_log_files` | u32 | `10` | 1 | Max number of log files to keep |
| `input_history_size` | usize | `100` | 10 | Number of input history entries to retain |

### `compression`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `enabled` | bool | `true` | - | Enable tool output compression |
| `max_log_lines` | usize | `50` | 10 | Max lines in compressed log output |
| `max_search_files` | usize | `20` | 5 | Max files in compressed search output |
| `max_matches_per_file` | usize | `5` | 1 | Max matches per file in search output |
| `max_diff_lines` | usize | `100` | 10 | Max lines in compressed diff output |
| `max_json_items` | usize | `15` | 5 | Max items in compressed JSON array output |
| `protect_recent_tool_outputs` | usize | `2` | 1 | Never compress the last N tool outputs |
| `semantic_enabled` | bool | `false` | - | Enable semantic relevance scoring (requires onnx feature) |

### `sandbox`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable sandbox restrictions on tools |
| `mode` | string | `"workspace_write"` | Sandbox mode. One of: `workspace_write`, `read_only`, `danger_full_access`, `off` |
| `network` | bool | `true` | Allow network access in sandboxed tools |

## Tools

The `tools` table lets you turn tools on or off. By default `webfetch` and `websearch` are on. `bash` is off by default.

```lua
craft.setup({
    tools = {
        bash = { enabled = true },
        websearch = { enabled = false },
    },
})
```

## Validation

If a value is below its minimum, Craft shows a `ConfigError` with the field name, value, and minimum.

## Directory layout

Craft uses XDG directories on Linux and macOS:

| Purpose | Path |
|---------|------|
| Config | `~/.config/craft/` (init.lua, permissions.toml, mcp.toml) |
| Data | `~/.local/share/craft/` |
| Logs | `~/.local/logs/craft/` |
| State | `~/.local/state/craft/` |

`~/.craft/` is checked as a legacy fallback.

## Personal Instructions

On top of `AGENTS.md`, you can add your own instructions in two places:

- `AGENTS.local.md` at project root for per-project preferences (gitignored)
- `~/.config/craft/AGENTS.md` for preferences that apply to all projects

Both are added to the system prompt at the start of every session.

## Migrating from config.toml

Still have a `config.toml`? Here is how to switch over.

**Rename your config files:**

```
~/.config/craft/config.toml  ->  ~/.config/craft/init.lua
.craft/config.toml           ->  .craft/init.lua
```

**Wrap the content in `craft.setup()`:**

Before:

```toml
[agent]
bash_timeout_secs = 180
```

After:

```lua
craft.setup({
    agent = { bash_timeout_secs = 180 },
})
```

Same field names, just Lua syntax instead of TOML.

**Move MCP sections to `mcp.toml`.**

- `~/.config/craft/mcp.toml` (global)
- `.craft/mcp.toml` (per-project)

Same format, just a different file. See [MCP](./mcp.md).

**Permissions stay in `permissions.toml`.**
