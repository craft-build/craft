# Recover model selection from stale session indexes

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/storage/sessions/mod.rs::latest_model_in` returns the indexed file's
optional model immediately. A present file with a malformed header or missing
model can suppress the existing scan fallback. Compare `latest_in` behavior.

## Work

Treat unusable indexed metadata as stale and apply a coherent fallback policy.
Do not accidentally resume an older conversation merely to obtain its model;
define model lookup versus session lookup semantics explicitly. Preserve
deterministic ordering, efficient header scanning and corruption reporting.

## Acceptance

- Tests cover missing files, malformed headers and legacy headers without models.
- Valid indexed records keep the fast path.
- Fallback selection is deterministic and agrees with documented resume policy.
- Index repair does not overwrite or delete session history.
