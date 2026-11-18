# Headless Mode

Run non-interactively with `--print` (also spelled `-p`):

```sh
craft --print "summarize the retry logic in agent/src"
```

## Output formats

`--output-format` selects:

- `text` — plain text reply.
- `stream-json` — streaming JSON in a Claude Code-compatible format. This does not imply full SDK compatibility; some flags are accepted without being implemented.

For example, save streaming events for another program to consume:

```sh
craft --print --output-format stream-json "summarize the retry logic" > events.jsonl
```

`--image PATH` attaches an image to the prompt.

## Prompting

Pass a prompt as a positional argument or pipe it through stdin:

```sh
printf '%s\n' "explain the retry logic" | craft --print
```

A positional prompt takes precedence over piped stdin. Without `--print`, either form opens the TUI and submits the prompt as its first message.

To continue saved work, use `--continue` or `--session <ID>`. Headless resume restores history and the saved model, but not the TUI's saved plan/build mode or thinking preference. Set the options needed by your script explicitly; see [Sessions](./sessions.md#mode-persistence).

## Permissions

Headless runs enforce the same [rules](./permissions.md) as the TUI, but cannot ask you to approve a call. An unresolved `ask` therefore blocks the call (“fails closed”); denied calls never execute. This also applies to batch children and subagent dispatch.

For automation, configure rules or use `--allowed-tools` to pre-approve the tools the task needs. For other calls that require approval, `-A` / `--auto-review` lets an LLM reviewer decide; low confidence still blocks the call.

`--yolo` bypasses permission prompts **and disables the sandbox**. Use it only when that loss of protection is intentional. It cannot be combined with `--auto-review`.

## Other flags

`--max-turns` bounds the run; `-m/--model provider/model` selects the model; `--allowed-tools`/`--disallowed-tools` set process-lifetime tool policy (see [Permissions](./permissions.md#tool-policy-flags)). A set of Claude Code SDK compatibility flags is accepted but ignored.

Not yet implemented (flagged with warnings at runtime): `--input-format stream-json` (SDK mode), verbose transcript output, flow mode.
