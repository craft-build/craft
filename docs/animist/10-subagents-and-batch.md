# 10 — Subagents and `batch` on animist

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt)
  and the [harness design rules](harness-design.md), especially
  [State ownership](harness-design.md#state-ownership). Start with the
  [test inventory](harness-design.md#test-integrity).
- When implementation is finished, **run the review agent**
  (`.craft/agents/reviewer.md`) as described in [Finish](#finish).

## Goal

The `task` tool runs each subagent as its own `animist::agent::Agent`, with a
child `History`, a restricted `ToolRegistry`, and the parent's policy layers.
`batch` runs its children concurrently through `ToolContext::dispatch_all`, so
they share the parent's layers, cancellation, budgets and nesting limit.

## Read first

- Craft:
  - `agent/src/subagent.rs` (`SubagentLauncher`, the output-schema validation,
    the child tool sets)
  - `agent/src/tools/{task.rs, batch.rs, review.rs}`
  - `docs/hardening/04-subagent-worktree-isolation.md`
  - `agent/src/run/events.rs` (`Event::Subagent` filtering)
  - session `subagent_messages`/`subagents` storage
- animist 0.7.0: `docs/agents.md` → *Bounded scheduling*. Note:
  - only exclusive tools dispatch children;
  - `dispatch_all` runs children in contiguous conflict-free waves, in call
    order, bounded by `max_concurrent_tools`; results come back in call order
    and events in completion order;
  - `dispatch_all` from a child context fails with `NestedConcurrentDispatch`
    (nested orchestrators stay serial through `dispatch`);
  - `max_child_calls` counts every attempt, and `dispatch_all` reserves the
    whole batch upfront;
  - a per-child denial, unknown tool or execution error is an `is_error`
    output for that child only; `Stop`, cancellation and fatal errors drop
    in-flight siblings and return a typed error;
  - child results belong to the parent, not the model's tool results.

## Work

1. **Subagents.** 05 already runs children as a child `Conversation`. This
   prompt finishes the job. The launcher builds the child through the same
   `ConversationBuilder` the surfaces use, with:
   - a fresh `History`;
   - the child model (tier or explicit);
   - the restricted tool set (with `task` removed to prevent recursion,
     matching today);
   - **its own layer instances**, built from the parent's shared *services*:
     the permission manager, the approval responder (so 07's serialization
     still means one prompt at a time across parent and children), guardrail
     config, auto-review config and the event sink (wrapped for subagent
     tagging). This matches today's behaviour, where "the task tool spawns
     subagents under the turn's shared permission gate". **Never share a
     layer instance with the parent**: `RunContext::id` restarts at zero per
     agent, and per-run fields reset in `turn_start` would clobber the
     parent's state mid-run;
   - `RecoveryLayer`/`RequestLayer` configured for the child window;
   - no `PersistLayer`: the parent stores the child's entries (below).

   Run it with `harness::driver::run_turn`:
   - Cancellation is linked to the parent's `context.run.cancellation`: the
     child's `Cancellation` is cancelled when the parent's fires, via an
     awaited `select!` inside the tool, not a detached task.
   - Events are wrapped as `CraftEvent::Subagent { tool_use_id:
     context.call.id, event: Box<harness::Event> }`, with today's filtering
     applied to the inner event.
   - Return the final assistant text, or the validated JSON for
     `output_schema`.
   - Always call `shutdown()` on the child agent, including on error and
     cancel.
   - Persist the child's `snapshot().entries()` in the session's subagent
     histories (the entry lists 04 defined) when the child ends, on every
     exit path.
2. **Worktree isolation.** Keep hardening/04's behaviour. The child workspace
   root flows into the child tools and the scheduling-claim normalization.
3. **`batch`.** Its scheduling claim is `Exclusive` (required for child
   dispatch). It hands all its entries to one
   `ToolContext::dispatch_all(Vec<ToolCall>)` call.
   - Children run in parallel where their 06 claims allow and serialize where
     they conflict, under the same wave rules as model-emitted calls.
   - Model-emitted parallel tool calls are parallel too, so measure whether
     models still need `batch`, and update its description to match.
     Retiring `batch` is a valid outcome; record the decision.
   - Nested `batch` stays rejected before dispatch (animist would also reject
     it with `NestedConcurrentDispatch`). Per-child failures already come
     back as per-entry `is_error` outputs. Map a batch-level
     `ChildCallLimitExceeded` or `ToolDepthExceeded` (returned for the whole
     call because the batch is reserved upfront) to per-entry error text, not
     a run failure. `Stop` and cancellation propagate.
   - Confirm that 06 removed the `SharedDispatch` back-reference, and delete
     anything left of it.
4. **Review tool.** `review` (the reviewer subagent) uses the same launcher.
   Make sure `.craft/agents/reviewer.md`-style agent definitions still load and
   restrict tools.
5. **Limits.** Set `max_child_calls` and `max_tool_depth` so a full batch (find
   today's maximum entry count) fits. Pin it with a test.

## Tests

- Port the `subagent.rs` and `task.rs` tests onto `ScriptedProvider`. Parent
  and child each get their own scripted provider (or a shared one with tagged
  scripts).
- A child denied by the parent's permission rules gets a denial result.
- Parent cancellation cancels the child promptly, and the child agent is shut
  down.
- `output_schema` validation failure behaves as today.
- `batch` children overlap when independent (barrier-instrumented tools, no
  timing), serialize in call order when they conflict, and return results in
  call order.
- `batch`: children pass through approval and guardrails; two gated children
  never show two approval prompts at once (07's serialization); nested batch
  is rejected; partial child failures stay per entry; a child-limit overflow
  gives per-entry errors; a child `Stop` drops its siblings.
- Child events reach the parent's stream tagged with the call id, and the
  child's `Done`/`Error`/`ToolOutput`/`ToolPending` are filtered.

## Acceptance

- `subagent.rs` holds only child-specific code (tier and model choice,
  tool restriction, schema validation, worktree isolation). Construction goes
  through `ConversationBuilder` and runs through `run_turn`, with no
  duplicated turn orchestration.
- No layer instance is reachable from both a parent and a child agent. Test
  this with a layer that counts `turn_start` calls per instance.
- No `SharedDispatch` remains.
- Hardening/04's isolation tests pass.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note:
   - whether `batch` is kept;
   - the measured overlap;
   - the limits chosen;
   - the test-inventory diff;
   - the indirection rows closed.

## Completion note

**Done 2026-11-01.** Working tree on `animist-migration` (one commit series
with this note). Validation: `cargo fmt --check` clean; clippy
`--all-targets` **115** warnings, byte-exact parity with the pre-10 tree
(stash-compared); `cargo test -p craft` **2005 passed, 3 failed, 1
ignored** — the failures are the three recipe-render fixed points, the
ignored one is the argosy env gate (the 01 baseline).

### `batch` decision: kept

Both paths to parallelism were measured with barriers, not wall clocks:
model-emitted parallel calls overlap (`run_tests::dispatch::
independent_calls_run_concurrently`, restored in 06) and `batch` children
overlap under the same wave rules (`tools::batch::tests::
independent_children_overlap`, a two-party barrier that only fills when
both children are in flight). Since the mechanism is duplicated, retiring
`batch` was considered and rejected: it still gives one round-trip for a
25-entry fan-out with a per-entry report and a partial-failure summary, it
is the documented orchestration idiom in the subagent tool sets and the
task tool's description, and its Exclusive claim is the only shape that
may dispatch children at all. Its description now says plain parallel
calls are also fine for a handful of operations (model-visible string
change, named by this prompt).

### Behaviour changes (complete list, each pinned)

1. **A batch-level child-budget or depth limit is per-entry error text**
   (`ChildCallLimitExceeded`/`ToolDepthExceeded` render as
   `[ERROR] …` per entry with the `0/25` footer), not one whole-call
   `batch dispatch failed: …` result — the model can split the fan-out.
   Fatal dispatch errors (cancellation, a layer stop) keep the whole-call
   wording and propagate. *Pins:* `a_child_limit_overflow_reports_
   per_entry_errors`, `cancellation_during_dispatch_is_the_batchs_error`,
   `a_child_stop_drops_its_siblings`.
2. **Worktree isolation fails explicitly** outside a git repository (or
   when `git worktree add` fails): the spawn errors instead of silently
   running in the parent tree. The `task` tool's `isolation` doc says so
   (model-visible string change, named by hardening/04). *Pins:*
   `worktree_isolation_fails_explicitly_outside_a_git_repo`,
   `create_fails_explicitly_without_git_repo`.
3. **Isolated children run on a child workspace** rooted at the worktree
   (`Workspace::child_workspace`): tools, snapshots, bash cwd, sandbox
   workspace root and scheduling-claim normalization anchor at the child
   root; instructions dedupe, todos, MCP, the sandbox cell, the state dir
   and the cross-agent path locks stay shared; no subagent seam, no plan
   file. The process-wide `chdir` under a global mutex (`run_isolated`)
   is gone — concurrent isolated children no longer serialize.
   *Pins:* `worktree_children_run_in_the_child_root`,
   `concurrent_isolated_children_get_their_own_roots`.
4. **Worktree children resolve permission scopes against the child root**
   (`PolicyLayer::authorize` uses the conversation's `write_root`), so an
   allow/deny rule scoped to the parent tree neither auto-approves nor
   denies writes inside the isolated copy. Verified load-bearing: the pin
   fails against the parent-rooted resolution. *Pin:*
   `worktree_children_resolve_scopes_against_the_child_root`.
5. **Child histories are persisted** (`subagent_messages` +
   `subagents` roster, the entry lists 04 defined) on every exit path —
   success, schema failure, error and cancel. Nothing wrote them before.
   A failed append rolls the session mirror back and surfaces
   `PersistenceFailed`. *Pins:* `the_child_history_is_persisted_when_
   it_succeeds`, `…_when_it_fails`, `a_call_without_an_id_persists_
   nothing`.
6. **The parent↔child cancellation link is an awaited `select!` inside
   the tool** (`run_linked`): the two detached watcher tasks are gone;
   the run is always awaited to completion after the link fires.
   *Pins:* `parent_cancellation_mid_run_cancels_the_child_promptly`
   (mid-run cancel of a blocked child), `cancelled_parent_short_
   circuits_child` (pre-cancelled).

### Limits

`max_child_calls = u64::MAX` (a full 25-entry batch — `MAX_BATCH_SIZE`,
now exported — is reserved upfront and always fits) and
`max_tool_depth = MAX_TOOL_DEPTH = 8`, previously animist's implicit
default and now craft-owned in `ConversationBuilder`. *Pin:*
`a_full_batch_fits_the_child_budget` (config assertions plus a real
25-entry batch through a conversation reporting `All 25 tools executed
successfully.`).

### Acceptance checks

- `subagent.rs` holds only child-specific code; construction is
  `ConversationBuilder` and the run is `run_turn` — no duplicated turn
  orchestration.
- No layer instance is shared: pinned by
  `no_layer_instance_reaches_both_parent_and_child` (a `turn_start`
  counter on the parent's chain-head test layer does not move while the
  child runs; mounted through the new test-only
  `ConversationBuilder::test_layer` seam).
- No `SharedDispatch` remains (grep clean); `set_current_dir` is gone
  from the crate.
- Hardening/04's suite: child tools in the worktree, parent checkout
  unchanged, concurrent children with distinct roots and an unmoved
  process cwd, explicit creation failure.
- The reviewer: `review` rides the same `SpawnSubagent` launcher; the
  registry serves exactly `REVIEWER_TOOLS` (+ `list_tools`) and every
  tool the vendored `.craft/agents/reviewer.md` definition advertises is
  servable (`reviewer_registry_serves_exactly_the_reviewer_budget`,
  `the_reviewer_definition_admits_only_servable_tools`); the install
  tests in `runtime.rs` still pass.
- Filtering: forwarded child events are tagged with the call id; the
  child's `CraftEvent::Done`/`Error`/`PersistenceFailed` and animist
  `ToolProgress`/`Finished` are dropped while `ToolFinished` passes
  (`child_noise_events_are_filtered_and_completion_passes`,
  `forwarded_events_carry_the_call_id`).
- 07's serialization holds across batch children: two gated `astgrep`
  children genuinely overlap (a second gated arrival lands while the first
  ask is being answered) yet the responder sees one ask at a time
  (`two_gated_batch_children_never_show_two_prompts`).

### Test-inventory diff

1992 → **2009** listed (+22, −5, every removal mapped):

- `dispatch_failure_is_reported_with_our_wording` → split into
  `a_child_limit_overflow_reports_per_entry_errors` (the limit case, new
  shape) and `cancellation_during_dispatch_is_the_batchs_error` (the
  fatal wording survives on the fatal class).
- `create_returns_none_without_git_repo` →
  `create_fails_explicitly_without_git_repo` (the new explicit contract).
- `run::cancel::tests::child_{cancelled_by_parent,
  cancelled_by_own_trigger, of_cancelled_parent_starts_cancelled}` —
  deleted with the now-orphaned `CancelToken::child` (a detached link
  spawn with no production callers after the awaited `select!`); the
  link semantics live on in
  `parent_cancellation_mid_run_cancels_the_child_promptly`.

Beyond the replacements: the barrier/order suite, the stop/cancel
propagation pair, the approval-serialization and full-batch pins, the
linked-cancellation pair, schema exhaustion, the filter pin, the
layer-instance pin, the isolation suite (rooting, scopes, failure,
concurrency), the three persistence pins, and the two reviewer-budget
pins.

### Indirection rows closed

- `run_isolated` (global `WORKTREE_GUARD` mutex + process `chdir`) →
  child-rooted workspaces.
- The two detached subagent cancel watchers and `CancelToken::child` →
  the awaited `run_linked` select.

### Review

Two review-agent passes over the full diff. First pass:
**approve_with_nits** (P2×3, P3×6). All three P2s fixed — worktree scope
rooting (+ load-bearing pin), the `record_subagent` mirror rollback with
`PersistenceFailed` surfacing, and this note. Four P3s fixed: snafu
  error for `Worktree::create` (`tools::error::Error::Tool`, same text),
  `CancelToken::child` deleted, distinct session/call ids in the
  concurrency fixture. Two P3s justified as skips: a doom-stopped child
  returning its final text as a normal success is pre-migration parity
  (main had the same final-text fallback and this prompt names no
  change), and the synchronous `git` calls in worktree create/Drop are
  pre-existing mechanics (`Drop` cannot await) left for 12's async audit.
  Second pass: **approve_with_nits** (P2×1, P3×1) — it confirmed the
  three P2 fixes and four of the P3 fixes, but caught the first overlap
  witness counting a monotone total (tautological) and a stale
  `CancelToken::child` mention in `run/cancel.rs`'s module doc. Both
  fixed: the witness now counts gated arrivals that land while another
  ask is being answered (impossible under claim serialization), and the
  doc points at the awaited `run_linked` select. Third pass over those
  two fixes: clean.

### Follow-ups

- The task-chats view (task 96) still renders forwarded events only;
  11's resume can now also read the persisted `subagent_messages`.
- bash's `<untrusted-content>` wrapper around command output is carried
  verbatim into persisted child tool results; unwrapping for display is
  a surface concern.
