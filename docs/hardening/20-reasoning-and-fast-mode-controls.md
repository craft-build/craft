# Define provider-aware reasoning and fast-mode controls

This is conditional capability work in the active `agent/` harness.
Follow the hardening README.

## Problem

The predecessor exposes interactive thinking and fast-mode controls. Streaming
reasoning text in the new harness does not establish request-level reasoning
control. Some compatibility flags were ignored in both versions.

## Work

Inventory actual provider capabilities and current request construction. Define
which controls are required, their normalized representation, persistence and
surface exposure. Use native provider semantics where needed, not one parameter
blindly sent to every backend. Keep execution mode and pricing metadata aligned;
never charge/display fast-mode prices without matching request behavior.

## Acceptance

- Fake request captures verify supported controls reach each provider adapter.
- Unsupported controls produce explicit feedback.
- Model changes and resume preserve or reset controls according to policy.
- UI, CLI/ACP where supported, and usage display agree.
