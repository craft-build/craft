# Run

`craft run` runs a single headless agent query and prints the result. It accepts either a prompt or a recipe file.

For a plain prompt, `craft run` is the subcommand equivalent of `--print`:

```bash
craft run "explain the auth module"
```

Pipe via stdin:

```bash
echo "list all TODO comments" | craft run
```

For a recipe file, pass its path. Craft detects recipes by their `.yaml`, `.yml`, or `.json` extension and runs them with parameter resolution and templating. See [Recipes](./recipes.md) for the recipe format.

```bash
craft run .craft/recipes/audit.yaml --param focus=security
```

## Flags

| Flag | Description |
|------|-------------|
| `-m`, `--model` | Model spec |
| `--output-format` | `text` (default), `json`, or `stream-json` |
| `--max-turns` | Cap the number of agent turns |
| `--allowed-tools` | Pre-approve a comma-separated tool list |
| `--param KEY=VALUE` | Recipe parameter override (repeatable) |
| `--no-session` | Don't persist a session log for this run |
| `--quiet` | Suppress the "running recipe" banner |
| `--yolo` | Skip all permission prompts |
| `--no-plugins` | Disable the Lua plugin system |

## Output Formats

Same as [Headless Mode](./headless.md). With `--output-format json`, the result is a single JSON object with `result`, `is_error`, `session_id`, `model`, `num_turns`, `stop_reason`, and `usage`:

```bash
craft run "summarize the diff" --output-format json | jq '.result'
```

## Persisted Sessions

By default, `craft run` persists a session log you can resume later, just like an interactive run. Pass `--no-session` for ephemeral runs (useful in CI or when driving `craft review` subprocesses).

```bash
craft run "refactor the parser" --yolo
craft -c   # resume that session in the TUI
```
