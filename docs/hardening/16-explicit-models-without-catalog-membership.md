# Support explicit models without mandatory discovery membership

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/model_selection.rs::resolve_spec` requires catalog membership.
Explicit manual models can fail with stale, incomplete or disabled discovery,
while the provider registry can construct an exact model directly.

## Work

Keep safe catalog-based implicit selection but support explicit provider/model
requests under a documented validation policy. Distinguish listing failures,
unknown configured providers and provider-side model rejection. Resolve limits
from explicit configuration or conservative unknown-metadata behavior.
Unify TUI/headless/ACP semantics without silently substituting a different model.

## Acceptance

- Explicit configured models work with discovery disabled or incomplete.
- Implicit defaults remain deterministic and capability-aware.
- Unknown providers and empty model IDs fail clearly.
- Fake-provider tests cover manual models, discovery failures and resume.
