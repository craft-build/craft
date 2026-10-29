# 05 — Agent runtime core: animist `Agent` runs every turn

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt),
  including the [async rules](README.md#async-native).
- When implementation is finished, **run the review agent**
  (`.craft/agents/reviewer.md`) as described in [Finish](#finish).

## Goal

Replace craft's hand-written step loop with `animist::agent::Agent`. This
prompt introduces:

- the `harness` module: Conversation, driver, event bridge, executor
  decorator, `RunScope`;
- the first layers: `RequestLayer`, plus dedup as a `ToolDecision::Respond`
  in a small `DedupLayer` that 07 folds into `GuardrailLayer`;
- a **temporary rig-tool bridge**, so today's tools run inside the animist
  registry until 06 ports them to native `ToolImpl`.

Keep `run::run`/`run_with_images` as a **facade** with today's signatures, so
the TUI, ACP, headless and subagent callers don't change yet (README decision 4).

## Read first

- animist 0.7.0 (`~/Projects/animist` at `v0.7.0`): read **all** of
  `docs/agents.md`, then `src/agent/{mod.rs, run.rs, layer/mod.rs, tool.rs,
  config.rs, prompt.rs, error.rs}`. Know these contracts:
  - one active run per agent;
  - the input must be a `User` message without tool blocks;
  - `prepare_request` may only remove tool declarations;
  - `validate_replay` runs before every attempt;
  - an `ExchangeCommit` conflicts when history changes mid-step;
  - placeholder results come before tool execution, and the agent squashes
    them to source + final revision when the step's calls all finish;
  - `Respond` completes a call without running the executor, but
    `post_tool_exec` and `ToolFinished` still see it;
  - executor panics become `ToolError::Panicked` error results, and the run
    continues;
  - await cancellation, don't drop the run;
  - bounded event channels apply backpressure.
- Craft:
  - `agent/src/run/{mod.rs, turns.rs, dispatch.rs, recency.rs, view.rs,
    read_lifecycle.rs, events.rs, cancel.rs, stats.rs, task_set.rs, dedup.rs}`
  - `agent/src/prompt.rs`, `agent/src/runtime.rs` (`run_policy`)
  - `agent/src/snapshot.rs`

## Work

1. **The `harness` module** (`agent/src/harness/`):
   - **`Conversation`**: owns the `History`, the current `ModelHandle`, the
     tool set, the layer handles and the `RunScope`. It builds an `Agent`, and
     rebuilds it on model or tool-set change on the **same** `History` handle.
     It owns `async fn shutdown()`; call it explicitly, and never rely on
     `Drop`.
   - **`RunScope`**: per-run state (event sender, `RunParams` snapshot, doom
     tracker and retry budget slots for 07/08) in a shared handle that layers
     read. Use `arc-swap` (already a dependency) or a short `std::sync::Mutex`
     never held across `.await`.
   - **`driver::run_turn(conversation, prompt, images, params, cancel, emit) ->
     RunOutcome`**:
     - builds the input `StoredMessage`;
     - creates a bounded `flume` channel and drains it concurrently into `emit`
       (`tokio::join!`, not a detached task);
     - maps craft `CancelToken` → `Cancellation::cancel()` with a
       `tokio::select!` that keeps awaiting the run afterwards (decision 9);
     - translates `RunReport`/`AgentError` into `RunOutcome`.
   - **Facade**: `run::run`/`run_with_images` build a throwaway `Conversation`
     from the caller's `Vec<Message>`, call `run_turn`, and write
     `history.current()` (plus marker tags) back. Document that 11 removes it.
2. **Status mapping.** Interim: 08 adds continuations and recovery.
   - `Completed` → `Done { reply }`.
   - `StepLimit` → `MaxTurns`, plus an `END_MARKER` observation added after the
     run.
   - `MaxTokens` → `MaxTokens { reply }`, with no continuation yet.
   - `Cancelled` → commit the interrupted attempt's validated text/thinking
     prefix (drop unexecuted `ToolUse` blocks), check it with `validate_replay`,
     then add the cancel marker.
   - `Stopped` → `Cancelled` or `DoomStop` (07 decides).
   - `Failed`, `Refused`, `Paused` and `ContextWindowExceeded` → `Failed(msg)`,
     with a `[Run failed]` observation (decision 11).
   - `AgentError::HistoryChanged` → `Failed`, with the retained messages logged.
3. **Event bridge** (`harness::events`). Map `AgentEvent` onto craft `Event`:
   - `Provider { TextDelta | ThinkingDelta }` → the matching deltas.
   - `ToolUseStart` → `ToolPending`.
   - `ToolStarted` → `ToolStart`.
   - `ToolProgress` → `ToolOutput` (full content so far).
   - `ToolFinished` → `ToolDone`.
   - `StepFinished` → `TurnComplete`.
   - `MessagesAppended` → render the appended observations by `craft.marker`
     tag (no producer until 07/08, but bridge it now).
   - The committed result message → `ToolResultsSubmitted`, once per step.
   - `AttemptInterrupted` → a new `Event::AttemptSuperseded { attempt }`.
   - Subagent tagging is unchanged.

   Use one bounded channel per run.
4. **The executor decorator** (`harness::tools::HarnessExecutor`). A
   `ToolExecutorImpl` that wraps any inner `ToolExecutor`, so it outlives the
   rig bridge:
   - does snapshot capture for `/undo`;
   - forwards `definition()` and `scheduling()` unchanged.

   It does **not** catch panics (animist's `ToolError::Panicked` covers that)
   and does not serve dedup (step 6b). If snapshot capture is all that's
   left, consider making it a `pre_tool_exec`/`post_tool_exec` pair instead,
   and drop the decorator.
5. **The temporary rig-tool bridge** (`harness::tools::rig_bridge`, deleted in
   06). Wrap each `rig_core::tool::DynamicTool` as a `ToolExecutorImpl`:
   - the definition comes from rig's `ToolDefinition`;
   - scheduling stays `Exclusive` for everything, keeping today's
     effectively-serial semantics until 06 adds claims;
   - the output becomes `ToolOutput`/`ToolContents`.

   `current_call_id`'s task-local is set from `ToolContext::call.id` around the
   call. MCP tools go through `register_executor` the same way. Keep the
   bridge small: it is throwaway.
6. **`RequestLayer`** (`prepare_request`). Ports the per-step request-view
   pipeline in today's order:
   - recency tail;
   - `read_lifecycle::apply_to_request`;
   - `compress_request_view`;
   - user-message coalescing, only if some animist adapter still needs it
     (check);
   - Chat Completions tool-result image relocation;
   - the max-output clamp (`clamped_max_tokens`) against the estimate vs the
     last measured input;
   - `RequestOptions` from 03's options builder;
   - publishing the request overhead into the compaction state.

   It must keep complete tool exchanges and never touch stored history. (09
   moves the between-run part of read-lifecycle/compression into
   `Alteration`s; this layer keeps only what must change within a run.)

6b. **`DedupLayer`**. `pre_tool_exec` looks the call up in the dedup cache and
   returns `ToolDecision::Respond { output }` on a hit, so the executor never
   runs. `post_tool_exec` records executed results into the cache. Keep
   today's order relative to approval and guardrails (dedup runs after them);
   07 merges this into `GuardrailLayer`. A responded call emits `ToolFinished`
   without `ToolStarted`, so the event bridge must still produce the
   `ToolStart`/`ToolDone` pair the surfaces expect.
7. **Prompt and config.**
   - Build `animist::agent::Prompt` from craft's preamble, and keep the system
     bytes identical: either one section whose `to_markdown` matches, or set
     `StepRequest::system` in `RequestLayer`. Pin it with a test.
   - `AgentConfig` follows README decision 12, with `max_retries = 0` until 08.
     `max_concurrent_tools` may already be >1: the bridge's tools are all
     `Exclusive`.
8. **Interim hooks.** Until 07, adapt the existing `BeforeExecute`/`AfterExecute`
   objects with a thin layer (`pre_tool_exec` → `decide`, `post_tool_exec` →
   `transform`).
9. **Delete the dead loop code:** `run_loop`, the streaming half of
   `turns::stream_turn`, `stream.rs`, `task_set.rs`, the `dispatch_tool_calls`
   waves, and the parts of `ToolDispatch` that the registry replaces. Keep
   pure-logic modules (doom, guardrails, dedup, recency, read_lifecycle, view,
   retry classification).

## Tests

- Port `run/tests/{turns, dispatch, spec}.rs` to drive the facade with
  `ScriptedProvider`. Update expectations deliberately where animist semantics
  differ (placeholder results, call-order slots, cancel commits), and list each
  in the completion note.
- New tests:
  - a panicking tool yields animist's `Panicked` error result and the run
    continues (pins the model-visible text);
  - cancel mid-stream commits the partial text and the cancel marker, and
    replays cleanly into the next run;
  - cancel mid-tool leaves the interrupted placeholder;
  - the request view holds recency and read-lifecycle markers, while stored
    history does not;
  - the system prompt bytes are unchanged;
  - a dedup hit is served through `Respond` without executing the tool, and
    the surfaces still see a start/done pair.

## Known interim gaps (closed by 06/07/08)

- Tools are serial: all claims are `Exclusive` until 06.
- No retry, fallback, reauth, overflow recovery, `MaxTokens` continuation,
  nudge, grace or advisor. Tests for those stay `#[ignore = "restored in 08"]`; don't delete
  them.

## Acceptance

- Every surface works through the facade, and a scripted headless end-to-end
  test passes.
- The run loop no longer streams or dispatches tools itself.
- All non-ignored tests pass, and every ignored test names 07 or 08.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note: the behaviour changes with their tests, the
   ignored tests, and the system-prompt strategy.

## Completion note

_(fill in when done)_
