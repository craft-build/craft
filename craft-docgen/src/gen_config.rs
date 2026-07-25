use std::fmt::Write;
use std::sync::Arc;

use craft_agent::tools::ToolRegistry;
use craft_config::{
    AgentConfig, CompressionConfig, ConfigField, DEFAULT_MAX_LOG_FILES, DEFAULT_MAX_OUTPUT_LINES,
    DEFAULT_MOUSE_SCROLL_LINES, FlowConfig, MIN_TOOL_OUTPUT_LINES, ProviderConfig, StorageConfig,
    TOP_LEVEL_FIELDS, ToolOutputLines, UiConfig,
};
use craft_lua::{PluginHost, PluginOptionSpecs};

fn escape_md_table(s: &str) -> String {
    s.replace('|', "\\|")
}

fn write_table_with_min(out: &mut String, fields: &[ConfigField]) {
    writeln!(out, "| Field | Type | Default | Min | Description |").unwrap();
    writeln!(out, "|-------|------|---------|-----|-------------|").unwrap();
    for f in fields {
        let default = f.default.format_default();
        let min = f.min.map_or("-".to_string(), |v| v.to_string());
        writeln!(
            out,
            "| `{name}` | {ty} | `{default}` | {min} | {desc} |",
            name = f.name,
            ty = escape_md_table(f.ty),
            desc = escape_md_table(f.description),
        )
        .unwrap();
    }
}

fn write_table_no_min(out: &mut String, fields: &[ConfigField]) {
    writeln!(out, "| Field | Type | Default | Description |").unwrap();
    writeln!(out, "|-------|------|---------|-------------|").unwrap();
    for f in fields {
        let default = f.default.format_default();
        writeln!(
            out,
            "| `{name}` | {ty} | `{default}` | {desc} |",
            name = f.name,
            ty = escape_md_table(f.ty),
            desc = escape_md_table(f.description),
        )
        .unwrap();
    }
}

fn has_any_min(fields: &[ConfigField]) -> bool {
    fields.iter().any(|f| f.min.is_some())
}

fn lua_section_name(heading: &str) -> String {
    heading
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string()
}

fn write_section(out: &mut String, heading: &str, fields: &[ConfigField]) {
    let lua_name = lua_section_name(heading);
    writeln!(out, "### `{lua_name}`\n").unwrap();
    if has_any_min(fields) {
        write_table_with_min(out, fields);
    } else {
        write_table_no_min(out, fields);
    }
    writeln!(out).unwrap();
}

fn write_plugin_options(out: &mut String, specs: &PluginOptionSpecs) {
    for (plugin, options) in specs {
        writeln!(out, "### `plugins.{plugin}`\n").unwrap();
        writeln!(out, "| Field | Type | Default | Min | Description |").unwrap();
        writeln!(out, "|-------|------|---------|-----|-------------|").unwrap();
        for o in options {
            let default = o
                .default
                .as_ref()
                .map_or("-".to_string(), |d| format!("`{d}`"));
            let min = o.min.map_or("-".to_string(), |m| m.to_string());
            writeln!(
                out,
                "| `{name}` | {ty} | {default} | {min} | {desc} |",
                name = o.name,
                ty = o.ty,
                desc = escape_md_table(&o.desc),
            )
            .unwrap();
        }
        writeln!(out).unwrap();
    }
}

fn collect_plugin_options() -> PluginOptionSpecs {
    let host =
        PluginHost::with_all_builtins(Arc::new(ToolRegistry::new())).expect("loading builtins");
    let specs = host.plugin_options().expect("collecting plugin options");
    assert!(
        !specs.is_empty(),
        "no plugin declared options; the plugins reference would be empty"
    );
    specs
}

fn write_theme_section(out: &mut String) {
    writeln!(out, "### `ui.theme`\n").unwrap();
    writeln!(
        out,
        "Name of the color theme to load at startup, overriding the theme you \
         last picked interactively. If unset, Craft keeps your last selection \
         (the built-in default on first run). An unknown name is ignored with \
         a warning.\n"
    )
    .unwrap();
    let names = craft_ui::BUNDLED_THEMES
        .iter()
        .map(|t| format!("`{}`", t.name))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "Available themes: {names}.\n").unwrap();
    writeln!(
        out,
        "Themes use 24-bit colors, but not every terminal can show them. Craft \
         checks the environment, terminfo, and the terminal itself, and when \
         truecolor is missing it quietly falls back to the closest of the 256 \
         classic terminal colors. If detection gets it wrong, set \
         `CRAFT_TRUECOLOR=1` to force truecolor or `CRAFT_TRUECOLOR=0` to force \
         the fallback.\n"
    )
    .unwrap();
}

fn write_tool_output_section(out: &mut String) {
    writeln!(out, "### `ui.tool_output_lines`\n").unwrap();
    writeln!(
        out,
        "How many lines of output to show per tool in the UI. \
         All values are `usize` with a minimum of {MIN_TOOL_OUTPUT_LINES}.\n"
    )
    .unwrap();
    writeln!(out, "| Field | Default |").unwrap();
    writeln!(out, "|-------|---------|").unwrap();
    for (name, default) in ToolOutputLines::FIELD_DEFAULTS {
        writeln!(out, "| `{name}` | {default} |",).unwrap();
    }
    writeln!(out).unwrap();
}

fn write_keybindings_section(out: &mut String) {
    use craft_ui::keybindings::all_action_ids;

    writeln!(out, "### `ui.keybindings`\n").unwrap();
    writeln!(
        out,
        "Override the default keybindings. Keys are snake_case action ids; \
         values are a chord string, a list of chords, or an empty list to disable. \
         Unknown ids and unparseable chords are warned and dropped.\n"
    )
    .unwrap();
    writeln!(out, "| Action | Chord format |").unwrap();
    writeln!(out, "|--------|--------------|").unwrap();
    writeln!(
        out,
        "| `<action>` | `\"Ctrl+P\"`, `\"Alt+M\"`, `\"Shift+Tab\"`, `\"F5\"`, or a list |"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "Remappable actions:\n").unwrap();
    writeln!(out, "| Action id |").unwrap();
    writeln!(out, "|-----------|").unwrap();
    for id in all_action_ids() {
        writeln!(out, "| `{}` |", id.snake()).unwrap();
    }
    writeln!(out).unwrap();
}

fn write_nested_agent_sections(out: &mut String) {
    writeln!(out, "### `agent.validation`\n").unwrap();
    writeln!(
        out,
        "Run a project-level compile check after the agent writes files. Disabled by default.\n"
    )
    .unwrap();
    writeln!(out, "| Field | Type | Default | Description |").unwrap();
    writeln!(out, "|-------|------|---------|-------------|").unwrap();
    writeln!(
        out,
        "| `enabled` | bool | `false` | Enable post-write compile validation |"
    )
    .unwrap();
    writeln!(out, "| `command` | string | `none` | Custom validation command, overriding the detected project command |").unwrap();
    writeln!(
        out,
        "| `max_iterations` | u8 | `3` | Max validation retry iterations |"
    )
    .unwrap();
    writeln!(
        out,
        "| `timeout_secs` | u64 | `30` | Validation command timeout (seconds) |"
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### `agent.format`\n").unwrap();
    writeln!(out, "Auto-format files in place after the agent writes them, before the compile check. \
                    Runs the formatter mapped to each file's extension, for example `rustfmt` for `.rs` \
                    and `prettier --write` for `.ts` or `.json`. A missing formatter is silently skipped. \
                    Set `command` to run one custom command for every formattable file.\n").unwrap();
    writeln!(out, "| Field | Type | Default | Description |").unwrap();
    writeln!(out, "|-------|------|---------|-------------|").unwrap();
    writeln!(
        out,
        "| `enabled` | bool | `false` | Enable post-write auto-formatting |"
    )
    .unwrap();
    writeln!(
        out,
        "| `command` | string | `none` | Custom formatter command, overriding the extension table |"
    )
    .unwrap();
    writeln!(
        out,
        "| `timeout_secs` | u64 | `15` | Formatter command timeout (seconds) |"
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### `agent.small_model`\n").unwrap();
    writeln!(
        out,
        "Route turns to a cheaper, lower-latency model when the conversation is running on a \
         small-context model (auto-detected from the context window). Off by default; when \
         `enabled` is true it always activates, otherwise it activates automatically for models \
         whose context window is below `auto_detect_context_window`.\n"
    )
    .unwrap();
    writeln!(out, "| Field | Type | Default | Description |").unwrap();
    writeln!(out, "|-------|------|---------|-------------|").unwrap();
    writeln!(
        out,
        "| `enabled` | bool | `false` | Force small-model mode on (skips auto-detection) |"
    )
    .unwrap();
    writeln!(
        out,
        "| `reduced_tools` | bool | `false` | Advertise a smaller tool set to save tokens |"
    )
    .unwrap();
    writeln!(
        out,
        "| `compact_prompt` | bool | `false` | Use a shorter system prompt |"
    )
    .unwrap();
    writeln!(
        out,
        "| `aggressive_truncation` | bool | `false` | Truncate tool output more aggressively |"
    )
    .unwrap();
    writeln!(
        out,
        "| `compaction_threshold` | number | `0.50` | Fraction of context window that triggers compaction |"
    )
    .unwrap();
    writeln!(
        out,
        "| `forgiving_parsing` | bool | `true` | Tolerate minor model output formatting errors |"
    )
    .unwrap();
    writeln!(
        out,
        "| `auto_detect_context_window` | u32 | `32000` | Context window cutoff (in tokens) below which small-model mode auto-activates |"
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### `agent.advisor`\n").unwrap();
    writeln!(
        out,
        "An always-on lightweight reviewer that reads the transcript delta each turn and emits \
         at most one deduplicated note. Distinct from the `judge` goal-completion check and the \
         on-demand `review` subagent. Off by default.\n"
    )
    .unwrap();
    writeln!(out, "| Field | Type | Default | Description |").unwrap();
    writeln!(out, "|-------|------|---------|-------------|").unwrap();
    writeln!(
        out,
        "| `enabled` | bool | `false` | Enable the turn-by-turn advisor reviewer |"
    )
    .unwrap();
    writeln!(
        out,
        "| `model` | string | `none` | `provider/model_id` spec for the advisor. When unset, the `advisor` role from `model_roles.toml` is used, falling back to the active model |"
    )
    .unwrap();
    writeln!(
        out,
        "| `dedup_size` | usize | `16` | Maximum advisor notes kept in the dedup FIFO |"
    )
    .unwrap();
    writeln!(out).unwrap();

    write_section(out, "[agent.flow]", FlowConfig::FIELDS);

    writeln!(out, "### `agent.ttsr`\n").unwrap();
    writeln!(
        out,
        "Time-traveling stream rules. When enabled, rules loaded from `.craft/rules/*.md` (lines \
         prefixed with `rule:`) are matched against the in-flight stream text each turn, and a \
         firing rule injects a system reminder. Off by default.\n"
    )
    .unwrap();
    writeln!(out, "| Field | Type | Default | Description |").unwrap();
    writeln!(out, "|-------|------|---------|-------------|").unwrap();
    writeln!(
        out,
        "| `enabled` | bool | `false` | Enable time-traveling stream rules |"
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### `agent.compaction`\n").unwrap();
    writeln!(
        out,
        "Per-model compaction thresholds. When set, override the global \
         `agent.compaction_buffer` and `agent.compaction_threshold` for matching models. \
         Lookup order: exact `provider/model_id` key, then bare `model_id`, then \
         `global_threshold`. Either `reserve_tokens` (absolute) or `compact_percent` \
         (percentage of the context window to compact at) may be set on a threshold; \
         `reserve_tokens` wins when both are present.\n"
    )
    .unwrap();
    writeln!(out, "| Field | Type | Default | Description |").unwrap();
    writeln!(out, "|-------|------|---------|-------------|").unwrap();
    writeln!(
        out,
        "| `model_thresholds` | table<string, ModelThreshold> | `{{}}` | Map of `provider/model_id` (or bare `model_id`) to per-model threshold |"
    )
    .unwrap();
    writeln!(
        out,
        "| `global_threshold` | ModelThreshold? | `nil` | Fallback threshold applied when no model-specific entry matches |"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "Each `ModelThreshold` may set:\n").unwrap();
    writeln!(out, "| Field | Type | Default | Description |").unwrap();
    writeln!(out, "|-------|------|---------|-------------|").unwrap();
    writeln!(
        out,
        "| `reserve_tokens` | u32? | `nil` | Absolute tokens to reserve; compacts once remaining context drops below this |"
    )
    .unwrap();
    writeln!(
        out,
        "| `compact_percent` | u8? | `nil` | Compact at this percentage of the context window (1-99) |"
    )
    .unwrap();
    writeln!(
        out,
        "| `keep_recent_tokens` | u32? | `nil` | Advisory token budget for the preserved tail (reserved for future use) |"
    )
    .unwrap();
    writeln!(out).unwrap();
}

pub fn generate() -> String {
    let mut out = String::with_capacity(4096);

    writeln!(
        out,
        "\
# Configuration

Settings go in `init.lua`, a Lua script that calls `craft.setup()`. Same language as plugins.

Two places, both optional:

- **Global**: `~/.config/craft/init.lua`
- **Project**: `.craft/init.lua` (relative to your working directory)

When both exist, project settings override global ones. Neither file is required.

## Example

```lua
craft.setup({{
    ui = {{
        splash_animation = true,
        mouse_scroll_lines = {mouse_scroll},
        theme = \"tokyonight\",
        tool_output_lines = {{
            bash = {tol_bash},
            read = {tol_read},
        }},
        keybindings = {{
            search = \"Ctrl+F\",
            plan_toggle = {{\"Ctrl+T\", \"Alt+T\"}},
            tasks = {{}},  -- disable
        }},
    }},
    agent = {{
        max_output_lines = {max_output_lines},
    }},
    provider = {{
        default_model = \"anthropic/claude-sonnet-4-6\",
    }},
    storage = {{
        max_log_files = {max_log_files},
    }},
    plugins = {{
        bash = {{ timeout_secs = 180 }},
    }},
}})
```

All fields are optional. Typos in field names cause an error right away.

`craft.setup()` can only be called once per init.lua.

## Full Reference
 ",
        mouse_scroll = DEFAULT_MOUSE_SCROLL_LINES + 2,
        tol_bash = ToolOutputLines::DEFAULT.bash + 3,
        tol_read = ToolOutputLines::DEFAULT.read + 2,
        max_output_lines = DEFAULT_MAX_OUTPUT_LINES + 1000,
        max_log_files = DEFAULT_MAX_LOG_FILES / 2,
    )
    .unwrap();

    writeln!(out, "### Top-level\n").unwrap();
    write_table_no_min(&mut out, TOP_LEVEL_FIELDS);
    writeln!(out).unwrap();

    write_section(&mut out, "[ui]", UiConfig::FIELDS);
    write_theme_section(&mut out);
    write_tool_output_section(&mut out);
    write_keybindings_section(&mut out);
    write_section(&mut out, "[agent]", AgentConfig::FIELDS);
    write_nested_agent_sections(&mut out);
    write_section(&mut out, "[provider]", ProviderConfig::FIELDS);
    write_section(&mut out, "[storage]", StorageConfig::FIELDS);
    write_section(&mut out, "[compression]", CompressionConfig::FIELDS);

    writeln!(out, "### `sandbox`\n").unwrap();
    writeln!(out, "| Field | Type | Default | Description |").unwrap();
    writeln!(out, "|-------|------|---------|-------------|").unwrap();
    writeln!(
        out,
        "| `enabled` | bool | `true` | Enable sandbox restrictions on tools |"
    )
    .unwrap();
    writeln!(
        out,
        "| `mode` | string | `\"workspace_write\"` | Sandbox mode. One of: `workspace_write`, `read_only`, `danger_full_access`, `off` |"
    )
    .unwrap();
    writeln!(
        out,
        "| `network` | bool | `true` | Allow network access in sandboxed tools |"
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "## Plugins\n").unwrap();
    writeln!(
        out,
        "The `plugins` table turns plugins on or off and passes options to \
         them. All bundled plugins are on by default. Set \
         `enabled = false` to turn one off.\n\n\
         Each plugin checks its own options at startup. A typo, a wrong \
         type, or an unknown plugin name gives you a clear error right \
         away. The old `tools` table is gone. If your config still uses \
         it, Craft stops at startup and shows you the new form.\n"
    )
    .unwrap();
    writeln!(
        out,
        "\
```lua
craft.setup({{
    plugins = {{
        bash = {{ timeout_secs = 180 }},
        websearch = {{ enabled = false }},
    }},
}})
```\n"
    )
    .unwrap();

    write_plugin_options(&mut out, &collect_plugin_options());

    writeln!(out, "## Validation\n").unwrap();
    writeln!(
        out,
        "If a value is below its minimum, Craft shows a `ConfigError` with the field name, \
         value, and minimum."
    )
    .unwrap();

    writeln!(
        out,
        "
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
craft.setup({{
    agent = {{ max_output_lines = 3000 }},
}})
```

Same field names, just Lua syntax instead of TOML.

**Move MCP sections to `mcp.toml`.**

- `~/.config/craft/mcp.toml` (global)
- `.craft/mcp.toml` (per-project)

Same format, just a different file. See [MCP](./mcp.md).

**Permissions stay in `permissions.toml`.**"
    )
    .unwrap();

    out
}
