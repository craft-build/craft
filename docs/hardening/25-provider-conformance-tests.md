# Establish provider behavioral conformance evidence

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

A broad Rig provider registry does not prove predecessor parity for streaming,
tool calls, usage, authentication, reasoning, limits and error handling.

## Work

Build a prioritized provider capability/conformance matrix based on intended
users. Add request/response fixtures and mock transport tests for representative
native and OpenAI-compatible adapters. Cover authentication failure, partial
streams, tool-call assembly, cache usage, limits and retry classification.
Distinguish verified adapters from untested registrations.

## Acceptance

- Tests are offline and require no credentials.
- Fixtures assert model input and normalized events, not just construction.
- Capability documentation reflects evidence and known limitations.
- Live smoke tests, if desired, require separate approval and a bounded budget.
