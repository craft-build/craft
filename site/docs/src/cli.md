# CLI

`craft` is the binary (crate `craft`). This page is the flag and subcommand reference.

```sh
craft [OPTIONS] [PROMPT]
```

## Session flags

| Flag | Effect |
| --- | --- |
| `-c`, `--continue` | resume the latest session |
| `-s`, `--session <ID>` | resume a specific session |
| `--fork-session` | copy a `--session`/`--continue` source under a new id, source untouched |
| `--session-id <ID>` | persistence id for a **new** session (never resumes) |

## Mode and model

| Flag | Effect |
| --- | --- |
| `-m`, `--model <provider/model>` | model choice; ids the catalog doesn't list stay usable |
| `--mode <build\|plan>` | seed a fresh session's mode |
| `--permission-mode <default\|plan\|bypassPermissions>` | permission posture (`acceptEdits` rejected as unsupported) |

In resumed sessions the stored mode wins over `--mode`/`--permission-mode plan` (see [Sessions](./sessions.md#mode-persistence)).

## Tool and permission flags

- `--allowed-tools` — pre-approves tools (allow rules; the tool set is not restricted).
- `--disallowed-tools` — denies tools at the permission gate.
- `--yolo` / `--dangerously-skip-permissions` — bypass all prompting and the sandbox.
- `-A`, `--auto-review` — LLM reviewer answers permission prompts, fail-closed.
- `--no-commands` — skip custom slash-command discovery in the TUI.

Semantics and accepted name forms: [Permissions & Snapshots](./permissions.md).

## Headless flags

- `--print` — headless run; see [Headless Mode](./headless.md).
- `--output-format <text|stream-json>` — output shape (stream-json is Claude Code-compatible).
- `--image <PATH>` — attach an image.
- `--max-turns <N>` — bound the run.
- `--system-prompt` — system prompt overrides.
- `--input-format stream-json` — **not yet implemented** (SDK mode), warned at runtime.

A set of Claude Code SDK compatibility flags is accepted but ignored.

## Startup prompt

The positional `PROMPT` is honored by `--print` **and** the interactive TUI — `craft "do x"` submits it as the first message right after provider startup. Piped stdin (`echo "do x" | craft`, no `--print`) is read the same way; the positional wins. In resume sessions the prompt waits for the session to load.

## Thinking controls

`/thinking` opens the TUI selector; `/thinking low`, `/thinking adaptive`, `/thinking off`, or `/thinking 4096` apply directly. Alt+E cycles. The preference persists with the session, restores on resume, and is inherited by subagents. ACP exposes a Thinking session option.

For headless or startup overrides use one of `--thinking <setting>`, `--effort <level>`, or `--max-thinking-tokens <positive-count>` — mutually exclusive. Seed new sessions in `craft.bml`:

```bml
always_thinking = "low"
```

Modes are `off`, `adaptive`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, or a positive token count. An unset preference means **off**. Providers that cannot disable reasoning use their lowest supported mode; "off" is not a universal promise of zero reasoning.

Per-model `models.dev` reasoning metadata resolves accepted effort levels. Requests snap to the closest lower supported level, or the minimum accepted level if none is lower. Effort-only and toggle-only APIs cannot enforce a thinking-token budget; numeric settings map to an effort/toggle there. Budget APIs clamp to their published limits and half the effective output window, reserving room for an answer.

Manual model declarations can override `supports_thinking`, `requires_thinking`, and `reasoning_options`; local backends can declare `thinking_fields` fragments (see [Providers](./providers.md#model-catalogs)).

## Subcommands

| Command | Purpose |
| --- | --- |
| `craft acp` | Agent Client Protocol server on stdio ([ACP](./acp.md)) |
| `craft completions <shell>` | shell completions |
| `craft models` | resolved model catalog |
| `craft stats [--sessions]` | usage ledger |
| `craft doctor [--export]` | diagnose and self-heal provider config |
| `craft update [-y]` / `craft rollback` | digest-pinned self-update / undo one update |
| `craft prompt [system\|research\|general] [--plan\|--tools\|--names]` | print built-in prompts |
| `craft term init\|log\|run\|info` | shell integration ([Terminal integration](#terminal-integration)) |
| `craft recipe list\|run` | recipes ([Skills & Recipes](./skills-recipes.md)) |

## Terminal integration

`craft term init` sets up shell integration: shell history logging, `@craft` queries, and a command-not-found handler. `craft term log`, `craft term run`, and `craft term info` work with it.
