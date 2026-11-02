# 07 — Tool policy layers: approval, plan mode, guardrails, doom loop, auto-review

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt)
  and the [harness design rules](harness-design.md), especially
  [Layers](harness-design.md#layers). Start with the
  [test inventory](harness-design.md#test-integrity).
- When implementation is finished, **run the review agent**
  as described in [Finish](#finish).

## Goal

Every tool-call policy becomes real layer code. This replaces the body of
05's interim `PolicyLayer` and completes 05's dedup-only `GuardrailLayer`.
Surfaces supply interaction (the approval UI, ACP permission requests, the
headless policy) through one narrow craft trait. The layers own sequencing
and state.

Only two policy layers exist, `PolicyLayer` and `GuardrailLayer`, in that
order at the head of the [chain](harness-design.md#chain). Don't add a layer
per rule. A denial short-circuits everything after it, so the order *is* the
policy, and it must read top to bottom in one place.

## Read first

- Craft:
  - `agent/src/run/dispatch.rs` (`ToolDispatch::execute`: today's order is
    approval → plan-mode write gate → guardrails → dedup/execute → after hook;
    verify it)
  - `agent/src/run/{guardrails.rs, doom.rs, mode.rs}`
  - `agent/src/permissions/`, `agent/src/auto_review.rs`
  - `agent/src/tui/provider/live/approval.rs`, `agent/src/acp/permissions.rs`,
    `agent/src/headless.rs` (`HeadlessGate`)
  - `docs/hardening/02-headless-permissions.md`,
    `docs/hardening/12-canonical-move-tool-permissions.md`
- animist 0.7.0: `src/agent/layer/mod.rs`, plus the denial, respond, stop and
  step-boundary semantics in `docs/agents.md` (*Layers*, *Step boundaries*):
  - denial and `Respond` short-circuit later layers;
  - `post_tool_exec` still sees denied, responded and unknown results;
  - `step_end` runs head-to-tail after a step's results commit and can append
    durable `Role::User` observations;
  - `Stop` terminates the run and closes unfinished calls;
  - layers must be concurrency-safe when `max_concurrent_tools > 1`.

## Work

1. **The approval seam.** Replace `BeforeExecute` with a craft
   `ApprovalResponder` trait shaped like today's interaction (ask the user or
   client, get allow/deny/stop plus "remember" scope).
   - The TUI `ApprovalGate`, the ACP `AcpPermissionGate` and `HeadlessGate`
     implement it.
   - Permission rule evaluation (`permissions/`) and auto-review sit in
     `PolicyLayer`, not in the responders, so all three surfaces get identical
     policy. Keep surface-specific defaults as explicit inputs, per
     hardening/01.
2. **`PolicyLayer::pre_tool_exec`**:
   1. permission rules → allow, deny or ask;
   2. if ask and auto-review is enabled → auto-review (emits
      `AutoReviewStart`/`Decision` through the conversation event sink);
   3. else → the responder;
   4. the plan-mode write restriction (`PLAN_WRITE_RESTRICTED`, with the plan
      path), read from `TurnSettings` at `turn_start`. Switching modes
      doesn't rebuild the agent.

   It maps onto `ToolDecision::{Run, Deny{reason}, Stop{reason}}`. Keep the
   exact model-visible denial texts. Child calls (`ToolContext::parent_call_id`
   is `Some`) go through the same pipeline; that already applies to `batch`
   children today.
3. **`GuardrailLayer`**:
   - **`pre_tool_exec`**:
     - Doom-loop repeat: `RecentCalls::is_doom_loop` → `Deny` with
       `DOOM_LOOP_MESSAGE`; bump `ToolBatchOutcome::doom_loops`; clear the
       window.
     - Guardrail block → `Deny` with `GUARDRAIL_BLOCK_MESSAGE`. A guardrail
       warning is recorded for `post_tool_exec` to append (the `[guardrail] `
       prefix), plus the `Event::Info` today's `commit_wave` emits.
     - Dedup: fold 05's `DedupLayer` in here, after the doom and guardrail
       checks, still answering hits with `ToolDecision::Respond`.
   - **`post_tool_exec`**: `record_result` and the doom tracker updates
     (`note_tool_error`/`success`).
   - **Doom hard stop**: when `DoomTracker::should_hard_stop()`, return
     `ToolDecision::Stop` with a tagged reason. The driver maps
     `RunStatus::Stopped` plus that reason to `RunOutcome::DoomStop` and adds
     the END marker.
   - **Grace prompt** (`step_end`): when `doom.should_grace()` holds after a
     step's results commit, return a `StepAction` whose messages carry the
     grace prompt as a `Role::User`, `MessageKind::Observation` message tagged
     `craft.marker = grace`, and emit `StagnationDetected` once.
     - This replaces today's mid-run `turn.push` with a durable append inside
       the same run: no `Stop`, no driver re-run, and sibling calls in the step
       finish normally.
     - If the step ended without tool calls (`StepOutcome::terminal` is
       `Some(Completed)`), set `continue_run` so the model sees the grace
       prompt; otherwise the run continues on its own.
     - The doom tracker is the same object, so the hard stop still fires on a
       later step.
     - A `HistoryChanged` from the append (an external edit during the step)
       maps to `Failed` like any other conflict; the retained messages are
       logged.
   - **State reset**: the tracker and guardrails reset when compaction rewrites
     history (08's compaction calls `reset()` on the typed handle
     `Conversation` holds), and on new user input. That reset happens in
     `GuardrailLayer::turn_start`, which replaces
     `reset_for_new_user_input` at the call sites.
   - Doom tracker, guardrail counters and the dedup cache live in one
     `GuardrailState` behind an `Arc`, shared by the layer and that typed
     handle. Delete the old `Shared*` type aliases.
4. **Concurrency.** These layers run concurrently once parallel waves are on.
   Guard shared state with `std::sync::Mutex` and never hold it across
   `.await`. Responders that block on the user hold no layer lock.
   - **`question`**: it is `Exclusive`, so it never overlaps with other calls.
   - **Approval**: today's dispatch calls `before.decide` per call. Find out
     whether it can run concurrently: two prompts at once may break the TUI
     modal. Serialize interactive approvals with an async mutex inside the
     responder if needed. Pin that with a test.
5. **Remove** the interim `PolicyLayer` body from 05, plus `BeforeExecute`, `AfterExecute`,
   `Decision` and `DispatchOutcome`, once nothing uses them.

## Tests

- Port `run/tests/dispatch.rs`, `permissions/tests.rs` (dispatch-level parts),
  `tools/batch.rs` hook tests, the `headless.rs` gate tests and `acp/tests.rs`
  permission tests onto the layer chain, using `ScriptedProvider`.
- Order: denial by rules never reaches the responder; plan mode blocks a
  responder-approved write; a guardrail block after approval still blocks.
- Doom loop: the third identical call is denied with the doom message; the hard
  stop gives `DoomStop` with history replayable; grace fires exactly once, is
  in history after step N and in step N+1's request, and arrives as a
  `MessagesAppended` event.
- A dedup hit inside the layer chain is served through `Respond` after
  approval and guardrails, and never reaches the executor.
- Two parallel gated calls never show two approval prompts at once.
- Un-ignore the 07-tagged tests from 05.
- `PolicyLayer` and `GuardrailLayer` each have direct hook tests, plus one
  test that builds the real chain from `ConversationBuilder` and asserts the
  layer order.

## Acceptance

- One policy implementation serves TUI, ACP and headless (hardening/01 and
  hardening/02 stay satisfied).
- Model-visible denial and guardrail texts are byte-identical to before.
- No `BeforeExecute`/`AfterExecute` remains.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note:
   - the approval serialization;
   - the layer order and where dedup sits in it;
   - the test-inventory diff;
   - the indirection rows closed.

## Completion note

## Completion note

**2026-10-31 · prompt 07 complete.** `cargo test -p craft`: 1906 passed,
3 failed (the three recipe-render fixed points on main), 24 ignored
(down from 29: every `restored in 07` tag cleared — `identical_calls_
blocked_as_doom_loop`, `children_feed_guardrail_counters`,
`guardrails_warn_then_block_repeated_failures`, `doom_loop_scores_
reach_grace_then_summarize`, `doom_score_hard_stops_the_run`; the rest
are `restored in 08/09` plus one env-dependent argosy test). fmt clean;
clippy at exact parity with the pre-07 baseline (diff of the warning
multiset is empty). Inventory 1923 → 1933 listed tests (27 removed, 37
added; every removal mapped below).

### The approval serialization

`ApprovalResponder` (`run/approval.rs`) is the one interaction seam; the
TUI and ACP implementations hold a `tokio::sync::Mutex` across the whole
ask (the TUI overlay is one modal with one pending slot; ACP clients
take one `session/request_permission` at a time), so two concurrently
gated calls from a parallel wave queue instead of clobbering each other's
parked oneshot — pinned by `two_parallel_asks_never_show_two_prompts`.
Headless fail-closes instantly, so it never queues. The pre-07 TUI gate
could clobber its `pending_approval` slot under the same concurrency;
that latent bug dies with the seam.

### The layer order and where dedup sits

`PolicyLayer` (rules → auto-review → responder → plan gate) →
`GuardrailLayer` (hard stop → doom repeat → guardrail warn/block → dedup)
→ request → snapshot → persist, assembled once in `ConversationBuilder`.
Dedup sits at the guardrail layer's tail, after the doom and guardrail
checks, exactly where 05's interim layer sat relative to policy: a
rules-allowed, guardrail-clean repeat call answers through
`ToolDecision::Respond` and never reaches the executor. The plan gate
runs whether or not the conversation wires the approval seam (plan mode
is conversation state, not an approval choice). Order pins:
`policy_denials_precede_the_doom_window` (a rules denial never feeds the
doom window), `plan_mode_blocks_a_responder_allowed_write`,
`guardrail_block_survives_an_allowed_call`,
`the_assembled_chain_orders_policy_guardrail_and_dedup`.

### Behaviour changes (each pinned, per the test-integrity rules)

- **Doom hard stop now appends the END marker** and reports
  `TurnOutcome::DoomStop` / `DoneReason::DoomStop` (the old loop's
  `commit_partial` added no marker). The stop reason carries the
  `doom-stop:` tag; every other layer stop keeps 05's failed-run
  mapping. `doom_hard_stop_ends_the_run_with_an_end_marker`.
- **Grace is a `step_end` append**, not a mid-run push: a durable
  `craft.marker = grace` observation, surfaced once as
  `StagnationDetected { similarity: score/25 }` plus
  `MessagesAppended`; `continue_run` only when the step was terminal
  `Completed`. `grace_prompt_fires_once_and_rides_the_next_request` and
  the restored `doom_loop_scores_reach_grace_then_summarize` (which now
  also pins `similarity == 17/25`, the executable form of the old
  score-math comment).
- **Batch children now hit the doom window and the guardrails' full
  chain** (they ride the agent pipeline since 06); the restored
  `children_feed_guardrail_counters` alternates identical-*result* reads
  so the no-progress counter, not the doom window, speaks.
- **Unknown tool names now ask before they error**: the runtime resolves
  unknown tools at execution, so the policy chain sees them (the old
  dispatch errored before any gate). Pinned by
  `an_unknown_tool_name_still_asks_before_it_errors`.
- **A report-less terminal `Done` carries the run's tallied
  usage/steps** (from `StepFinished` events) instead of zeros — a doom
  stop made real billed calls. `by_model` stays empty (events carry no
  model identity) so `cost` is `None` rather than misattributed; the
  report-bearing path is unchanged. Pinned in
  `doom_hard_stop_ends_the_run_with_an_end_marker`.

### Parity notes

- Model-visible texts byte-identical: permission denials (non-error
  `Respond`, is_error=false as before), plan gate and guardrail/doom
  denials (error `Deny`), `GUARDRAIL_BLOCK_MESSAGE`, `DOOM_LOOP_MESSAGE`,
  `GRACE_CALL_PROMPT`, the `[guardrail] ` prefix, the pre-warned
  fallback ("{tool} is repeating unproductively; consider a different
  approach or tool") and both `Event::Info` wordings ("guardrail blocked
  {tool}: …", "guardrail warning on {tool}: …").
- Doom accounting matches the old wave commit exactly: executed
  error/success ±1, guardrail blocks and plan denials +1, permission
  denials −1 (skips counted as successes), doom blocks exactly +15
  (`doom_blocked` — the old loop never routed them through
  `commit_wave`'s error count). Executable pins:
  `permission_denials_decay_the_doom_score` (similarity 0.6, not 0.68)
  and the no-grace assertion in `identical_calls_blocked_as_doom_loop`.
  Depends on animist's contract that `post_tool_exec` sees denied and
  responded results (docs/agents.md, Layers).
- State reset reading: `GuardrailState::reset_for_turn` (turn_start)
  resets the doom score and grace flag but keeps the recent-call window,
  the guardrail counters and the dedup cache (conversation state, as
  before); `reset()` (08's compaction handle) clears everything. The
  old `reset_for_new_user_input` call sites are gone — the layer owns
  the reset now.

### Test-inventory diff (1923 → 1933)

Moved to `harness::policy::tests` (the policy they test moved into the
layer): the eight headless gate decide-level tests and the eleven
rules-level TUI gate tests, each re-driven end-to-end through a
conversation — `read_only_tools_pass_the_gate` → `read_only_tools_run_
without_asking`, `deny_rules_block_bash_mutations_and_mcp_keys` →
`deny_rules_block_mutations_and_mcp_keys`, the headless guidance/
destructive-hint/yolo/auto-review tests under the same names,
`pre_decided_calls_never_reach_the_reviewer` (same name),
`apply_patch_scope_covers_every_patched_file` → `apply_patch_scopes_
cover_every_patched_file`, `task_runs_without_approval` +
`argosy_tools_run_without_approval` → `task_and_argosy_tools_run_
without_asking`, `argosy_code_tools_require_approval` →
`argosy_code_tools_still_ask`, `approval_grants_the_session_scope` →
`an_allow_session_answer_covers_the_sibling_call`,
`allow_once_grants_nothing_persistent` → `an_allow_once_answer_grants_
nothing_persistent`, `mcp_wire_tool_prompts_and_grants_as_mcp_key` →
`an_mcp_wire_tool_ask_grants_as_an_mcp_key`,
`unknown_and_mutating_tools_require_approval` →
`an_unknown_tool_name_still_asks_before_it_errors` (mutating half:
`an_ask_parks_and_the_answer_allows`). TUI interaction tests rewritten
against the responder: `a_rejection_carries_the_denial_message_and_
remember_scope` (was `deny_with_guidance_reaches_the_model`),
`decide_ignores_stale_ids_and_delivers_current_one` (same name),
`a_cancelled_turn_stops_the_parked_ask`,
`two_parallel_asks_never_show_two_prompts`; the auto-review overlay
lines are pinned at the renderer (`auto_review_events_render_as_review_
lines`). Renames: `approval_gate_reaches_batch_children` →
`policy_reaches_batch_children`, `skip_decision_reports_reason_to_
model` → `responder_denial_reports_reason_to_model`, `approval_gate_governs_
child_tool_calls` → `approval_policy_governs_child_tool_calls` (now a
deny-rule pin, the faithful shape of "children cannot bypass
approvals"). Added beyond replacements: the GuardrailState unit tests
(3), the order/doom/grace/guardrail-text pins listed above, and the
`Probe` helpers moved to `run/tests/mod.rs` so turns.rs could share
them. 5 ignored tests restored (see above); 24 remain, all tagged for
08/09 or env-dependent.

### Indirection rows closed

`run::{BeforeExecute, Decision}` (the approval seam with three
duplicate gates) · `SessionState.{dedup, guardrails}` handles →
`run::GuardrailState` behind one `Arc`, shared by both layers and the
conversation's typed handle · `run::dedup::{SharedDedupCache,
shared_cache, shared_cache_with_root}` and `run::guardrails::
{SharedGuardrails, shared_guardrails}` deleted · `CompactionState::
{with_dedup, with_guardrails}` → `with_guardrail_state` (08 keeps one
reset call). Surviving seams, on purpose: `ApprovalResponder` (three
real implementations plus the test doubles), `Reviewer` (auto-review's
injected model call).

### Review

Two review-agent passes over the full diff. First pass (request_changes,
P1×2 P2×4 P3×3): the three still-ignored 07 tests in turns.rs (adapted
to exclusive-claim probes for deterministic doom-window ordering under
concurrency; the doom denial asserted as a block of the squashed results
entry; distinct call ids because replay validation rejects duplicates),
the apply_patch scope pin restored, the doom-block ±1 fixed
(`doom_blocked`), report-less `Done` usage tallied from events, the
unknown-tool/auto-review-ordering/fallback-wording pins added, the
single chain-order test, the similarity pin, an engine doc nit. Second
pass: **approve_with_nits** (P3×4 — the missing similarity pin it
looked for was in the tree, the other three nits are the executable
pins now landing in this final state).
