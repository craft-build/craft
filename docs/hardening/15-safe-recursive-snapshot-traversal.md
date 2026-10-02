# Keep recursive snapshot traversal inside the workspace

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/tools/delete.rs::note_tree` uses `path.is_dir()`, which follows child
symlinks despite top-level path resolution rejecting symlink components.
This can traverse external trees or cycles while collecting snapshots.

## Work

Use non-following metadata and skip symlink/special entries. Reuse workspace
boundary checks where appropriate. Bound traversal and make skipped or failed
snapshot coverage visible without weakening deletion safety. Inspect similar
recursive mutation/snapshot walkers for the same rule violation.

## Acceptance

- An in-tree link to an outside directory is never read into a snapshot.
- Symlink cycles terminate and special files are not opened as text.
- Normal recursive deletion and supported undo coverage still work.
- Temporary-directory tests check external files remain untouched.
