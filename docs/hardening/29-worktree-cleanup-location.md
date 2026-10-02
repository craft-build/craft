# Make worktree cleanup independent of process cwd

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/tools/worktree.rs::drop` uses `git -C` for worktree removal but a bare
`git branch -D` for branch deletion. Cleanup can run outside the source repo and
leave branches behind. The predecessor has a similar issue.

## Work

Retain repository identity and run every cleanup command against that repository
explicitly. Define failure reporting and cleanup ownership, especially when
subagents fail or are cancelled. Never delete an unrelated or user-owned branch.
Coordinate with the removal of global cwd changes in prompt 04.

## Acceptance

- Cleanup works when process cwd is outside the repository.
- Temporary-repository tests cover normal completion, cancellation and failures.
- Only the task's owned worktree/branch is removed.
- Cleanup errors are observable without panicking during drop.
