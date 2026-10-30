# 07 — Tool policy layers: approval, plan mode, guardrails, doom loop, auto-review

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt)
  and the [harness design rules](harness-design.md), especially
  [Layers](harness-design.md#layers). Start with the
  [test inventory](harness-design.md#test-integrity).
- When implementation is finished, **run the review agent**
  (`.craft/agents/reviewer.md`) as described in [Finish](#finish).

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
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note:
   - the approval serialization;
   - the layer order and where dedup sits in it;
   - the test-inventory diff;
   - the indirection rows closed.

## Completion note

_(fill in when done)_
