# Terminal Integration

`craft term` makes Craft aware of your shell history without copy-paste. As you run commands, Craft logs them. When you ask a question, recent history is injected as context.

It works in bash, zsh, and fish.

## Setup

Print the integration script for your shell and evaluate it:

```bash
eval "$(craft term init bash)"
```

For zsh:

```bash
eval "$(craft term init zsh)"
```

For fish, add this to your `config.fish`:

```fish
craft term init fish | source
```

Add the `eval` line to your shell's startup file (`.bashrc`, `.zshrc`, or `config.fish`) to make it permanent.

By default the integration logs commands and defines the `@craft` alias. To also install a `command_not_found` handler that asks Craft when a command is missing, pass `--with-not-found`. This is opt-in because it runs a headless query on every misspelled command:

```bash
eval "$(craft term init bash --with-not-found)"
```

## What the Integration Does

Once active:

- Every command you run is logged to a per-directory history with `craft term log`. Craft commands themselves are skipped to avoid feedback loops.
- An `@craft` alias is defined. Use it to ask Craft a question with your recent history as context: `@craft why did the build fail?`
- With `--with-not-found`, a `command_not_found` handler falls back to Craft when a command is not found. Off by default, since it runs a headless query on every typo.

History is capped at the last 50 commands per directory to avoid context bloat, and the history file rotates when it grows large.

## Subcommands

### `init`

Print a shell integration script for the given shell (`bash`, `zsh`, or `fish`).

```bash
craft term init bash
```

### `log`

Append a shell command to the current directory's history. Called automatically by the integration hook. You usually don't run this by hand.

```bash
craft term log "cargo build"
```

### `run`

Run a headless agent query with recent shell history injected as context.

```bash
craft term run "why did that last command fail?"
```

Or via the alias:

```bash
@craft what does the failing test expect?
```

The history is wrapped in a `<context>` block and prepended to your query.

| Flag | Description |
|------|-------------|
| `-m`, `--model` | Model spec |
| `--output-format` | `text` (default), `json`, or `stream-json` |

### `info`

Show the active session id for this directory and the recent logged commands.

```bash
craft term info
```

## Where History Is Stored

Command history is stored as JSONL in your state directory, scoped by working directory. Each line records the directory, the command, and a timestamp.

```
<state_dir>/shell_history.jsonl
```

See [Configuration: Directory Layout](./configuration.md#directory-layout) for where your state directory lives.
