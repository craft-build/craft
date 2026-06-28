Propose a structural AST rewrite, then commit or discard it with `resolve`. Safer than a global `edit` when many call sites need the same change.

`ast_edit` runs a dry ast-grep rewrite across files, stages the proposed new content without writing, and returns a `(proposed)` card with an `edit_id`, the replacement count, and a unified diff preview. Call `resolve` with the `edit_id` and `apply` to commit (each file written atomically) or `discard` to drop it without touching the tree.

Patterns use metavariables:
- `$NAME` matches a single AST node (identifier, expression, statement, etc.)
- `$$$BODY` matches zero or more AST nodes (function body, argument list, etc.)

The rewrite is rejected if it introduces syntax errors, so a staged edit is always syntactically valid. Use `ast_grep` for search-only; use `ast_edit` when you intend to apply a rewrite across files.

Supported languages: rust, typescript, tsx, python, go, java, c, cpp, ruby, lua, bash, kotlin, swift, c_sharp, elixir, scala, php, html, dart, starlark, nix, zig.
