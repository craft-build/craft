# Review

`craft review` runs code review checks against your current diff. Each check is a Markdown file with a small frontmatter header and a body of instructions. Craft discovers them, runs them in parallel, and collects the findings.

The built-in `review` tool (used mid-session by the agent) and `craft review` share the same finding model and styleguide integration, so findings look the same whether they come from an in-session review or a CLI run.

## A First Check

Save this as `.agents/checks/security.md`:

```markdown
---
name: security
model: anthropic/claude-sonnet-4-6
turn-limit: 8
severity-default: high
---

Review the changed files for security issues:
- secrets or credentials committed by mistake
- unsafe deserialization or code injection
- missing input validation on external data

Use the styleguide tools to find the rules that apply. Link each finding to its rule IDs.
```

Run it against your current diff:

```bash
craft review
```

## Check Frontmatter

Frontmatter goes between two `---` lines at the top of the file.

| Field | Description |
|-------|-------------|
| `name` | Check name (defaults to the file name) |
| `model` | Model spec for this check, overriding `-m` |
| `turn-limit` | Max agent turns, passed as `--max-turns` |
| `tools` | Extra tools to allow (merged with the defaults) |
| `severity-default` | `low`, `medium`, `high`, or `critical` (default `medium`) |

### Default Tools

Every check runs with `read`, `grep`, `glob`, and the three styleguide tools (`styleguide_list`, `styleguide_search`, `styleguide_get`) available. A check's `tools` field adds to this set. Findings are emitted as JSON on stdout (the output contract), not via the in-process `report_finding` tool, since that cannot cross the subprocess boundary.

### Findings

Each check must respond with a single JSON object:

```json
{
  "findings": [
    {
      "file_path": "src/auth.rs",
      "line_start": 42,
      "line_end": 48,
      "severity": "high",
      "title": "Unvalidated redirect target",
      "body": "The redirect URL is taken from user input without checking the host.",
      "suggestion": "Allow-list redirect hosts or reject absolute URLs.",
      "rule_ids": ["rust-security-1"],
      "confidence": 0.9
    }
  ]
}
```

If there are no issues, respond with `{"findings": []}`.

Craft normalizes each finding into the shared `Finding` type, mapping severity to priority: `critical` to P0, `high` to P1, `medium` to P2, `low` to P3.

## Main Pass

After running the discovered checks, `craft review` runs a file-sharded main pass over the diff. It splits the diff by touched file (via `git diff --name-only HEAD`) and fans out one subprocess per file. This prevents large diffs from causing the model to short-circuit.

## Where Craft Looks

Checks are discovered from project-scoped directories, walked from the current directory up to the nearest `.git`, plus the global config directory. Closer scopes shadow farther ones by file name.

**Project** (per ancestor level):

- `.craft/checks/`
- `.agents/checks/`
- `.claude/checks/`
- `.opencode/checks/`

**Global**:

- `~/.config/craft/checks/`
- `~/.craft/checks/`

## Flags

| Flag | Description |
|------|-------------|
| `--dry-run` | List discovered checks without executing |
| `--no-file-pass` | Skip the file-sharded main pass over the diff |
| `--fail-on-findings` | Exit non-zero if any findings are produced (for CI) |
| `--check-filter <regex>` | Only run checks whose name matches |
| `--severity <level>` | Minimum severity to include (`low`, `medium`, `high`, `critical`) |
| `-m`, `--model` | Model spec for checks that don't specify one |

## Examples

Preview which checks will run:

```bash
craft review --dry-run
```

Only run security checks, hide low-severity noise:

```bash
craft review --check-filter '^security' --severity high
```

Run checks only (skip the per-file main pass) and fail CI on any finding:

```bash
craft review --no-file-pass --fail-on-findings
```

Checks run in parallel with at most 4 concurrent subprocesses, so wall-clock time is the slowest check, not the sum of all of them.
