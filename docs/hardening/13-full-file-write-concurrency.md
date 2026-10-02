# Protect full-file writes from lost updates

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/tools/write.rs` captures original content but verifies path/existence
instead of comparing current bytes before replacement. `edit.rs` performs a
content-change check. Atomic rename alone does not protect another editor's work.

## Work

Apply a coherent optimistic concurrency rule to full-file replacement and other
mutation tools that share this issue. Reuse staging/path checks. Define conflict
errors and ensure failed writes do not corrupt snapshots or files. Do not claim
an atomic filesystem compare-and-swap guarantee if only a best-effort check is
implemented; document remaining race windows.

## Acceptance

- Injected intervening changes are detected and preserved.
- New-file creation races and disappearance are handled.
- Permissions and unchanged bytes retain existing guarantees.
- Tests deterministically inject changes between read and persist.
