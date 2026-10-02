# Define and enforce an honest undo contract

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/snapshot.rs` primarily records file contents. Directory moves,
empty-directory deletes, metadata and removal of newly created paths are not
fully reversible. The stacked undo feature is useful but is not a complete
filesystem checkpoint.

## Work

Compare two approaches: explicitly limited content undo, or operation-aware
rollback with create/remove/move records and metadata. Recommend the smallest
safe contract; ask for a product decision if needed before a broad redesign.
Never delete newly created user work during rollback without conflict checks.
Report unsupported and partial restoration instead of implying full success.

## Acceptance

- Tests cover existing-file edits, new files, directory moves, empty directories
  and external modifications after the agent's mutation.
- UI/tool documentation states supported limits accurately.
- Partial failures preserve evidence and report actionable details.
- Undo stack ordering and size limits remain correct.
