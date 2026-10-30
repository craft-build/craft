# 05 — Agent runtime core: surfaces own a `Conversation` on animist `Agent`

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt),
  including the [async rules](README.md#async-native), and the
  [harness design rules](harness-design.md), which win where the two
  disagree. Read the design rules' [Layers](harness-design.md#layers) section
  in full before writing any layer.
- Start with the [test inventory](harness-design.md#test-integrity).
- When implementation is finished, **run the review agent**
  (`.craft/agents/reviewer.md`) as described in [Finish](#finish).

## Goal

Replace craft's hand-written step loop with `animist::agent::Agent`, and give
every surface a long-lived `harness::Conversation`. **There is no
`run::run` facade.** A throwaway conversation per turn would drop entry
sources, revisions and inactive entries between turns, and break 08 and 11.

This prompt:

- introduces `harness/`: `Conversation`, `ConversationBuilder`, the driver,
  `harness::Event`, the conversation event sink, and the persister;
- adds the first layers: `GuardrailLayer` (dedup only for now),
  `RequestLayer`, `SnapshotLayer`, `PersistLayer`, plus a thin interim
  `PolicyLayer` over today's approval hooks (07 replaces its body);
- bridges today's tools into the registry with a **temporary rig-tool
  bridge** until 06;
- moves TUI, ACP, headless, print and the subagent launcher onto the
  conversation, and deletes the old loop;
- removes the indirection rows assigned to 05 in the
  [inventory](harness-design.md#no-indirection-without-a-job).

Work in three phases. **Each phase ends with a green commit.**

## Read first

- animist 0.7.0 (`~/Projects/animist` at `v0.7.0`): read **all** of
  `docs/agents.md` and `docs/history.md` (*Atomicity and concurrency*, the
  change feed), then `src/agent/{mod.rs, run.rs, layer/mod.rs, tool.rs,
  config.rs, prompt.rs, error.rs}` and `src/history/{mod.rs, changes.rs,
  stored.rs}`. Know these contracts:
  - one active run per agent; `RunContext::id` is zero-based per agent;
  - the input must be a `User` message without tool blocks;
  - `prepare_request` may only remove tool declarations;
  - `validate_replay` runs before every attempt;
  - an `ExchangeCommit` conflicts when history changes mid-step;
  - placeholder results come before tool execution, and the agent squashes
    them to source + final revision when the step's calls all finish
    (`EntryReplaced`);
  - `Respond` completes a call without running the executor, but
    `post_tool_exec` and `ToolFinished` still see it;
  - executor panics become `ToolError::Panicked` error results, and the run
    continues;
  - `step_end` runs after a step's results commit; `turn_end` runs for every
    admitted run;
  - await cancellation, don't drop the run;
  - bounded event channels apply backpressure;
  - `changes_since` returns owned data; `ChangesTooOld` past 1024 events.
- Craft:
  - `agent/src/run/{mod.rs, turns.rs, dispatch.rs, recency.rs, view.rs,
    read_lifecycle.rs, events.rs, cancel.rs, stats.rs, task_set.rs, dedup.rs}`
  - `agent/src/runtime.rs` (`run_policy`, `RunPolicyInputs`, `compaction_ctx`,
    `new_compaction_state`), `agent/src/model_selection.rs`
  - `agent/src/prompt.rs`, `agent/src/snapshot.rs`
  - Surfaces: `agent/src/tui/provider/live/{mod.rs, turn.rs, commands.rs,
    bootstrap.rs, resume.rs, approval.rs, usage_recorder.rs}`,
    `agent/src/acp/{mod.rs, turn.rs}`, `agent/src/headless.rs`
    (`SessionStore`, `HeadlessGate`), `agent/src/print.rs`, `agent/src/cli.rs`
    (`run_headless_query`), `agent/src/subagent.rs`
  - `agent/src/storage/sessions/{mod.rs, log.rs}` (epoch, rewrites, cursors)
  - `docs/hardening/01-*` (single policy source),
    `docs/hardening/09-continue-and-fork-semantics.md`

## Phase A: the harness, tested on its own

Surfaces still use the old loop at the end of this phase. The harness is
driven only by tests.

1. **`Conversation`** (`harness/conversation.rs`):
   - Owns the `History`, the current model (the type 02/03 landed), the tool
     registry inputs, the layer instances (with typed handles to their state;
     see [State ownership](harness-design.md#state-ownership)), the
     `ArcSwap<TurnSettings>`, and the event sink.
   - Builds its `Agent` with `Agent::builder().history(history.clone())`.
     Rebuilds it only when the model or tool set changes, between runs, on
     the same `History` and the same layer instances.
   - `async fn shutdown()` is explicit, idempotent and called on every exit
     path. Never rely on `Drop`.
   - **`ConversationBuilder`** replaces `RunParams`, `run_policy`'s bag
     output, `CompactionCtx`, `compaction_ctx` and `new_compaction_state`. It
     reads config once and produces `AgentConfig`, `Prompt`, layer configs
     and `TurnSettings`. `run_policy`'s values stay single-sourced
     (hardening/01): fold `run_policy` into the builder, or make it return the
     builder's inputs. Don't keep both a params bag and a builder.
2. **`harness::Event`**: `Agent(animist::agent::AgentEvent) |
   Craft(CraftEvent)`. `CraftEvent` holds only what animist can't express
   (see the inventory). Delete the `run::Event` variants that duplicate
   animist events (`TextDelta`, `ThinkingDelta`, `ToolPending`, `ToolStart`,
   `ToolOutput`, `ToolDone`, `TurnComplete`, `ToolResultsSubmitted`,
   `StreamClosed`) once phase C moves the surfaces.
   - Surfaces match animist events directly. `ToolUseStart` (via
     `ProviderEvent`), `ToolStarted`, `ToolProgress`, `ToolFinished`,
     `StepFinished`, `MessagesAppended` and `AttemptInterrupted` carry what
     the old variants did. Where a surface needs a derived value (a
     responded call never gets `ToolStarted`), derive it in the surface
     adapter with a test.
   - **The event sink** is conversation-scoped: one `flume` sender that
     surfaces subscribe to once. Layers send `CraftEvent`s into it. The
     driver forwards each run's animist channel into it. Follow the
     [ordering rule](harness-design.md#events-from-layers).
3. **Driver** (`harness/driver.rs`):
   `run_turn(&Conversation, input, cancel: Cancellation) -> TurnOutcome`.
   - Builds the input `StoredMessage`, runs `Agent::run` with a bounded
     channel drained concurrently into the sink (`tokio::join!`, not a
     detached task), and keeps awaiting the run after cancellation.
   - Status mapping (09 adds continuations and recovery):
     - `Completed` → done with the reply.
     - `StepLimit` → max turns, plus an `END_MARKER` observation.
     - `MaxTokens` → max tokens, no continuation yet.
     - `Cancelled` → commit the interrupted attempt's validated
       text/thinking prefix (drop unexecuted `ToolUse` blocks), check it with
       `validate_replay`, then add the cancel marker.
     - `Stopped` → cancelled or doom stop (07 decides).
     - `Failed`, `Refused`, `Paused` and `ContextWindowExceeded` → failed,
       with a `[Run failed]` observation (README decision 11).
     - `AgentError::HistoryChanged` → failed, with the retained messages
       logged.
   - Pre-turn compaction belongs here, but it is **off from 05 to 07**:
     today's engine rewrites a `Vec` in place, and writing that back into
     `History` would need the throwaway rewrite adapter this plan avoids. 08
     brings it back as `Compactor` plans called from this spot. Delete the
     surfaces' copies now, and leave a single `// 08: compaction` call site.
   - Nothing else. See [the driver's job](harness-design.md#pick-the-hook-by-what-the-behaviour-is).
4. **Cancellation.** Runs use `animist::agent::Cancellation`. Delete the
   `CancelToken`/`CancelFlag` epoch re-arm and `CancelTrigger` where
   `Cancellation` covers them. Keep `CancelMap` only if per-subagent cancel
   needs grouping, and document why.
5. **Layers** (head to tail, as in the [chain](harness-design.md#chain)):
   - **`PolicyLayer` (interim)**: `pre_tool_exec` calls today's
     `BeforeExecute::decide` and maps `Decision` onto `ToolDecision`. 07
     replaces the body; keep this to a few lines.
   - **`GuardrailLayer` (dedup only)**: `pre_tool_exec` answers cache hits
     with `ToolDecision::Respond`; `post_tool_exec` records executed results
     and runs today's `AfterExecute` transform. The cache is
     conversation-scoped state. Its typed handle on `Conversation` is how
     compaction clears it. 07 adds doom and guardrails here.
   - **`RequestLayer`** (`prepare_request` only), the per-step request-view
     pipeline in today's order:
     - recency tail;
     - `read_lifecycle::apply_to_request`;
     - `compress_request_view`;
     - 04's `run/view.rs` functions (Chat Completions image relocation;
       coalescing only if 04 kept it);
     - the max-output clamp;
     - `RequestOptions` from the landed options code;
     - publishing request overhead into the compaction state.

     It must keep complete tool exchanges and never touch stored history.
     `RecencySource` becomes a field unless two real implementations exist.
   - **`SnapshotLayer`**: `pre_tool_exec` captures `/undo` snapshots for
     writing tools and returns `Run`. Being at the tail, it only sees calls
     every earlier layer allowed. No executor decorator.
   - **`PersistLayer`**: see phase B. In phase A, a no-op placeholder.
6. **Temporary rig-tool bridge** (`harness/rig_bridge.rs`, deleted in 06).
   Wrap each `rig_core::tool::DynamicTool` as a `ToolExecutorImpl`:
   - the definition comes from rig's `ToolDefinition`;
   - scheduling is `Exclusive` for everything;
   - the output becomes `ToolOutput`/`ToolContents`;
   - set the `current_call_id` task-local from `ToolContext::call.id` around
     the call.

   MCP tools go through `register_executor` the same way. Keep the bridge
   under ~150 lines. If it grows, stop and port the offending tool natively
   instead.
7. **Prompt and config.**
   - Build `animist::agent::Prompt` from craft's preamble with identical
     system bytes: one section whose `to_markdown` matches, or set
     `StepRequest::system` in `RequestLayer`. Pin it with a test.
   - `AgentConfig` follows README decision 12, with `max_retries = 0` until
     09.

## Phase B: persistence through the change feed

8. **Persister** (`harness/persist.rs`), one per `Conversation`:
   - On load or a full write, persist `snapshot().entries()` as `entry`
     records and keep `snapshot.generation()` as the cursor. Take the cursor
     from the restored history: `History::from_entries` starts a fresh feed
     (`ForeignVersion`).
   - At each checkpoint, call `history.changes_since(&cursor)`, append its
     `changes()` as 04's records, and only then advance the cursor.
   - `ChangesTooOld`: fall back to a full rewrite with an epoch bump, using
     the existing mechanism, then resume from the new cursor.
   - Never hold a history lock across file I/O (`changes_since` returns
     owned data).
9. **`PersistLayer`** checkpoints in `step_end` (after the step's results
   commit, replacing today's `content_revision` tool-result checkpoints) and
   in `turn_end`. The driver flushes once more after adding terminal
   markers. A persistence error is logged and surfaced as a `CraftEvent`; it
   never fails the run.
10. Move `headless::SessionStore` into `storage::sessions`; the TUI and ACP
    use it from there.

## Phase C: surfaces cut over, old loop deleted

11. **Each surface holds one `Conversation` per session**, built from the
    resumed entries or empty:
    - **TUI live provider**: `SessionState` loses `history`, `compaction`,
      `dedup`, `guardrails` and the per-turn provider rebuild. `TurnCtx`
      shrinks to what the driver needs. Both `compact_history` copies go:
      the driver owns pre-turn compaction from 08. `/compact` returns a
      "compaction returns in 08" info message until then. Model, mode and MCP tool-set
      changes go through `Conversation` (rebuild or `TurnSettings`). The TUI
      `Provider` trait and its mock stay: it is the UI's test seam, with two
      real implementations.
    - **ACP**: the same. Delete `acp/turn.rs`'s pre-turn compaction and
      shared-handle wiring.
    - **Headless and print**: the same. JSON output shapes stay stable for
      SDK consumers (hardening/18). If a field must change, version it.
    - **Subagent launcher**: a child `Conversation` with its own layer
      instances (never the parent's; see
      [State ownership](harness-design.md#state-ownership)), run through
      `run_turn`. 10 finishes subagents and `batch`.
    - Teardown calls `Conversation::shutdown().await` on every exit path:
      TUI quit, ACP session close, headless end, child end.
12. **Delete the dead loop code**: `run::run`, `run_with_images`, the
    `RunOutcome` plumbing nothing uses, `run_loop`, the streaming half of
    `turns::stream_turn`, `stream.rs`, `task_set.rs`, the
    `dispatch_tool_calls` waves, the parts of `ToolDispatch` the registry
    replaces, and the duplicated `run::Event` variants. Keep the pure-logic
    modules (doom, guardrails, dedup, recency, read_lifecycle, view, retry
    classification). Merge the two `ResolvedModel` types if 02/03 didn't.

## Tests

- Port `run/tests/{turns, dispatch, spec}.rs` to drive `Conversation` +
  `run_turn` with `ScriptedProvider`. Update an expectation only where
  animist semantics really differ (placeholder results, call-order slots,
  cancel commits), and list each in the completion note.
- New tests:
  - a panicking tool yields animist's `Panicked` error result, and the run
    continues (pins the model-visible text);
  - cancel mid-stream commits the partial text and the cancel marker, and
    replays cleanly into the next turn **of the same conversation**;
  - cancel mid-tool leaves the interrupted placeholder;
  - the request view holds recency and read-lifecycle markers, while stored
    history does not;
  - the system prompt bytes are unchanged;
  - a dedup hit is served through `Respond` without executing the tool, and
    the surface adapter still renders a start/done pair;
  - `SnapshotLayer` captures nothing for a call the policy layer denied;
  - a model switch between turns keeps history, layer state (dedup cache)
    and the event subscription;
  - persister: for runs with appends, a tool step with mid-run checkpoints
    and a squash, replaying the written records equals
    `snapshot().entries()`; a one-message run on a long session appends a
    small delta; `ChangesTooOld` falls back to a rewrite and resumes;
  - `Conversation::shutdown` runs on every surface exit path (counting
    layer).
- A scripted end-to-end test per surface: TUI (model level), ACP and
  headless.

## Known interim gaps (closed by 06–09)

- Tools are serial: every claim is `Exclusive` until 06.
- Approval goes through the interim `PolicyLayer` until 07; doom and
  guardrails are off until 07.
- Compaction (pre-turn, `/compact`, in-run overflow) is off until 08. Long
  sessions on this branch will hit the context window, which is acceptable
  because the branch doesn't merge before 11.
- No retry, fallback, reauth, `MaxTokens` continuation, nudge, grace or
  advisor until 09.
- Mark the affected tests `#[ignore = "restored in 07"]`, `"restored in 08"`
  or `"restored in 09"`. Don't delete them.

## Acceptance

- `grep -rn 'run::run\b\|run_with_images' agent/src` is empty.
- Every surface holds a `Conversation`. No surface runs compaction, builds
  `RunParams`, or holds a `Shared*` handle for compaction, dedup or
  guardrails.
- The loop no longer streams or dispatches tools itself.
- All non-ignored tests pass, and every ignored test names 07, 08 or 09.
- The test-inventory diff is explained in the completion note.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff, with the [test-integrity](harness-design.md#test-integrity)
   instruction. Address its findings, re-run validation, and repeat until
   the verdict is clean.
3. Append a completion note:
   - the behaviour changes with their tests;
   - the ignored tests;
   - the system-prompt strategy;
   - the event mapping and every derived event;
   - the indirection rows closed;
   - anything left in the driver beyond its listed job, with the reason.

## Completion note

_(fill in when done)_
