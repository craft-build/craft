# CLI

Use `craft` to open the terminal UI, or add `--print` to run non-interactively. This page covers startup options and subcommands; see [TUI](./tui.md) for interactive commands.

```sh
craft [OPTIONS] [PROMPT]
```

## Session flags

### Resume a session

`-c`, `--continue` resumes the latest session. `-s`, `--session <ID>` resumes a specific one.

```sh
craft --continue
craft --session <ID>
```

### Fork or name a session

Add `--fork-session` to `--continue` or `--session` to work from a copy without changing the source session.

`--session-id <ID>` sets the saved ID for a **new** session. It does not resume an existing one.

## Mode and model

### Choose a model

`-m`, `--model <provider/model>` selects a provider alias and exact model ID. Models do not have to appear in the catalog to be usable. Run `craft models` to inspect the resolved catalog.

### Start in plan mode

`--mode <build|plan>` sets a fresh session's mode. Plan mode is read-only; build mode allows edits subject to permissions.

`--permission-mode <default|plan|bypassPermissions>` selects the permission behavior. `plan` also selects plan mode; `bypassPermissions` is equivalent to `--yolo`. The compatibility value `acceptEdits` is not supported.

In resumed sessions the stored mode wins over `--mode`/`--permission-mode plan` (see [Sessions](./sessions.md#mode-persistence)).

### Enable Python tool orchestration

`--codemode <off|alongside|exclusive>` sets the code-execution mode for this run. The default is `off`; see [Configuration](./configuration.md#codemode) for what each mode exposes.

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
- `--system-prompt <TEXT>` — replace the agent preamble.
- `--append-system-prompt <TEXT>` — append to the agent preamble instead of replacing it.
- `--exit-on-done` — exit after the agent completes (automation workflows).
- `--verbose` — verbose transcript output (**accepted but not implemented yet**, warned).
- `--include-partial-messages` — partial streaming messages in SDK output (**accepted but not implemented yet**, warned).
- `--input-format stream-json` — **not yet implemented** (SDK mode), warned at runtime.

Some Claude Code SDK compatibility flags are accepted but ignored. Do not assume that accepting a flag means its behavior is implemented; see `craft --help` and check startup warnings.

## Startup prompt

The positional `PROMPT` is honored by `--print` **and** the interactive TUI — `craft "do x"` submits it as the first message right after provider startup. Piped stdin (`echo "do x" | craft`, no `--print`) is read the same way; the positional wins. In resume sessions the prompt waits for the session to load.

## Thinking controls

`/thinking` opens the TUI selector; `/thinking low`, `/thinking adaptive`, `/thinking off`, or `/thinking 4096` apply directly. Alt+E cycles. The TUI preference persists with the session, restores on resume, and is inherited by subagents. ACP exposes a Thinking session option. [Headless resume](./sessions.md#mode-persistence) does not restore the saved thinking preference.

For headless or startup overrides use one of `--thinking <setting>`, `--effort <level>`, or `--max-thinking-tokens <positive-count>` — mutually exclusive. Seed new sessions in `craft.bml`:

```bml
always_thinking = "low"
```

Modes are `off`, `adaptive`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, or a positive token count. An unset preference means **off**. Providers that cannot disable reasoning use their lowest supported mode; "off" is not a universal promise of zero reasoning.

Per-model `models.dev` reasoning metadata resolves accepted effort levels. Requests snap to the closest lower supported level, or the minimum accepted level if none is lower. Effort-only and toggle-only APIs cannot enforce a thinking-token budget; numeric settings map to an effort/toggle there. Budget APIs clamp to their published limits and half the effective output window, reserving room for an answer.

Manual model declarations can override `supports_thinking`, `requires_thinking`, and `reasoning_options`; local backends can declare `thinking_fields` fragments (see [Providers](./providers.md#model-catalogs)).

## Subcommands

### Inspect configuration and usage

- `craft models` lists the resolved model catalog.
- `craft doctor [--export]` diagnoses and repairs provider configuration.
- `craft stats` reports the usage ledger. Add `--sessions` for a per-session breakdown, or `--codemode` to summarize `codemode.jsonl` runs instead of the cost ledger.
- `craft prompt [system|research|general] [--plan|--tools|--names]` prints built-in prompts.

### Maintain your installation

- `craft update [-y]` installs a digest-pinned update.
- `craft rollback` undoes one update.
- `craft completions <shell>` generates shell completions.

### Connect editors and workflows

- `craft acp` starts the [ACP](./acp.md) server on stdio.
- `craft term init|log|run|info` provides [terminal integration](#terminal-integration).
- `craft recipe list|run` lists or runs [recipes](./skills-recipes.md#recipes).

Use `craft <subcommand> --help` for that command's arguments.

## Terminal integration

`craft term init` sets up shell integration: shell history logging, `@craft` queries, and a command-not-found handler. `craft term log`, `craft term run`, and `craft term info` work with it.
