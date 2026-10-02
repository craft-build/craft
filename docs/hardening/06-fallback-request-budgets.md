# Rebudget requests for each fallback model

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/run/retry.rs` clones a request prepared for the primary model when
calling fallbacks. A fallback may have smaller context or output limits.
The predecessor clamps per model in
`~/Projects/craft/craft-agent/src/agent/streaming.rs`.

## Work

Carry the needed capability/limit metadata into fallback selection and prepare
or clamp each attempt for its actual model. Coordinate with complete-request
budgeting. Define what happens when history cannot fit a fallback; do not
silently truncate essential user input or loop indefinitely.
Preserve retry budgets, cancellation and accurate model/usage reporting.

## Acceptance

- A small fallback never receives the primary model's oversized output cap.
- Context incompatibility has a bounded, explicit recovery or failure path.
- Returning to another model does not retain an incorrectly mutated request.
- Tests use fake primary/fallback models with different limits and failures.
