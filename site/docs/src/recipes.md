# Recipes

Recipes are declarative, parameterized session blueprints. Instead of typing the same long prompt every time, you write a small YAML or JSON file once and run it with `craft run`.

A skill is passive: it adds instructions to context. A recipe is active: it defines a full session that can run headlessly, take parameters, and compose with other recipes.

## A First Recipe

Save this as `.craft/recipes/audit.yaml`:

```yaml
name: audit
description: Audit dependencies for known vulnerabilities and outdated versions
parameters:
  - name: focus
    type: string
    default: security
    description: What to focus on (security, licensing, or staleness)
  - name: depth
    type: number
    default: 1
instructions: |
  Audit the dependencies in this project.
  Focus: {{ focus }}.
  Depth: {{ depth }} (1 = direct deps only, 2 = transitive).
  Produce a table of findings sorted by severity.
```

Run it:

```bash
craft run .craft/recipes/audit.yaml
```

Override a parameter:

```bash
craft run .craft/recipes/audit.yaml --param focus=staleness --param depth=2
```

## Parameters

Each parameter has a `name`, a `type`, and optional `default`, `required`, `description`, and `options`.

| Type | Accepts | Coercion |
|------|---------|----------|
| `string` | Any text | Used as-is |
| `number` | Digits | Parsed to a float |
| `boolean` | `true`/`false`, `yes`/`no`, `1`/`0` | Parsed to bool |
| `date` | Date text | Used as-is |
| `file` | A path | Used as-is |
| `select` | One of `options` | Validated against `options` |

A required parameter with no default and no `--param` override triggers an interactive prompt. If stdin is not a terminal, Craft errors out and tells you which parameter to pass.

```yaml
parameters:
  - name: target
    type: file
    required: true
    description: File or directory to review
  - name: level
    type: select
    default: shallow
    options: ["shallow", "deep"]
```

## Templating

Instructions are rendered with [minijinja](https://docs.rs/minijinja), a Jinja2-compatible template engine. Use `{{ name }}` to inject a parameter, and the usual control flow:

```yaml
instructions: |
  {% if verbose %}Run in verbose mode.{% else %}Be concise.{% endif %}
  Focus on {{ focus }}.
```

### Sub-recipes

A recipe can include another recipe by relative path. The included recipe's `instructions` are rendered and inlined:

```yaml
# main.yaml
instructions: |
  First, the setup:
  {% include "setup.yaml" %}

  Then continue with the main task.
```

```yaml
# setup.yaml
instructions: "Clone the repo and install dependencies for {{ focus }}."
parameters:
  - name: focus
    type: string
    default: security
```

Non-recipe files (`.txt`, `.md`) are included verbatim.

## Settings Overrides

A recipe can pin the model and cap turns for the session it starts:

```yaml
model: anthropic/claude-sonnet-4-6
max_turns: 10
```

The CLI `--model` and `--max-turns` flags take precedence over the recipe when both are set.

## Where Craft Looks

Recipes are discovered from project-scoped directories, walked from the current directory up to the nearest `.git`, plus the global config directory. Closer scopes shadow farther ones by file name.

**Project** (per ancestor level):

- `.craft/recipes/`
- `.agents/recipes/`
- `.claude/recipes/`
- `.opencode/recipes/`

**Global**:

- `~/.config/craft/recipes/`
- `~/.craft/recipes/`

## Running Recipes

| Flag | Description |
|------|-------------|
| `--param KEY=VALUE` | Override a parameter (repeatable) |
| `-m`, `--model` | Model spec, overriding the recipe's `model` |
| `--max-turns` | Cap turns, overriding the recipe's `max_turns` |
| `--allowed-tools` | Pre-approve a comma-separated tool list |
| `--no-session` | Don't persist a session log for this run |
| `--quiet` | Suppress the "running recipe" banner |
| `--output-format` | `text` (default), `json`, or `stream-json` |
| `--yolo` | Skip all permission prompts |
| `--no-plugins` | Disable the Lua plugin system |

Recipe execution produces a session log identical to an interactive session, unless you pass `--no-session`.

See [Headless Mode](./headless.md) for the output formats. Recipes use the same machinery, so `--output-format json` works the same way.
