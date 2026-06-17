Search and replace code using AST patterns. More precise than regex for code.

Patterns use metavariables:
- `$NAME` matches a single AST node (identifier, expression, statement, etc.)
- `$$$BODY` matches zero or more AST nodes (function body, argument list, etc.)

Search mode (no `rewrite`): finds all matches, showing file:line with match preview.
Replace mode (with `rewrite`): shows unified diffs by default. Set `apply=true` to write.

Replace validates syntax before writing — invalid replacements are rolled back.

Supported languages: rust, typescript, tsx, python, go, java, c, cpp, ruby, lua, bash, kotlin, swift, c_sharp, elixir, scala, php, html, dart, starlark, nix, zig.

Examples:
- `pattern="fn $NAME($$$ARGS)"` — find all function declarations
- `pattern="console.log($MSG)" rewrite="tracing::info!($MSG)"` — dry-run replace
- `pattern="$OBJ.$METHOD($$$ARGS)" lang="python"` — find method calls
