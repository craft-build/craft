# Skills & Recipes

## Skills

Skills are reusable workflows: markdown with instructions and frontmatter, discovered from two sources:

- **bundled** — shipped with the binary (e.g. `debugging`, `run`, `verify`, `stuck`, `agents-md-init`);
- **filesystem** — project-stored skills (`.craft/skills/`) join discovery via argosy.

Discovery is surfaced through `list_skills` (origin, kind, shadowing status, trust tier); loading goes through the builtin `skill` tool. Local skills shadow bundled ones by name.

Skill bodies are trusted instructions only when their provenance is: imported argosy skills arrive wrapped as untrusted content, capped at the user's configured trust.

## Recipes

Recipes are parameterized YAML/JSON blueprints rendered with minijinja and run via the CLI:

```sh
craft recipe list
craft recipe run <name> [params…]
```

Recipe runs are headless surfaces: they enforce the same [permission gate](./permissions.md) as `--print`, with the same opt-outs.

## Custom slash commands

Custom namespaced slash commands are discovered from project config; `--no-commands` skips discovery in the TUI. `/recipe` picks a recipe interactively.
