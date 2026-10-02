# Budget the complete model request

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/run/mod.rs` and `agent/src/compaction/engine.rs` budget history without
system-prompt and tool-schema overhead. `agent/src/compaction/estimate.rs`
already exposes an overhead-aware estimator. Independent rounding also
undercounts collections of very short messages.

## Work

Use one complete-request accounting rule for proactive compaction, output caps
and overflow decisions. Include preamble, instructions, active tool definitions,
message structure and image estimates. Aggregate before rounding where
appropriate. Keep estimates conservative and explain unknown-context behavior.
Avoid rebuilding expensive schema estimates on every token/event.

## Acceptance

- Large instructions and MCP schemas trigger compaction before provider overflow.
- Short-message collections contribute nonzero aggregate cost.
- Tool-set changes update overhead accounting.
- Output reservation and threshold logic agree.
- Fake-provider and estimator tests cover boundaries, images and unknown limits.
