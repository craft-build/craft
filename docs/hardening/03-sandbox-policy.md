# Restore fail-closed and configurable sandbox policy

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/tools/bash.rs::spawn` skips sandboxing when the backend is missing and
always chooses workspace-write with the default allowed network policy.
The predecessor refuses execution when requested sandboxing is unavailable
(`~/Projects/craft/craft-lua/src/terminal_backend.rs`) and maps configured mode
and network policy (`craft-lua/src/api/fn.rs`).

## Work

Connect execution to an explicit sandbox policy. Distinguish disabled,
required-but-unavailable and active sandbox states. Refuse required sandboxed
execution when its backend is missing. Preserve a deliberate, documented
unsandboxed opt-in; do not infer it from backend availability.
Wire read-only and network-denied profiles through actual command setup.
Define behavior on unsupported platforms and for permission bypass explicitly.

## Acceptance

- Missing required backends fail before a command starts.
- Mode and network choices reach Linux/macOS profile construction.
- Read-only workers do not receive workspace-write by default.
- Tests inject backend availability and inspect policy without changing the host.
- Platform limitations and effective sandbox status are visible to callers.
