# Apply system-prompt overrides exactly once

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/main.rs` applies `Cli::effective_preamble`, and `run_print` in
`agent/src/cli.rs` applies it again. Without an explicit system-prompt override,
append text can be duplicated.

## Work

Choose one boundary where CLI prompt overrides are resolved. Pass the effective
value onward without applying the transformation again. Preserve intended
built-in prompt/instruction behavior and clarify whether "system prompt override"
replaces the full system prompt or only its configurable slot.

## Acceptance

- Configured preamble plus append text occurs exactly once.
- Explicit replacement plus append text occurs exactly once.
- Empty/whitespace values behave predictably.
- Tests inspect the final model request for TUI and print, not only the helper.
