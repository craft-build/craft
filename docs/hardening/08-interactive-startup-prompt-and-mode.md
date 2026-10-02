# Honor interactive startup prompt and mode

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/main.rs` starts the TUI without forwarding the initial prompt or mode.
`agent/src/tui/app/mod.rs` initializes in Build mode. The predecessor handles
startup prompt input and initial session mode in `src/cmd/tui.rs`.

## Work

Pass an explicit startup configuration into the interactive surface. Honor a
positional prompt and decide/document piped-input behavior. Initialize plan mode
before submitting the prompt so the first turn receives plan restrictions.
Define precedence between CLI mode and resumed-session mode. Do not enable Flow
as a side effect; it remains unsupported unless separately implemented.

## Acceptance

- An initial prompt is submitted exactly once after provider startup.
- `--mode plan` affects the first executed turn, not just a label.
- Resume, empty input and startup failure have predictable behavior.
- Mock-provider/TUI tests cover sequencing and prevent accidental Build writes.
