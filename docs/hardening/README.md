# Harness hardening prompts

These are independent implementation prompts derived from the comparison of the
active CLI harness in `agent/` with its predecessor at `~/Projects/craft`.
The comparison inspected new revision `7ace4b01` and reference revision
`ddf676b0`, including their working trees. Findings are starting points, not a
substitute for checking the current implementation.

## Instructions for every task

- Work on the active harness in `agent/`, not the root desktop application,
  unless the task explicitly requires an integration change.
- Read applicable `AGENT.md`/`AGENTS.md` instructions before editing.
- Inspect the named paths and reproduce the issue before choosing a fix.
  Preserve existing correct behavior; do not assume the predecessor is correct.
- The predecessor at `~/Projects/craft` is a read-only reference. Do not edit it
  unless it is attached and explicitly included in the assignment.
- Keep changes focused. Add behavioral regression tests, not just parser tests.
  Use fake providers, temporary repositories and mock servers where possible.
  Do not make paid model calls or publish anything without explicit approval.
- Do not write credentials into files, weaken permission checks to make tests
  pass, or enable unsandboxed execution implicitly.
- Finish with the behavior changed, tests run, limitations and any follow-up.
  Update user-facing documentation when a contract changes.

## Suggested execution order

First agree on the shared runtime setup contract in **01**. Permissions,
sandboxing, isolation and budgeting are release blockers for relying on their
advertised guarantees. Other tasks can run independently where their files and
contracts do not overlap.

Priority here is relative to this hardening effort, not a formal incident
severity. Compatibility tasks are conditional on intended product scope.

| Prompt | Priority | Scope |
| --- | --- | --- |
| [01 Shared runtime setup](01-shared-runtime-setup.md) | High | Architectural prerequisite |
| [02 Headless permissions](02-headless-permissions.md) | Critical | Confirmed defect |
| [03 Sandbox policy](03-sandbox-policy.md) | Critical | Confirmed regression |
| [04 Subagent worktree isolation](04-subagent-worktree-isolation.md) | High | Inherited correctness defect |
| [05 Complete request budgeting](05-complete-request-budgeting.md) | High | Confirmed accounting defect |
| [06 Fallback request budgets](06-fallback-request-budgets.md) | High | Retry correctness |
| [07 CLI permission and command flags](07-cli-permission-and-command-flags.md) | High | Silent option gaps |
| [08 Interactive startup prompt and mode](08-interactive-startup-prompt-and-mode.md) | High | Startup contract |
| [09 Continue and fork semantics](09-continue-and-fork-semantics.md) | High | Session safety and compatibility |
| [10 Apply prompt overrides once](10-apply-prompt-overrides-once.md) | Medium | Confirmed duplication |
| [11 Accurate capability warnings](11-accurate-capability-warnings.md) | Medium | CLI/documentation truthfulness |
| [12 Canonical move tool permissions](12-canonical-move-tool-permissions.md) | High | Permission identity mismatch |
| [13 Full-file write concurrency](13-full-file-write-concurrency.md) | High | Lost-update protection |
| [14 Honest and complete undo](14-honest-and-complete-undo.md) | High | Reversibility contract |
| [15 Safe recursive snapshot traversal](15-safe-recursive-snapshot-traversal.md) | High | Workspace boundary |
| [16 Explicit models without catalog membership](16-explicit-models-without-catalog-membership.md) | Medium | Offline/manual configuration |
| [17 Release and updater identity](17-release-and-updater-identity.md) | High before distribution | Packaging and integrity |
| [18 SDK automation compatibility](18-sdk-automation-compatibility.md) | Conditional | Missing predecessor protocol |
| [19 ACP attachments and client MCP](19-acp-attachments-and-client-mcp.md) | Conditional | Editor compatibility |
| [20 Reasoning and fast-mode controls](20-reasoning-and-fast-mode-controls.md) | Conditional | Provider capability parity |
| [21 Repository navigation capabilities](21-repository-navigation-capabilities.md) | Conditional | Evaluate before restoring |
| [22 Programmable tool orchestration](22-programmable-tool-orchestration.md) | Conditional | Evaluate before restoring |
| [23 TUI file-link regression tests](23-tui-file-link-regression-tests.md) | Medium | Observed test failures |
| [24 Formatting and build warnings](24-formatting-and-build-warnings.md) | Low | Observed validation failures |
| [25 Provider conformance tests](25-provider-conformance-tests.md) | Medium | Behavioral parity evidence |
| [26 Harness comparison benchmark](26-harness-comparison-benchmark.md) | After core fixes | Outcome evidence |
| [27 Synthetic grace-message cleanup](27-synthetic-grace-message-cleanup.md) | Medium | Resume history correctness |
| [28 Session model index fallback](28-session-model-index-fallback.md) | Medium | Stale/corrupt index recovery |
| [29 Worktree cleanup location](29-worktree-cleanup-location.md) | Medium | Inherited cleanup defect |

## Baseline validation

The comparison ran `cargo test -p craft --offline`: 1,809 tests passed and two
TUI file-link header tests failed. `cargo fmt -p craft --check` reported existing
differences. The CLI built, and unsupported SDK input and Flow mode were rejected.
No paid-model quality, cost or latency comparison was performed.

Use the package selector `-p craft` for the active harness; its binary is
`crafty`. Verify current commands and results rather than treating this baseline
as permanently accurate.

## Deliberately not blanket porting

Flow, wiki workflows, Lua package management, plugin-defined UI and every old
slash command are not automatic hardening requirements. Prompts 18–22 explicitly
require a scope decision or evaluation. Keep the simpler architecture unless a
concrete workflow justifies bringing complexity back.
