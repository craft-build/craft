# 10 — Subagents and `batch` on animist

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt).
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

1. **Subagents.** `SubagentLauncher` builds a child `Conversation` (06) with:
   - a fresh `History`;
   - the child model (tier or explicit);
   - the restricted tool set (with `task` removed to prevent recursion,
     matching today);
   - the parent's `PolicyLayer`/`GuardrailLayer` instances, or equivalents
     sharing the parent's permission gate (today: "the task tool spawns
     subagents under the turn's shared permission gate");
   - `RecoveryLayer`/`RequestLayer` configured for the child window.

   Run it with `harness::driver::run_turn`:
   - Cancellation is linked to the parent's `context.run.cancellation`: the
     child's `Cancellation` is cancelled when the parent's fires, via an
     awaited `select!` inside the tool, not a detached task.
   - Events are wrapped as `Event::Subagent { tool_use_id: context.call.id, … }`,
     with today's filtering.
   - Return the final assistant text, or the validated JSON for
     `output_schema`.
   - Always call `shutdown()` on the child agent, including on error and
     cancel.
   - Persist the child's history in the session's subagent histories (animist
     shapes from 04; 11 moves them to entries).
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
   - Delete the `SharedDispatch` back-reference that `Workspace::register`
     patches in after the table is built.
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

- `subagent.rs` no longer calls `run::run`. It uses `run_turn` on a child
  `Conversation`.
- No `SharedDispatch` remains.
- Hardening/04's isolation tests pass.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note: whether `batch` is kept, the measured overlap,
   and the limits chosen.

## Completion note

_(fill in when done)_
