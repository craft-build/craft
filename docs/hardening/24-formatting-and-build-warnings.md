# Resolve the observed formatting and build warnings

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`cargo fmt -p craft --check` reported differences in `headless.rs`,
`knowledge_memory.rs` and `tui/provider/live/commands.rs`.
The build reported deprecated MCP sampling types and an unused recipe description.

## Work

Verify current failures, then make the smallest formatting and warning fixes.
For deprecated protocol APIs, inspect upstream compatibility before changing
behavior; do not suppress warnings broadly. Use recipe description meaningfully
or remove unused state if it has no intended consumer.
Report toolchain/dependency linker warnings separately from source defects.

## Acceptance

- `cargo fmt -p craft --check` passes.
- Harness tests and an appropriate Clippy check run.
- Any remaining warnings have a specific explanation and follow-up.
- No unrelated wholesale formatting or dependency upgrade is mixed in.
