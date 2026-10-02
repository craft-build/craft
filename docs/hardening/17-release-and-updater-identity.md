# Separate the new harness's release and update identity

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/storage/version.rs` and `agent/src/update.rs` target the predecessor's
`craft-build/craft` releases/install script, while the new binary is `crafty`.
The version comparator also treats prerelease versions as not newer; inspect
behavior for the current beta version.

## Work

Confirm the intended release repository, artifact names, executable destination,
version policy and rollback contract with the developer. Do not guess publishing
identity. Then fix lookup/install/backup behavior together. Preserve integrity
verification and explain what same-origin digests do and do not guarantee.
Evaluate the build-time script pin across future script versions.

## Acceptance

- Mock release/install fixtures update the intended executable only.
- Beta/stable/prerelease comparisons match the agreed policy.
- Missing/mismatched integrity data fails closed.
- Rollback targets the same executable and preserves permissions.
- Do not run a real installer, overwrite host binaries or publish a release.
