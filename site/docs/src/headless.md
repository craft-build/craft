# Headless Mode

Run non-interactively with `--print` (also spelled `-p`):

```sh
crafty --print "summarize the retry logic in agent/src"
```

## Output formats

`--output-format` selects:

- `text` — plain text reply.
- `stream-json` — Claude Code-compatible streaming JSON, so scripts written for Claude Code SDK-style agents work unmodified.

`--image PATH` attaches an image to the prompt.

## Prompting

The positional `PROMPT` is honored by `--print` **and** the interactive TUI; piped stdin (`echo "do x" | crafty`, no `--print`) is read the same way. The positional wins over piped stdin. Empty/whitespace input starts the TUI normally.

In resume sessions (`-c` / `-s`) the prompt waits for the session to load, so its first turn runs in the loaded mode.

## Permissions

Headless runs enforce the same [rules](./permissions.md) through a permission gate with no interactive approver: unresolved asks fail closed with guidance naming the opt-outs, and denied calls never execute. `--yolo` (bypass everything) and `-A/--auto-review` (an LLM reviewer answers asks, fail-closed) are the deliberate opt-outs — mutually exclusive, rejected at startup. The gate covers batch children and subagent dispatch.

## Other flags

`--max-turns` bounds the run; `-m/--model provider/model` selects the model; `--allowed-tools`/`--disallowed-tools` set process-lifetime tool policy (see [Permissions](./permissions.md#tool-policy-flags)). A set of Claude Code SDK compatibility flags is accepted but ignored.

Not yet implemented (flagged with warnings at runtime): `--input-format stream-json` (SDK mode), verbose transcript output, flow mode.
