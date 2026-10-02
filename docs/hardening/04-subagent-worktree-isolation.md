# Give isolated subagents a real child workspace

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/subagent.rs` creates a worktree but builds child tools from the parent
workspace. Its isolation helper changes process-wide cwd. Filesystem tools and
default bash cwd still use `Workspace::root()`.

## Work

Build child execution state rooted at the child worktree: tools, snapshots,
instructions, sandbox roots and path-scoped permission behavior. Preserve
intended shared services without sharing mutable path identity. Remove global
cwd changes from the isolation mechanism. Explicitly handle worktree creation
failure; do not advertise isolation while silently editing the parent.

## Acceptance

- Child read/edit/write/bash operate in the child worktree.
- The parent checkout remains unchanged until an explicit integration operation.
- Concurrent isolated children cannot change each other's cwd or roots.
- Cancellation and error cleanup are correct.
- Temporary-repository tests verify files, command cwd and concurrent children.
