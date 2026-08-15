use std::fmt::Write;

use craft_ui::BUILTIN_COMMANDS;

use crate::lua_util;

const ALIASING: &str = r#"## Aliasing commands

Prefer a different name for a command? `craft.api.run_command` runs any slash command exactly as typing it would, so an alias is a one-line handler in your `init.lua` instead of a reimplementation.

```lua
-- ~/.config/craft/init.lua
local aliases = {
    { name = "/clear", target = "/new", description = "Alias for /new" },
    { name = "/resume", target = "/sessions", description = "Alias for /sessions" },
}

for _, alias in ipairs(aliases) do
    craft.api.register_command({
        name = alias.name,
        description = alias.description,
        handler = function()
            local ok, err = craft.api.run_command(alias.target)
            if not ok then
                craft.ui.flash("could not run " .. alias.target .. ": " .. err)
            end
        end,
    })
end
```

Both names stay in the palette: aliasing adds a name, it does not rename or hide the original. It works for any command listed above, plus plugin commands and MCP prompts. See the plugins page for matching and error handling, or `craft.ui.action` to bind a key instead of a name."#;

fn write_row(out: &mut String, name: &str, description: &str) {
    writeln!(out, "| `{name}` | {} |", description.replace('|', "\\|")).unwrap();
}

pub fn generate() -> String {
    let mut out = String::new();
    writeln!(out, "# Commands").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Type `/` in the input box to open the command palette."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "## Built-in commands").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| Command | Description |").unwrap();
    writeln!(out, "|---------|-------------|").unwrap();
    for cmd in BUILTIN_COMMANDS {
        write_row(&mut out, cmd.name, cmd.description);
    }
    for cmd in &lua_util::load_builtin_plugin_commands() {
        write_row(&mut out, &cmd.name, &cmd.description);
    }

    writeln!(out).unwrap();
    writeln!(out, "## Sessions").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Sessions run concurrently. `/new` starts a fresh session while the old one keeps working in the background, and `/sessions` shows the live status of each (working, needs input, idle) so you can jump between them. When a background session finishes or needs input, Craft flashes a note in the status bar."
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(out, "## Custom commands").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "You can define your own slash commands as Markdown files."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### Project commands").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Place `.md` files in `.craft/commands/` in your project root."
    )
    .unwrap();
    writeln!(out, "They appear in the palette as `/project:<filename>`.").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### User commands").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "Place `.md` files in `~/.config/craft/commands/`.").unwrap();
    writeln!(out, "They appear in the palette as `/user:<filename>`.").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Project commands override user commands with the same name."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "`.claude/commands/` directories are also supported for compatibility."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### Metadata").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "You can add optional metadata at the top of the file between `---` lines to set `name`, `description`, and `argument-hint`:"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "```markdown").unwrap();
    writeln!(out, "---").unwrap();
    writeln!(out, "description: Review code for issues").unwrap();
    writeln!(out, "argument-hint: <file>").unwrap();
    writeln!(out, "---").unwrap();
    writeln!(out, "Review $ARGUMENTS and suggest improvements.").unwrap();
    writeln!(out, "```").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### Arguments").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Use `$ARGUMENTS` in the command body. It gets replaced with whatever you type after the command name."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "For example, `/project:review main.rs` replaces `$ARGUMENTS` with `main.rs`."
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(out, "{ALIASING}").unwrap();

    if out.ends_with('\n') {
        out.pop();
    }
    out
}
