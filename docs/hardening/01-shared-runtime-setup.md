# Unify runtime setup across execution surfaces

Implement this task in the active `agent/` harness. Follow the hardening README's
engineering and validation instructions.

## Problem

TUI, headless and ACP construct different runtime policies. In
`agent/src/cli.rs`, `run_headless_query` supplies no compaction state or recency
context and uses a default retry context. Permission installation also differs.
Inspect `agent/src/tui/provider/live/`, `agent/src/acp/` and `agent/src/run/`.

## Work

Define one shared setup contract for resolved model metadata, instructions,
tool registration, permission policy, compaction, retry/fallback configuration
and session state. Keep interaction callbacks and event presentation
surface-specific. Compare plausible designs and choose the smallest coherent
abstraction; do not create a new framework or blindly copy the predecessor.

Coordinate the contract with prompts 02, 05, 06, 07, 09 and 16. Do not silently
change model selection or permission defaults during a structural refactor.

## Acceptance

- Equivalent configured sessions receive equivalent execution policies.
- Headless runs have usable context metadata and staged compaction.
- Surface differences are explicit and tested.
- Fake-provider tests cover setup and a long headless run without paid calls.
