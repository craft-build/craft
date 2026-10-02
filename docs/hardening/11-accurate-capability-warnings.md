# Make help and capability warnings truthful

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`run_print` emits stale "not implemented" warnings for image/verbose options
although downstream paths handle them. SDK input, Flow and fork support have
different availability. README/help text also contains migration-era claims.

## Work

Audit option handling, help, warnings and `README.md`/`agent/README.md` against
actual behavior. Remove stale warnings only after confirming end-to-end support.
Clearly separate output streaming from SDK input/control compatibility.
Reject unsupported safety-affecting behavior; avoid broad parity claims.
Do not treat parser acceptance as implementation.

## Acceptance

- Supported flags produce no false unimplemented warnings.
- Unsupported features fail or warn according to an explicit contract.
- Help describes the actual binary, modes and output variants.
- CLI smoke tests capture stderr and exit status without paid provider calls.
