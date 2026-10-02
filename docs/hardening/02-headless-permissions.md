# Enforce permissions in headless execution

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/cli.rs::run_headless_query` passes `before: None` to headless setup.
`agent/src/run/dispatch.rs` consults permission interception only when a hook is
installed. Configured permission denies and normal approval requirements are
therefore bypassed by CLI headless runs. Tool-local restrictions are not a
replacement for that policy.

## Work

Install a noninteractive permission gate for print, recipe and shell-integration
queries. Enforce denies and explicit grants. Fail closed on unresolved asks;
support an explicit bypass or reviewer policy only when actually selected.
Reuse the permission manager and shared setup rather than duplicating rules.
Inspect `agent/src/headless.rs`, `agent/src/permissions/` and the TUI approval
gate. Read-only reference: `~/Projects/craft/src/cmd/headless.rs`.

## Acceptance

- Without bypass, denied bash, mutation and MCP calls never execute.
- Ask decisions do not hang an unattended run or silently become allows.
- Explicit bypass and auto-review semantics are deliberate and tested.
- Batch and child dispatch cannot escape the same policy.
- Tests exercise execution, exit/result reporting and all headless entry points.
