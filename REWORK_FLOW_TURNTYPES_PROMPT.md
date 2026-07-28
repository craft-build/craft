# Prompt: Rework Flow mode to fold the turn-typed loop into the normal agent loop

You are picking up a half-finished refactor. The previous work *looked* done (clippy clean, 1360 tests pass) but the architecture is wrong: it reimplemented the old fixed `craft-flow` pipeline inside `craft-agent` and called it Phase 2. The user caught it. Your job is to fix the architecture, not to add more tests on top of the broken shape.

Read this whole document before touching anything. It is long because it carries every decision and every symbol reference you need, so you don't have to re-derive them. Trim detail at your own peril.

---

## 1. What is actually wrong right now

`craft-agent/src/agent/flow_loop.rs::run` (line ~357, ~1960-line file) is a **hardcoded global orchestrator**. It calls, in this exact order:

```
General → Scout → TPM (approval gate) → Plan → spawn chunks (per chunk: Req → Execute → Review↔Execute → QA↔Execute → Report) → Integrator → Verifier → exit
```

Every step is `mgr.advance(&root, TurnType::Scout)` etc. with the type literal baked in. The turn-type machinery that was supposed to *replace* this orchestrator is decorative:

- `craft-agent/src/agent/turn_type.rs::TurnType::spec` (line 90) builds `TurnTypeSpec { read, write, tools, role, transitions: Vec<TransitionRule> }` for all 11 types. The `transitions` field is populated. It is **never consulted** by `flow_loop::run`.
- `craft-agent/src/agent/transitions.rs::resolve` (line 76) implements the precedence rule (Advisor > objective gate > self-report) and returns `ResolvedTransition::{Accepted, Blocked, Illegal}`. It is **never called** by `flow_loop::run`. The only callers are the unit tests in `transitions.rs`.
- `TurnProposal::self_report` (transitions.rs line 42) is constructed nowhere outside tests.
- The `Gate::Objective(GateFn)` gates on each `TransitionRule` — `compile`, `test`, `drift` — are wired into a `GateSet` but `flow_loop::run` calls `GateSet::cargo()` defaults and never runs a gate against a proposal. Objective gates are enforced inline in `run_chunk` instead (e.g. `if last_qa_failed { return Err(...) }` at flow_loop.rs ~line 824/1098), which is the *opposite* of the design: the gate is a property of the transition, not a hand-coded branch.

The design doc (referenced in the plan at `~/.local/state/craft/plans/skilled-stirred-terrier.md`, section "Target architecture" → "Turn types") and the plan's Phase 2 acceptance criteria both say:

> "driven only by local transition declarations, with no global orchestrator."

We have a global orchestrator. Phase 2 was not actually completed; it was faked.

---

## 2. The target architecture (what you are building)

Flow mode should behave like Build mode: the agent starts as `General` and runs the **normal** `Agent::run_loop` (`craft-agent/src/agent/run.rs` line 387). The only differences from Build are:

1. The session carries a mutable `turn_type` (already a field on `Agent`, run.rs line 107) and the typed-log / thread-tree / advisor machinery (already built in `typed_log.rs`, `threads.rs`, `advisor.rs`).
2. At turn boundaries, the agent may **shift** its `turn_type` based on what the task demands, via a dedicated `shift` tool whose output is parsed at the turn boundary and run through `transitions::resolve`.
3. Subtasks (the existing `task` tool, `craft-agent/src/tools/task.rs`) work the same way: a spawned subagent starts `General` and can shift. (Subtasks shifting is a smaller, follow-on change; do the root loop first.)

The pipeline *shape* (Scout → Plan → chunks → Integrator → Verifier) **emerges** when the model chooses to shift into those types, not because a driver forced the order. If the model never shifts, Flow mode just runs as General — which is correct, because some requests don't need a pipeline.

### What gets deleted

- `flow_loop::run` (the orchestrator) — the entire function body.
- `flow_loop::run_chunk_dag` and `flow_loop::run_chunk` — the per-chunk pipeline and its JoinSet scheduler.
- `flow_loop::SubagentTurnRunner`, `flow_loop::DeterministicRunner`, the `TurnRunner` trait — these exist only to feed the orchestrator. The normal agent loop uses the real provider via `tool_context`; there is no "turn runner" seam.
- `flow_loop::FlowLoopParams` — only `project_id`, `workstream_id`, `store`, `approval`, `resume`, `progress`, `advisor`, `cancel` are meaningful, and most of those move to `Agent`/`AgentInput`/`AgentParams`. The rest (`runner`, `gates`, `request`) go away.
- The hardcoded `mgr.advance(...)` calls and the inline gate checks in `run_chunk`.

### What stays (move to the right home)

- `FlowProgress` enum (flow_loop.rs ~line 73) — stays, but is emitted from `run_loop` / `turn`, not from an orchestrator. Add `FlowProgress::TurnTypeEntered` emission when a shift is accepted.
- `FlowOutcome` (flow_loop.rs ~line 113) — stays as the terminal result type for the Flow entry point.
- `ApprovalPayload`, `FLOW_APPROVE_ANSWER`, `FLOW_CANCEL_ANSWER` (flow_loop.rs lines 32-42) — stay; the approval gate becomes an ordinary turn boundary where the agent pauses with `StopReason::AwaitingGoalApproval` (already exists in `craft-providers/src/types.rs` line 278).
- `FlowAdvisor` trait, `ForcedTransition`, `NoopFlowAdvisor`, `record_advisor_note` (flow_loop.rs ~lines 178-227, 362-380) — stay; the advisor fires at turn boundaries inside `run_loop` instead of inside the orchestrator. The precedence rule in `transitions::resolve` is the *only* path through which an accepted shift becomes the next type, so the Advisor's forced transition is expressed as an `advisor_override` argument to `resolve` (already the API: `resolve(rules, proposal, advisor_override)`).
- `ThreadManager`, `Thread`, `ThreadSnapshot`, `ThreadId` — stay; the root thread is created when Flow mode starts and child threads are created by the model via the existing `task` tool (or a thin `spawn_thread` tool if the design needs explicit thread creation — see §6).
- `ThreadHistory`, `EntryType`, projections, persistence, resume — stay; the typed log is appended at turn boundaries (see §4).
- `turn_type.rs::TurnType::spec`, `TransitionRule`, `Gate`, `GateSet` — stay and become **actually used**: the `shift` path consults `spec().transitions` and `resolve`.
- `transitions::resolve` — stays and becomes the single decision point for every shift.

---

## 3. The chosen shift mechanism (do not do the other options)

A dedicated tool named **`shift`**. Its `ToolOutput` is a **new variant**:

```rust
// craft-agent/src/types.rs, add to `pub enum ToolOutput` (line 140)
ShiftTurnType {
    target: TurnType,        // the requested next type
    rationale: String,       // free-text, recorded in the typed log
    // Optionally: thread_action: ThreadAction, if we want the model to propose Advance/Exit/Spawn.
    // Start without it; the transition rules' own `action` is the source of truth.
}
```

The tool itself does **not** mutate `Agent` state. It just returns the structured value. The shift is applied at the **turn boundary**, after `process_tool_calls` returns, in `Agent::turn` or `Agent::run_loop`. This is the same place `run_advisor` (run.rs line 706), `doom` checks, and the goal judge already run, so it is a known safe seam.

**Why this design (the user's reasoning, recorded so you don't relitigate it):**
- Tool calls always happen at the end of a turn anyway, so a tool that returns a sentinel `ToolOutput` variant puts the shift request in an easy-to-parse location.
- The tool's return value is the parse; there is no second self-report-parsing mechanism. One mechanism, not two.
- "Last shift wins" falls out for free: the last `shift` tool call in the batch is the one applied.
- The shift is observable in the transcript as a real tool call, which is what the user wanted from the dedicated-tool option.
- The tool handler is trivial (no `&mut Agent`), so it composes with worktree isolation, subagents, and tests exactly like every other tool.

### The tool signature

```rust
// craft-agent/src/tools/shift.rs (new file)
#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Shift {
    #[param(description = "The turn type to shift into: scout, tpm, plan, req, execute, review, qa, report, integrator, verifier, general")]
    target: String,                    // parse to TurnType via TurnType::parse
    #[param(description = "Why this shift is warranted (one or two sentences). Recorded in the typed log.")]
    rationale: String,
}
```
- `TurnType::parse` already exists (`turn_type.rs` ~line 630 region — check; if not, add it, it's a round-trip of `as_str()`).
- The tool's `execute` validates the string parses to a `TurnType` and returns `Ok(ToolOutput::ShiftTurnType { target, rationale })`. On a bad string it returns `Err("unknown turn type '{s}'")`.
- Register it in `register_tools!` (tools/mod.rs line 686). Audience: `ToolAudience::MAIN` only (not subagents, for the first cut). `kind = "switch_mode"` (matches the existing `switch_mode` tool-kind bucket used by `switch_mode` if any; otherwise `"think"`).
- The tool is only available in Flow mode. Gate it the same way `flow_search` is gated (feature-flagged native tools, tools/mod.rs ~line 724, or via the audience/dynamic context). Prefer: include it in `build_active_tools` only when `ctx.mode` is `AgentMode::Flow`.

### Applying the shift at the turn boundary

In `Agent::turn` (run.rs line 452) or in `run_loop` after `self.turn().await?` (run.rs line 432), after the existing `process_tool_calls` path, drain the last `ShiftTurnType` from the just-completed turn's tool outputs and run it through `resolve`:

```rust
// Pseudocode for the boundary logic.
// `self` is &mut Agent. `last_shift` is Option<ShiftTurnType> extracted from
// the tool results submitted this turn (see §5 for where to plumb it).
let rules = self.turn_type.spec(&self.gates).transitions;
let proposal = TurnProposal::self_report(last_shift.target, ThreadAction::Advance, last_shift.rationale);
let advisor_override = self.run_flow_advisor().await;  // Option<TurnProposal>, forced target = General
match resolve(&rules, &proposal, advisor_override.as_ref()) {
    ResolvedTransition::Accepted { target, action } => {
        self.commit_turn_write(self.turn_type, target);  // append distilled entry to ThreadHistory
        self.advance_turn_type(target, action);          // mutate self.turn_type, emit FlowProgress::TurnTypeEntered
    }
    ResolvedTransition::Blocked { reason } => {
        // stay in current type; push a user message explaining the block so the model can react
        self.history.push(Message::user(format!("Shift to {} blocked: {reason}", last_shift.target.as_str())));
        // do NOT change turn_type
    }
    ResolvedTransition::Illegal { proposed } => {
        // the model proposed a transition not in this type's declared set
        self.history.push(Message::user(format!("Illegal shift from {} to {}; staying.", self.turn_type.as_str(), proposed.as_str())));
    }
}
```

Key points:
- `resolve` is the **single** decision point. No inline gate checks. No hardcoded sequence.
- A `Blocked` result does not end the run; the agent stays in the current type and the model can try again (or shift somewhere else legal). This is the objective-gate behavior the design wants: "a turn doesn't get to certify its own transition unchecked" (plan, "Precedence rule").
- An `Illegal` result is a soft error for the model, not a panic. The model proposed a transition its current type doesn't declare; tell it and continue.
- The Advisor's forced transition is `advisor_override` to `resolve`. When present, `resolve` returns `Accepted { target: General, action: forced.action }` regardless of the rules. The forced target is always `General` (plan, locked user decision). This is already how `resolve` works (transitions.rs lines 81-86).
- If no `shift` tool call was made this turn, `proposal` is `None`, `resolve` is not called, `turn_type` is unchanged. The agent just continues as the same type. This is the common case and must remain cheap.

---

## 4. Where the typed log gets appended

Today `flow_loop::run` appends to `ThreadHistory` after every turn (e.g. `hist.append(root, EntryType::CodebaseContext, &o)` at flow_loop.rs ~line 365). In the new design, appends happen at the turn boundary in `Agent`, not in an orchestrator.

The mapping is `TurnType::spec().write.entry` (the `WritePolicy`'s `EntryType`). After a turn completes and before a shift is applied, commit the turn's distilled write:

```rust
// in Agent, at the turn boundary
let entry_type = self.turn_type.spec(&self.gates).write.entry;
let content = distill(self.turn_type, &self.history);  // see §4a
self.thread_history.append(self.thread_id.clone(), entry_type, &content);
```

The `thread_id` is the root thread (the workstream id) for top-level turns, or the child thread id when running inside a spawned subagent's own loop.

### 4a. Distillation

The design says narrow types commit "one distilled typed entry (e.g. a `diff`, not every edit tried)" and General "writes itself back verbatim". For the first cut:
- `TurnType::General` → `EntryType::GeneralTurn`, content = the assistant's final text for the turn (verbatim).
- Narrow types → their `WritePolicy.entry`, content = the assistant's final text for the turn (which for structured types is the JSON document the model produced).

A real distillation function (extract just the diff, dedupe retry attempts, etc.) is a later refinement. Do not build it now. Verbatim final-text is fine and matches what `flow_loop::run` does today (`hist.append(root, EntryType::CodebaseContext, &o)` where `o` is the subagent's raw output).

---

## 5. Where `last_shift` comes from (the plumbing)

`process_tool_calls` (`craft-agent/src/agent/tool_dispatch.rs`, called from `Agent::process_tool_calls` at run.rs line 884) returns a `ToolBatchOutcome`. Today it does not surface structured tool outputs to the caller beyond what gets pushed into `self.history`. You need the last `ToolOutput::ShiftTurnType` from this turn's batch.

Two options, pick the simpler:

**Option A (preferred): scan `self.history` at the boundary.** After `self.turn().await?` returns `TurnOutcome::Continue`, look at the most recent assistant turn's tool-use blocks in `self.history`. For each `ToolResult` whose matching `ToolUse` was `shift`, deserialize the result text back to the `ShiftTurnType` shape (the tool's output is structured JSON, see §5a). Take the last one. This needs no changes to `tool_dispatch.rs` and no new state on `Agent`.

**Option B: thread a side-channel through `ToolBatchOutcome`.** Add a field to `ToolBatchOutcome` (e.g. `last_shift: Option<ShiftTurnType>`) that `tool_dispatch` fills when it sees a `ToolOutput::ShiftTurnType`. Cleaner separation but touches the dispatch path.

Use Option A unless it proves awkward. The boundary logic should be a small helper on `Agent`, e.g. `fn last_shift_request(&self) -> Option<ShiftTurnType>`.

### 5a. `ToolOutput::ShiftTurnType` serialization

`ToolOutput` already derives `Serialize`/`Deserialize` (it's the type sent to the UI and persisted in session history). The `shift` tool returns `ToolOutput::ShiftTurnType { target, rationale }`. When `tool_dispatch` renders this to text for the transcript (the `ToolResult.content` string the model sees), render it as a compact JSON object: `{"shift":{"target":"scout","rationale":"need codebase map"}}`. The boundary helper parses that JSON back. Keep the wire shape stable and add a round-trip test.

---

## 6. Threads, spawning, and the `task` tool

The thread tree (`ThreadManager`) currently gets children from `flow_loop::run`'s Plan-parsing (`mgr.spawn(...)` at flow_loop.rs ~line 445). In the new design, threads are created when the model spawns a subtask. The existing `task` tool (`tools/task.rs`) already spawns a child agent; it just doesn't register a `Thread` in `ThreadManager` or persist a typed log for the child.

For the first cut:
- The root thread is the workstream. Created when Flow mode starts.
- When the model calls `task` with `subagent_type: "general"` and we're in Flow mode, register a child `Thread` in `ThreadManager` (parent = root) before spawning. The child's `turn_type` starts at `General`.
- The child subagent runs its own `run_loop` with the same shift machinery. When it shifts, it shifts within its own thread; its typed-log appends go to its own `ThreadId`.
- When the child's run ends (`Done`/`Error`), mark the `Thread` `Done` in `ThreadManager` and emit `FlowProgress::ThreadExit`.

A separate `spawn_thread` tool (explicit thread creation with declared `depends_on`) is in the design but is **out of scope** for this rework. The `task` tool covering it is enough for the first cut; the design's dependency-ordered scheduler can be added later if real concurrency matters. Do not build the JoinSet scheduler again.

---

## 7. The approval gate (TPM → Plan)

Today this is `flow_loop::run` returning `FlowOutcome::AwaitingGoalApproval` (flow_loop.rs ~line 410) and the TUI/CLI/ACP re-prompting. In the new design the gate is an **ordinary turn boundary** (plan, locked: "the approval gate is an ordinary turn boundary"). When the agent shifts into `TurnType::Tpm`, runs the turn, and the turn's write is a goal doc, the agent emits `AgentEvent::Done { stop_reason: Some(StopReason::AwaitingGoalApproval) }` and the run ends. The host (TUI/CLI/ACP) re-prompts; the next prompt carries the approval and the agent resumes, shifting into `Plan`.

Concretely:
- `run_loop` ends the run when it sees `StopReason::AwaitingGoalApproval` (this is already how the TUI's `do_flow_run` loop works — `craft-ui/src/agent/agent_loop.rs` ~line 402).
- The approval payload (`ApprovalPayload::Approved` / `Revised`) is passed back in via `AgentInput` (extend `AgentInput` if it doesn't already carry it; the TUI already threads it through `answer_rx`).
- On resume, the agent shifts to `Plan` itself (the model calls `shift` after seeing the approval), OR the resume path seeds `turn_type = Plan` directly. Pick one and document it. Prefer: the model shifts; the resume path just re-enters the loop at `General` and the model re-derives the shift from the persisted goal. This keeps the loop uniform.

---

## 8. Resume

Today `flow_loop::run` has `replay()` (flow_loop.rs ~line 342) that checks the persisted projection and skips re-deriving a turn if its `EntryType` already exists. In the new design, resume is simpler: when Flow mode starts, open the `ThreadHistory` for the workstream and let the agent read its own past entries via `history.query` (the `flow_search` tool, already wired). The agent decides what to do next. No `replay()` skip logic is needed because there is no orchestrator re-running stages. The typed log is the source of truth; the agent consults it.

Remove `replay()` and the skip-on-resume guards added to `flow_loop::run` (they were added to work around the orchestrator re-running stages on the ACP approval resume). They are not needed once the orchestrator is gone.

The per-chunk resume skip (the `if projection(EntryType::Report).is_some() return Completed` guard added to `run_chunk`) also goes away with `run_chunk`.

---

## 9. What the entry points become

### CLI `craft flow` (`src/cmd/subcmd/flow.rs`)
Today it builds `FlowLoopParams` and calls `flow_loop::run`. After the rework it builds an `Agent` in `AgentMode::Flow(workstream_id)`, attaches a `ThreadHistory` + `ThreadManager` + `FlowAdvisor`, and calls `agent.run(input).await` (the normal path). The `FlowOutcome` is derived from the terminal `AgentEvent::Done` stop reason:
- `StopReason::EndTurn` + Flow completed → `FlowOutcome::Done`
- `StopReason::AwaitingGoalApproval` → `FlowOutcome::AwaitingGoalApproval`
- `StopReason::Cancelled` → `FlowOutcome::Cancelled`
- `AgentEvent::Error` → `FlowOutcome::Failed`

### TUI `do_flow_run` (`craft-ui/src/agent/agent_loop.rs` ~line 334)
Today it loops calling `flow_loop::run` and translating `FlowOutcome`. After the rework it calls `agent.run(input)` once per prompt and translates the `AgentEvent` stream (the `flow_progress_tx` channel still carries `FlowProgress` events emitted from inside `run_loop`). The approval-gate loop stays (TUI re-prompts on `AwaitingGoalApproval`).

### ACP `headless::spawn_interactive` (`craft-agent/src/headless.rs` ~line 417)
Today it has an inline Flow branch that builds `FlowLoopParams` and calls `flow_loop::run`. After the rework the Flow branch goes away — Flow mode is just `AgentMode::Flow` set on the session, and the normal `agent.run(input)` path handles it. The `AgentEvent::FlowProgress` forwarding (the spawned forwarder task) stays, but it forwards events emitted from `run_loop`/`turn`, not from an orchestrator.

### `headless::spawn_interactive` Flow branch
Delete the entire `if let AgentMode::Flow(workstream_id) = &input.mode { ... }` block (headless.rs ~lines 417-540). Flow mode is handled by the normal `Agent::new(...).run(input)` path at headless.rs ~line 473. The only Flow-specific setup is: when `input.mode` is `Flow`, construct the `ThreadHistory`/`ThreadManager`/`FlowAdvisor` and pass them into `AgentParams` (extend `AgentParams` if needed). The `PendingFlow` struct and `pending_flow` state go away (the approval gate is now an ordinary `Done` with `AwaitingGoalApproval`, re-entered via the next prompt's `AgentInput`).

---

## 10. The `Agent` struct changes

`Agent` (run.rs line 92) needs these new fields (some already exist, check before adding):

- `thread_history: Option<Arc<std::sync::Mutex<ThreadHistory>>>` — `None` in Build/Plan, `Some` in Flow.
- `thread_manager: Option<Arc<std::sync::Mutex<ThreadManager>>>` — same.
- `flow_advisor: Option<Arc<dyn FlowAdvisor + Send + Sync>>` — `None` in Build/Plan and when the advisor is disabled; `Some(NoopFlowAdvisor)` otherwise. (Or just `Option<...>>` and treat `None` as no-op.)
- `gates: GateSet` — the objective gates. `GateSet::cargo()` by default; injectable for tests. Already referenced by `turn_type.spec(&gates)` (run.rs line 402 uses `GateSet::cargo()` inline — replace with the field).
- `thread_id: ThreadId` — the current thread (root or a child). Defaults to the root (= workstream id) when Flow mode starts.
- `flow_progress_tx: Option<flume::Sender<FlowProgress>>` — for emitting `FlowProgress` events. `None` in Build/Plan.
- `flow_cancel: Option<CancelToken>` — checked at turn boundaries. Already a pattern in the codebase.

The `turn_type` field (run.rs line 107) already exists. `AgentInput::mode` already carries `AgentMode::Flow(workstream_id)`.

`AgentParams` (run.rs ~line 50 region) and `AgentRunParams` (run.rs ~line 100 region) need to accept the Flow pieces. Keep them `Option` so Build/Plan are unaffected.

---

## 11. What the `run_loop` change looks like

`Agent::run_loop` (run.rs line 387) currently:
1. Checks doom.
2. Calls `self.turn()`.
3. On `Done` → runs advisor, emits done, returns.
4. On `Overflow` → auto-compact.
5. On `Continue` → loops.

The new `run_loop` inserts, between step 2 and step 3, the shift logic:
1. Checks doom.
2. **Checks `flow_cancel` → if cancelled, emit `FlowProgress::Cancelled` and return `Ok(())` with `StopReason::Cancelled`.** (Only in Flow mode.)
3. Calls `self.turn()`.
4. **If Flow mode and `TurnOutcome::Continue`: drain `last_shift_request()`, run `resolve` with `advisor_override`, apply the accepted shift (or push a block/illegal message and stay). Commit the turn's write to `ThreadHistory`. Emit `FlowProgress::TurnTypeEntered` on a successful shift.**
5. On `Done` → run advisor, emit done, return.
6. On `Overflow` → auto-compact.
7. On `Continue` → loop.

Steps 2 and 4 are Flow-mode-only no-ops in Build/Plan (guarded by `self.thread_history.is_some()` or a `matches!(self.mode, AgentMode::Flow(_))` check). This keeps Build/Plan byte-identical, which is the plan's invariant (Phase 1 acceptance criterion).

### Where exactly to put the shift logic

Prefer a helper called from `run_loop` rather than inlining 30 lines. Something like:

```rust
async fn apply_shift_if_requested(&mut self) -> Result<(), AgentError> {
    if !matches!(self.mode, AgentMode::Flow(_)) {
        return Ok(());
    }
    let Some(shift) = self.last_shift_request() else {
        return Ok(());
    };
    // ... resolve, commit write, advance turn_type ...
    Ok(())
}
```

Called from `run_loop` after `self.turn().await?` returns `TurnOutcome::Continue`, before the next loop iteration.

---

## 12. The Advisor in the new design

`FlowAdvisor::review` (flow_loop.rs ~line 199) currently takes `(history, tree, thread_id, turn_type)` and returns `Option<ForcedTransition>`. In the new design it's called from `run_loop` at the turn boundary, right before `resolve`:

```rust
let advisor_override = match &self.flow_advisor {
    Some(a) => a.review(Arc::clone(&self.thread_history), self.thread_manager_snapshot(), self.thread_id.clone(), self.turn_type).await,
    None => None,
};
// advisor_override is Option<ForcedTransition>; convert to Option<TurnProposal> with target=General
```

The forced target is always `General` (plan, locked). The `ForcedTransition` already carries `note` and `severity`; `record_advisor_note` (flow_loop.rs ~line 362) appends it to the parent thread's `ThreadHistory` as `EntryType::AdvisorNote`. Keep that.

The existing per-turn advisor (`advisor::review`, run.rs line 706, the `AdvisorNote` event) is a **separate, cheaper, always-on** gate (its docstring says so). Do not merge them. The `FlowAdvisor` is the between-turn tree watcher with override power; `advisor::review` is the delta reviewer. They coexist.

---

## 13. Tests to delete, keep, add

### Delete
- All `flow_loop.rs` tests that exercise the orchestrator's hardcoded sequence: `multi_chunk_run_produces_tree_shape_and_done`, `resume_reenters_and_completes`, `objective_gate_blocks_when_tests_fail`, `dependency_ordering_holds_c2_until_c1_done`, `advisor_forces_stale_child_exit_and_root_reenters`, `cancel_returns_cancelled_outcome`, `per_chunk_resume_skips_completed_chunks`. These test the broken shape.
- The `ScriptedRunner`, `ForceOnAdvisor`, `DeterministicRunner` test helpers in `flow_loop.rs` — they exist to feed the orchestrator.

### Keep
- `transitions.rs` tests (`self_report_proposal_is_accepted`, `objective_gate_pass_accepts`, `objective_gate_fail_blocks`, `proposal_outside_transition_set_is_illegal`, `advisor_override_takes_precedence_over_gate`, `advisor_override_takes_precedence_over_self_report`). These test the resolver, which is now actually used.
- `threads.rs` tests (`spawn_adds_child_under_parent`, `exit_marks_done_and_returns_parent`, `eligible_children_respects_depends_on`, etc.). The `ThreadManager` is kept.
- `typed_log.rs` tests (projections, persistence, resume).
- `turn_type/templates.rs` tests, plus the `general_stage_prompt_passes_input_verbatim` regression test that was just added (the empty-prompt bug).
- `flow_loop.rs` `parse_chunks`/`review_failed`/`qa_failed`/`verifier_passed`/`merge_ok`/`extract_json` helpers and their tests — these are still used (the model's structured outputs need parsing). Move them to a `flow_loop::helpers` or `turn_type::parsing` module if `flow_loop.rs` otherwise shrinks to nothing.

### Add
- `shift` tool: round-trip test (`ToolOutput::ShiftTurnType` serializes to the wire JSON and back), invalid-target test, not-available-in-Build-mode test.
- `Agent::apply_shift_if_requested`: tests for `Accepted` (turn_type changes, `FlowProgress::TurnTypeEntered` emitted, write committed to `ThreadHistory`), `Blocked` (turn_type unchanged, block message pushed to history), `Illegal` (turn_type unchanged, illegal message pushed), `advisor_override` (forced target `General` wins over a self-proposed `Plan`), no-shift-request (no-op, cheap).
- `Agent::run_loop` Flow-mode integration test: seed `AgentMode::Flow`, a stub `FlowAdvisor`, and a scripted provider that emits a `shift` tool call to `Scout`; assert the agent shifts, emits `FlowProgress::TurnTypeEntered { turn_type: Scout }`, and continues. This is the test that proves the architecture works end-to-end without an orchestrator.
- Regression test: Build mode with the same `Agent` unchanged produces byte-identical stop-reason/turn-count behavior (the plan's Phase 1 invariant). The existing `run.rs` tests (`turn_counting`, `interrupt_handling`, `nudge_*`, `try_auto_compact_*`, `do_compact_*`, `cancel_token_aborts_during_api_call`) are this bar; make sure they still pass.

---

## 14. Acceptance criteria for this rework

1. `flow_loop::run`, `run_chunk_dag`, `run_chunk`, `SubagentTurnRunner`, `DeterministicTurnRunner`, the `TurnRunner` trait, and `FlowLoopParams.runner`/`.gates`/`.request` are gone. `flow_loop.rs` is either deleted or reduced to the types that stayed (§2).
2. Flow mode runs through `Agent::run_loop`. There is no second loop. The only place `turn_type` changes is `Agent::apply_shift_if_requested` (or equivalent), and the only decision point is `transitions::resolve`.
3. `transitions::resolve` is called on every shift; `TurnType::spec().transitions` is the rule set; `Gate::Objective` gates actually run via `resolve`.
4. Build and Plan modes are byte-identical (existing `run.rs` tests pass unchanged; `flow_search`/`shift` tools are not registered in Build/Plan).
5. The CLI (`craft flow`), TUI (`do_flow_run`), and ACP (`spawn_interactive` Flow branch) all drive Flow mode through `agent.run(input)`, not through `flow_loop::run`. The ACP Flow branch in `headless.rs` is deleted.
6. A scripted Flow run that emits a `shift` to `Scout` shifts, emits `FlowProgress::TurnTypeEntered`, and continues. A scripted Flow run with no shift stays `General`. A scripted Flow run where the model shifts to a type not in the current type's `transitions` gets an `Illegal` message and stays. A scripted Flow run with a failing objective gate gets a `Blocked` message and stays.
7. clippy clean: `cargo clippy --all-features --all --tests -- -D warnings`. Tests pass: `cargo nextest run --all-features -p craft-agent -p craft-acp -p craft`. craft-ui compiles (its tests hang in the harness; compile-only is the standing bar).

---

## 15. Order of operations (suggested, not mandatory)

1. Add `ToolOutput::ShiftTurnType` and the `shift` tool. Wire it into `build_active_tools` for Flow mode only. Unit-test the tool in isolation.
2. Add the `Agent` Flow fields (`thread_history`, `thread_manager`, `flow_advisor`, `gates`, `thread_id`, `flow_progress_tx`, `flow_flow_cancel`). Plumb them through `AgentParams`/`AgentRunParams` as `Option`. Build/Plan pass `None`.
3. Write `Agent::apply_shift_if_requested` and `Agent::last_shift_request`. Unit-test these in isolation against a scripted history.
4. Wire `apply_shift_if_requested` into `run_loop` (the two new steps in §11). Make it a no-op in Build/Plan.
5. Wire the typed-log commit (`commit_turn_write`) and `FlowProgress::TurnTypeEntered` emission into the accepted-shift path.
6. Wire the `FlowAdvisor` call into the shift path (advisor_override to `resolve`).
7. Rework the CLI `craft flow` entry point to use `agent.run` and derive `FlowOutcome` from the terminal `AgentEvent::Done`.
8. Rework the TUI `do_flow_run` to call `agent.run` per prompt and translate `AgentEvent`s (the `flow_progress_tx` channel still works).
9. Delete the ACP `headless` Flow branch. Flow mode is just `AgentMode::Flow` on the session.
10. Delete `flow_loop::run`, `run_chunk_dag`, `run_chunk`, `SubagentTurnRunner`, `DeterministicRunner`, `TurnRunner`, `FlowLoopParams.runner/.gates/.request`, `replay()`, the per-chunk resume skip, and the orchestrator tests. Move the kept helpers (`parse_chunks`, `extract_json`, etc.) to their new home.
11. Add the `run_loop` Flow-mode integration test (scripted provider + scripted `shift` tool call).
12. Run clippy + tests; fix fallout. Run the existing `run.rs` regression tests to prove Build/Plan is unchanged.

---

## 16. Things to watch out for

- **`Agent::turn` is subtle.** The plan flagged it as the highest-risk refactor. You are inserting logic at the turn boundary, not rewriting `turn` itself. Keep `turn`'s body unchanged; put the shift logic in `run_loop` or a helper called from `run_loop`. Do not move doom/advisor/judge/compaction around.
- **The `turn_type` field on `Agent` is set in `Agent::run` (run.rs line 354) to `General` for all modes.** That stays. The shift is the only thing that changes it after that. Do not seed `Scout` or any other type at start.
- **`GateSet::cargo()` runs real `cargo` commands.** Tests must inject `GateSet { compile: always_pass(), test: always_pass(), drift: always_pass() }` (helpers already exist in `transitions.rs` lines 108-115). Never call `GateSet::cargo()` in a unit test.
- **The `shift` tool must not be available in Build/Plan.** If it is, the model in Build mode might call it and change `turn_type`, which would break Build's invariant. Gate registration on `AgentMode::Flow`.
- **`FlowProgress` derives `Serialize`/`Deserialize`** (added in the Phase 3 work) and `AdvisorSeverity` does too (added in Phase 3). Keep those derives; ACP serializes `FlowProgress` and the TUI matches on it exhaustively.
- **`StopReason::AwaitingGoalApproval` exists** in `craft-providers/src/types.rs` line 278 and `translate::map_stop_reason` (craft-acp/src/translate.rs line 196) already maps it to ACP `EndTurn`. Do not re-add it.
- **The `AgentEvent::FlowProgress` variant** (types.rs ~line 659) and the ACP/TUI handlers for it stay. They are fed from `run_loop` now instead of an orchestrator, but the wire shape is unchanged.
- **The plan's "two history notions"** (plan lines 27-55) still hold: `agent::History` (the chat transcript) and `ThreadHistory` (the typed log). Do not rename `History`. Do not fold one into the other.
- **Do not re-introduce a global orchestrator.** If you find yourself writing `match turn_type { General => ..., Scout => ..., ... }` in `run_loop`, stop. That's the orchestrator in another form. The shape comes from `TurnType::spec().transitions` + `resolve`, not from a match on the type.

---

## 17. Files you will touch (rough)

- `craft-agent/src/types.rs` — add `ToolOutput::ShiftTurnType`.
- `craft-agent/src/tools/shift.rs` — new file, the `shift` tool.
- `craft-agent/src/tools/mod.rs` — register `shift` in `register_tools!`; gate it for Flow mode in `build_active_tools`.
- `craft-agent/src/agent/run.rs` — `Agent` fields, `run_loop` shift steps, `apply_shift_if_requested`, `last_shift_request`, `commit_turn_write`, `run_flow_advisor`. `AgentParams`/`AgentRunParams` extensions.
- `craft-agent/src/agent/flow_loop.rs` — delete the orchestrator; keep `FlowProgress`, `FlowOutcome`, `ApprovalPayload`, `FLOW_*` constants, `FlowAdvisor`, `ForcedTransition`, `NoopFlowAdvisor`, `record_advisor_note`. Move `parse_chunks`/`extract_json`/`review_failed`/`qa_failed`/`verifier_passed`/`merge_ok` to a helpers module or keep in `flow_loop` if it still has a home.
- `craft-agent/src/agent/transitions.rs` — no change; now actually used.
- `craft-agent/src/agent/turn_type.rs` — no change; now actually used.
- `craft-agent/src/agent/threads.rs` — maybe a `spawn_from_task` helper for the `task`-tool integration; otherwise no change.
- `craft-agent/src/agent/typed_log.rs` — no change.
- `craft-agent/src/agent/advisor.rs` — no change (the cheap per-turn advisor is separate from `FlowAdvisor`).
- `craft-agent/src/headless.rs` — delete the Flow branch; Flow mode is `AgentMode::Flow` on the session.
- `src/cmd/subcmd/flow.rs` — build `Agent` in `AgentMode::Flow`, call `agent.run`, derive `FlowOutcome`.
- `craft-ui/src/agent/agent_loop.rs` — `do_flow_run` calls `agent.run` per prompt; `flow_progress_tx` stays.
- `craft-acp/src/server.rs` / `translate.rs` — no change (the `AgentEvent::FlowProgress` path stays).
- `craft-agent/src/lib.rs` — adjust re-exports if `flow_loop` types move.

---

## 18. Memory notes from the previous sessions

Two memory files exist with the state of the previous (broken) work: `flow-acp-rewire.md` and `flow-phase3-advisor.md`. Read them for context on what was built, but remember their "done" claims are about the broken orchestrator. The types they describe (`FlowAdvisor`, `ForcedTransition`, `EntryType::AdvisorNote`, `FlowProgress::AdvisorNote`, cancel wiring) are kept; the orchestrator they were wired into is deleted.

After the rework, update both memory files (or replace them with one new one) describing the correct architecture: Flow mode = normal `run_loop` + `shift` tool + `transitions::resolve` + typed log.

---

## 19. The one-paragraph summary for when you're done

Flow mode is Build mode with a mutable `turn_type` and a typed log. The agent starts `General` and runs the normal loop. At turn boundaries it drains the last `shift` tool call from the just-completed turn, runs it through `transitions::resolve` against the current type's declared `TransitionRule` set (with the Advisor's forced transition as the override), and shifts, blocks, or rejects. The typed log commits one distilled entry per turn. The pipeline shape — Scout, Plan, chunks, Integrator, Verifier — emerges from the model's shift choices, not from a driver. There is no `flow_loop::run`, no `TurnRunner`, no second loop. Build and Plan are unchanged.
