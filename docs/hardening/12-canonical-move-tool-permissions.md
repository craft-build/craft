# Align move tool identity with permission rules

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

The tool table registers `move_file`, but `agent/src/permissions/mod.rs` uses
`move` in write-tool classification, builtin grants and scope matching.

## Work

Choose a canonical wire identity and use it throughout tool registration,
permission scope resolution, UI classification and tests. If retaining an alias,
normalize it at one boundary rather than registering ambiguous behavior.
Consider both source and destination scopes and overwrite behavior.

## Acceptance

- Real `move_file` calls receive the intended mutation classification.
- Source/destination allow, deny and ask rules are enforced.
- Tests call the registered tool through dispatch, including directory moves.
- A consistency test detects future registered-name/permission-name drift.
