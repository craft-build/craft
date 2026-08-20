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
        theme = "tokyonight",
        tool_output_lines = {
            bash = 8,
            read = 5,
        },
        keybindings = {
            search = "Ctrl+F",
            plan_toggle = {"Ctrl+T", "Alt+T"},
            tasks = {},  -- disable
        },
    },
    agent = {
        max_output_lines = 3000,
    },
    provider = {
        default_model = "anthropic/claude-sonnet-4-6",
        allowed_models = { "anthropic/*", "openai/gpt-5" },
        excluded_models = { "*/*-preview" },
    },

    storage = {
        max_log_files = 5,
    },
    plugins = {
        bash = { timeout_secs = 180 },
    },
})
```

All fields are optional. Typos in field names cause an error right away.

`provider.allowed_models` is a list of glob patterns for qualified `provider/model-id` specs. `*` also matches `/`, so `opencode/*` includes nested model IDs. When the list is empty or omitted, every model is allowed. `provider.excluded_models` removes matching models after that, so exclusions always win. A project list replaces the matching global list; omit it to inherit or use `{}` to clear it. The policy applies to selectors, CLI and API model changes, delegation, and `craft models`.

`craft.setup()` can only be called once per init.lua.

## Full Reference
 
### Top-level

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `always_yolo` | bool | `false` | Start every session with YOLO mode (skip permission prompts, deny rules still apply) |
| `always_auto_review` | bool | `false` | Start every session with auto-review (an LLM auto-decides allow/deny on permission prompts instead of asking) |
| `always_fast` | bool | `false` | Start every session with Anthropic fast mode (Opus only; ignored otherwise) |
| `always_thinking` | bool \| string | `false` | Start every session with extended thinking (true/"adaptive", "off", an effort level ("minimal" to "max"), or a token budget) |

### `ui`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `splash_animation` | bool | `true` | - | Show splash animation on startup |
| `scrollbar` | bool | `true` | - | Show vertical scrollbar in scrollable areas |
| `notifications` | string | `auto` | - | Terminal notification method: auto, osc9, bell, or off |
| `flash_duration_ms` | u64 | `1500` | - | Duration of flash messages (ms) |
| `typewriter_ms_per_char` | u64 | `4` | - | Typewriter effect speed (ms/char) |
| `mouse_scroll_lines` | u32 | `3` | 1 | Lines per mouse wheel scroll |
| `max_input_lines` | u32 | `20` | 1 | Maximum visible input lines |
| `show_thinking` | bool | `true` | - | When true (default), show full model reasoning live and persisted. When false, hide reasoning behind an indicator (thinking> ...) with a click-to-expand hint, both while thinking and after it completes |
| `clock_format` | String | `system` | - | Clock format for timestamps: "12h", "24h", or "system" (follow the OS preference, 24h when unknown) |

### `ui.theme`

Name of the color theme to load at startup, overriding the theme you last picked interactively. If unset, Craft keeps your last selection (the built-in default on first run). An unknown name is ignored with a warning.

Available themes: `craft`, `ayu_dark`, `ayu_light`, `ayu_mirage`, `carbonfox`, `catppuccin_frappe`, `catppuccin_latte`, `catppuccin_macchiato`, `catppuccin_mocha`, `dracula`, `everforest_dark`, `fleet_dark`, `github_dark`, `gruvbox`, `gruvbox_light`, `kanagawa`, `material_darker`, `monokai_pro`, `night_owl`, `nightfox`, `nord`, `onedark`, `rose_pine`, `rose_pine_dawn`, `rose_pine_moon`, `solarized_dark`, `solarized_light`, `tokyonight`, `vscode_dark_plus`, `zenburn`.

You can add your own themes too. Drop a `<name>.toml` file into `themes/` inside your Craft config directory, for example `~/.config/craft/themes/`. If it reuses a built-in name, yours wins.

Themes use 24-bit colors, but not every terminal can show them. Craft checks the environment, terminfo, and the terminal itself, and when truecolor is missing it quietly falls back to the closest of the 256 classic terminal colors. If detection gets it wrong, set `CRAFT_TRUECOLOR=1` to force truecolor or `CRAFT_TRUECOLOR=0` to force the fallback.

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

### `ui.keybindings`

Override the default keybindings. Keys are snake_case action ids; values are a chord string, a list of chords, or an empty list to disable. Unknown ids and unparseable chords are warned and dropped.

| Action | Chord format |
|--------|--------------|
| `<action>` | `"Ctrl+P"`, `"Alt+M"`, `"Shift+Tab"`, `"F5"`, or a list |

Remappable actions:

| Action id |
|-----------|
| `quit` |
| `help` |
| `prev_chat` |
| `next_chat` |
| `scroll_half_up` |
| `scroll_half_down` |
| `scroll_line_up` |
| `scroll_line_down` |
| `scroll_to_top` |
| `scroll_to_bottom` |
| `pop_queue` |
| `delete_word` |
| `search` |
| `file_picker` |
| `open_editor` |
| `plan_toggle` |
| `tasks` |
| `suspend` |
| `delete` |
| `kill_line` |
| `line_start` |
| `line_end` |
| `edit_input` |

### `agent`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_output_bytes` | usize | `51200` | 1024 | Max tool output size (bytes) |
| `max_output_lines` | usize | `2000` | 10 | Max tool output lines |
| `max_line_bytes` | usize | `3000` | 80 | Max bytes per line before truncation (read tool) |
| `code_execution_timeout_secs` | u64 | `30` | 5 | Code execution timeout (seconds) |
| `max_continuation_turns` | u32 | `3` | 1 | Max automatic continuation turns |
| `compaction_buffer` | u32 \| string | `20%` | - | Context reserved for compaction: token count or percent of the context window (e.g. "20%") |
| `compaction_instructions` | String | `none` | - | Extra instructions appended to the compaction summary prompt |
| `post_compaction_instructions` | String | `none` | - | Extra instructions the agent receives after any compaction (e.g. re-read plan.md) |
| `interpreter_max_memory_mb` | usize | `50` | 10 | Memory limit for code interpreter (MB) |
| `stale_read_check` | bool | `true` | - | Require re-reading a file that changed on disk before editing it |

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

### `agent.small_model`

Route turns to a cheaper, lower-latency model when the conversation is running on a small-context model (auto-detected from the context window). Off by default; when `enabled` is true it always activates, otherwise it activates automatically for models whose context window is below `auto_detect_context_window`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Force small-model mode on (skips auto-detection) |
| `reduced_tools` | bool | `false` | Advertise a smaller tool set to save tokens |
| `compact_prompt` | bool | `false` | Use a shorter system prompt |
| `aggressive_truncation` | bool | `false` | Truncate tool output more aggressively |
| `compaction_threshold` | number | `0.50` | Fraction of context window that triggers compaction |
| `forgiving_parsing` | bool | `true` | Tolerate minor model output formatting errors |
| `auto_detect_context_window` | u32 | `32000` | Context window cutoff (in tokens) below which small-model mode auto-activates |

### `agent.advisor`

An always-on lightweight reviewer that reads the transcript delta each turn and emits at most one deduplicated note. Distinct from the `judge` goal-completion check and the on-demand `review` subagent. Off by default.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable the turn-by-turn advisor reviewer |
| `model` | string | `none` | `provider/model_id` spec for the advisor. When unset, the `advisor` role from `model_roles.toml` is used, falling back to the active model |
| `dedup_size` | usize | `16` | Maximum advisor notes kept in the dedup FIFO |
| `auto_act` | string | `concern` | Minimum severity (`off`, `nit`, `concern`, `blocker`) that triggers an automatic follow-up turn. At or above this severity the note is pushed into the agent context and the run continues; `off` keeps the advisor display-only |
| `max_act_turns` | u32 | `2` | Maximum advisor-driven follow-up turns a single run may take before stopping and displaying the note |

### `agent.flow`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable the Flow multi-stage pipeline |
| `max_review_iterations` | u32 | `3` | How many times Review can send a chunk back to Execute |
| `max_qa_iterations` | u32 | `2` | How many times QA can send a chunk back to Execute |
| `parallel_chunks` | u32 | `1` | Chunks to run at once |

### `agent.ttsr`

Time-traveling stream rules. When enabled, rules loaded from `.craft/rules/*.md` (lines prefixed with `rule:`) are matched against the in-flight stream text each turn, and a firing rule injects a system reminder. Off by default.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable time-traveling stream rules |

### `agent.compaction`

Per-model compaction thresholds. When set, override the global `agent.compaction_buffer` and `agent.compaction_threshold` for matching models. Lookup order: exact `provider/model_id` key, then bare `model_id`, then `global_threshold`. Either `reserve_tokens` (absolute) or `compact_percent` (percentage of the context window to compact at) may be set on a threshold; `reserve_tokens` wins when both are present.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `model_thresholds` | table<string, ModelThreshold> | `{}` | Map of `provider/model_id` (or bare `model_id`) to per-model threshold |
| `global_threshold` | ModelThreshold? | `nil` | Fallback threshold applied when no model-specific entry matches |

Each `ModelThreshold` may set:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `reserve_tokens` | u32? | `nil` | Absolute tokens to reserve; compacts once remaining context drops below this |
| `compact_percent` | u8? | `nil` | Compact at this percentage of the context window (1-99) |
| `keep_recent_tokens` | u32? | `nil` | Advisory token budget for the preserved tail (reserved for future use) |

### `provider`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `default_model` | String | `none` | - | Default model identifier (e.g. `anthropic/claude-sonnet-4-6`) |
| `allowed_models` | string[] | `[]` | - | Glob patterns for permitted qualified model specs; empty permits all models |
| `excluded_models` | string[] | `[]` | - | Glob patterns for excluded qualified model specs; exclusions take precedence |
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

### `sandbox`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable sandbox restrictions on tools |
| `mode` | string | `"workspace_write"` | Sandbox mode. One of: `workspace_write`, `read_only`, `danger_full_access`, `off` |
| `network` | bool | `true` | Allow network access in sandboxed tools |

## Plugins

The `plugins` table turns plugins on or off and passes options to them. All bundled plugins are on by default. Set `enabled = false` to turn one off.

Each plugin checks its own options at startup. A typo, a wrong type, or an unknown plugin name gives you a clear error right away. The old `tools` table is gone. If your config still uses it, Craft stops at startup and shows you the new form.

```lua
craft.setup({
    plugins = {
        bash = { timeout_secs = 180 },
        websearch = { enabled = false },
    },
})
```

### `plugins.bash`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_output_bytes` | integer | - | - | Override `agent.max_output_bytes` for this tool. |
| `max_output_lines` | integer | - | - | Override `agent.max_output_lines` for this tool. |
| `timeout_secs` | integer | `120` | 5 | Kill the command after this many seconds. A call's `timeout` param overrides it. |

### `plugins.glob`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_output_bytes` | integer | - | - | Override `agent.max_output_bytes` for this tool. |
| `max_output_lines` | integer | - | - | Override `agent.max_output_lines` for this tool. |
| `search_result_limit` | integer | `100` | 10 | Max files returned per search. |

### `plugins.grep`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_line_bytes` | integer | `500` | 80 | Skip lines longer than this many bytes. |
| `max_output_bytes` | integer | - | - | Override `agent.max_output_bytes` for this tool. |
| `max_output_lines` | integer | - | - | Override `agent.max_output_lines` for this tool. |
| `search_result_limit` | integer | `100` | 10 | Max match groups per search. A call's `limit` param overrides it. |

### `plugins.skill`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `plugin_dev` | boolean | `true` | - | Offer the builtin plugin-dev skill for writing craft plugins. |

### `plugins.webfetch`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_output_bytes` | integer | - | - | Override `agent.max_output_bytes` for this tool. |
| `max_output_lines` | integer | - | - | Override `agent.max_output_lines` for this tool. |
| `max_response_bytes` | integer | `5242880` | 1024 | Stop reading a response after this many bytes. |

### `plugins.websearch`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_output_bytes` | integer | - | - | Override `agent.max_output_bytes` for this tool. |
| `max_output_lines` | integer | - | - | Override `agent.max_output_lines` for this tool. |
| `max_response_bytes` | integer | `5242880` | 1024 | Stop reading a response after this many bytes. |

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
max_output_lines = 3000
```

After:

```lua
craft.setup({
    agent = { max_output_lines = 3000 },
})
```

Same field names, just Lua syntax instead of TOML.

**Move MCP sections to `mcp.toml`.**

- `~/.config/craft/mcp.toml` (global)
- `.craft/mcp.toml` (per-project)

Same format, just a different file. See [MCP](./mcp.md).

**Permissions stay in `permissions.toml`.**
