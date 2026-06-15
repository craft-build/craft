# Headless Mode

Run Craft non-interactively with `--print` / `-p`. Useful for scripts, CI, and automation.

```bash
craft "explain this codebase" --print
```

Pipe via stdin:

```bash
echo "list all TODO comments" | craft -p
```

## Output Formats

| Format | Description |
|--------|-------------|
| `text` | Raw response only (default) |
| `json` | Single JSON object with metadata |
| `stream-json` | JSONL stream, one event per line |

```bash
craft "fix the tests" --print --output-format json
```

JSON output includes `type`, `subtype`, `is_error`, `duration_ms`, `num_turns`, `result`, `stop_reason`, `session_id`, `total_cost_usd`, and `usage`.

Add `--verbose` to include full turn-by-turn messages in the output.

## Claude Code Compatibility

Craft's `--print` is a drop-in replacement for Claude Code:

```bash
# Before
claude "fix the bug" --print --output-format json

# After
craft "fix the bug" --print --output-format json
```

Same JSON fields, same `--output-format` options, same `--verbose` behavior. Scripts that parse Claude Code output work unchanged.

## Flags

The most useful flags for automation:

| Flag | Description |
|------|-------------|
| `-p`, `--print` | Non-interactive: run the prompt and exit |
| `-m`, `--model` | Model spec, e.g. `anthropic/claude-sonnet-4-6` |
| `--output-format` | `text`, `json`, or `stream-json` |
| `--verbose` | Include full turn-by-turn messages |
| `--yolo` | Skip all permission prompts (deny rules still apply) |
| `--allowed-tools` | Pre-approve a comma-separated tool list |
| `--disallowed-tools` | Forbid a comma-separated tool list |
| `-c`, `--continue` | Resume the most recent session in this directory |
| `-s`, `--session` | Resume a specific session by ID |
| `--max-turns` | Cap the number of agent turns |
| `--system-prompt` | Replace the system prompt |
| `--append-system-prompt` | Append to the system prompt |
| `--exit-on-done` | Exit after the agent finishes |

See [CLI](./cli.md) for the complete list.

## Examples

Pipe compiler errors back for a fix:

```bash
cargo build 2>&1 | craft "Fix these compiler errors." --print --yolo
```

Generate a changelog from recent commits:

```bash
git log --oneline v1.2.0..HEAD | craft "Write a user-facing \
  changelog grouped by: Added, Changed, Fixed. Skip chores." --print
```

Automated PR summaries in CI:

```bash
SUMMARY=$(git diff main..HEAD | craft "Write a 2-3 sentence \
  summary of this change for a PR description." --print)
gh pr edit --body "$SUMMARY"
```

Migrate an API across many files:

```bash
grep -rl 'old_api_call' src/ | while read file; do
  craft "In $file, migrate old_api_call() to new_api_call(). \
    Keep behavior identical." -p --yolo --allowed-tools Read,Edit
done
```

Cost tracking:

```bash
craft "refactor the database layer" -p --output-format json | jq '.total_cost_usd'
```
