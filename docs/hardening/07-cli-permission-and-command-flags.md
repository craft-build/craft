# Make permission and command flags operational

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/cli.rs` parses `--allowed-tools`, `--disallowed-tools`,
`--permission-mode` and `--no-commands`, but operational plumbing is missing.
Custom markdown command discovery itself exists in `agent/src/command.rs`.

## Work

Specify and implement each flag's behavior across its supported surfaces.
Distinguish tool availability from preapproval; normalize accepted tool aliases
and define deny precedence. Reject unsupported modes/options rather than
silently ignoring safety restrictions. Honor `--no-commands` at discovery.
Audit yolo/auto-review propagation in headless and interactive paths.
Do not claim all predecessor flags worked; inspect reference semantics.

## Acceptance

- Execution tests prove allow/deny effects, including batch and MCP tools.
- Invalid tool names and conflicting policies have useful errors.
- Command discovery is genuinely suppressed when requested.
- Help matches behavior, and no safety flag is a silent no-op.
