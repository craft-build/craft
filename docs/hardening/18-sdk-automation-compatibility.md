# Decide and implement the SDK automation contract

This is conditional compatibility work in the active `agent/` harness.
Follow the hardening README.

## Problem

The new CLI supports stream-json output but rejects stream-json SDK input.
The predecessor implements input/control messages in
`~/Projects/craft/src/sdk_mode.rs` and also has a standalone JSON output variant.

## Work

First identify the required consumers and supported protocol subset. If SDK
compatibility is in scope, implement input framing, session lifecycle, control
requests, permission responses, interruption and model changes using shared
runtime setup. Evaluate whether standalone JSON output is needed.
Do not promise full compatibility from similarly named flags.

## Acceptance

- Recorded protocol fixtures exercise multi-turn input and control messages.
- Permission decisions, cancellation and malformed frames are safe and bounded.
- Stdout remains protocol-clean; diagnostics use stderr.
- Unsupported messages and negotiated capabilities are explicit.
- If out of scope, document the limitation and reject misleading options.
