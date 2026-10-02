# Evaluate programmable tool orchestration before restoring it

This is conditional capability work in the active `agent/` harness.
Follow the hardening README.

## Problem

The predecessor's `craft-interpreter` provides a programmable execution model
for tool orchestration. The new harness uses direct native tools and batch
dispatch. These strategies have different overhead and safety properties.

## Work

Compare concrete multi-tool workflows and identify what direct/batch tools
cannot express efficiently. Recommend retaining the simpler model or restoring
a narrowly scoped interpreter capability. Do not add Lua/plugin infrastructure
as an incidental dependency. If implementing, route every host call through the
same permission, sandbox, cancellation, output-budget and audit pipeline.

## Acceptance

- The scope decision is backed by reproducible workflow evidence.
- Programmatic host calls cannot bypass restrictions.
- Execution time, memory, output and recursion are bounded.
- Tests cover denial, cancellation and partial failure.
