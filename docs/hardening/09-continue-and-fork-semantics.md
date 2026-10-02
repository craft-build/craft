# Implement safe continue and fork semantics

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

Print-mode session resolution does not implement `--continue` latest-by-cwd
lookup. `--fork-session` is accepted with a warning rather than performing a
fork. Inspect `agent/src/cli.rs`, `agent/src/headless.rs` and session storage.

## Work

Define resume, supplied session ID and fork behavior consistently. Restore the
intended model/history policy on resume, with explicit CLI overrides taking
documented precedence. Fork into a new identity without altering the source
session. Reject unsupported combinations before execution. If implementation
cannot land safely, reject the fork option instead of pretending to honor it.

## Acceptance

- Continue resolves the latest valid session for the current workspace.
- Fork preserves source history, identity, metadata and persisted contents.
- New-session IDs and resume IDs have distinct tested semantics.
- Tests cover absent sessions, malformed IDs, corrupt records and overrides.
