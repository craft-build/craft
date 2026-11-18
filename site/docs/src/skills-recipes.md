# Skills & Recipes

Use a **skill** to give the agent reusable instructions for a task. Use a **recipe** to run a parameterized workflow from the CLI or TUI.

## Skills

Skills are reusable workflows: markdown with instructions and frontmatter, discovered from two sources:

- **bundled** — shipped with the binary (e.g. `debugging`, `run`, `verify`, `stuck`, `agents-md-init`);
- **filesystem** — project-stored skills (`.craft/skills/`) join discovery via argosy.

The agent uses `list_skills` to discover skills and `skill` to load their instructions. Discovery includes each skill's source and trust tier. If a local skill has the same name as a bundled skill, the local one takes precedence.

Trust depends on the skill's source. Imported Argosy skills are wrapped as untrusted content, and their trust cannot exceed the user's configured trust level.

## Recipes

Recipes are YAML or JSON blueprints rendered with MiniJinja. List available recipes, then run one by name:

```sh
craft recipe list
craft recipe run <name> [params…]
```

Recipe runs are non-interactive. They enforce the same [permissions](./permissions.md) as `--print`: unresolved approval requests block calls unless you configure rules or enable auto-review.

## Custom slash commands

Custom namespaced slash commands are discovered from project config; `--no-commands` skips discovery in the TUI. `/recipe` picks a recipe interactively.
