# Diagnose and fix the observed TUI file-link failures

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

The comparison test run failed these tests in
`agent/src/tui/ui/messages/mod.rs`:

- `assistant_markdown_read_highlight_and_painted_card_header`
- `render_injects_osc8_into_visible_header_and_skips_scrolled_out`

Assertions expected OSC-8 file links in visible tool headers. Determine whether
the cause is rendering, wrapping, capability detection or a stale test contract.

## Work

Reproduce both failures and inspect the actual header/link pipeline. Preserve
clickable paths, wrapping, background painting and offscreen behavior. Do not
merely remove the assertions or force terminal capabilities globally.

## Acceptance

- Both tests pass with meaningful coverage.
- Narrow viewports, long paths and scrolled-out cards are covered.
- Run focused tests and the full harness suite.
- State what a real terminal smoke check confirms, or why it was unavailable.
