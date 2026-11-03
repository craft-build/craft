# 05 — Agent runtime core: surfaces own a `Conversation` on animist `Agent`

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt),
  including the [async rules](README.md#async-native), and the
  [harness design rules](../../animist/harness-design.md), which win where the two
  disagree. Read the design rules' [Layers](../../animist/harness-design.md#layers) section
  in full before writing any layer.
- Start with the [test inventory](../../animist/harness-design.md#test-integrity).
- When implementation is finished, **run the review agent**
  as described in [Finish](#finish).

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
  [inventory](../../animist/harness-design.md#no-indirection-without-a-job).

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
     see [State ownership](../../animist/harness-design.md#state-ownership)), the
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
     [ordering rule](../../animist/harness-design.md#events-from-layers).
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
   - Nothing else. See [the driver's job](../../animist/harness-design.md#pick-the-hook-by-what-the-behaviour-is).
4. **Cancellation.** Runs use `animist::agent::Cancellation`. Delete the
   `CancelToken`/`CancelFlag` epoch re-arm and `CancelTrigger` where
   `Cancellation` covers them. Keep `CancelMap` only if per-subagent cancel
   needs grouping, and document why.
5. **Layers** (head to tail, as in the [chain](../../animist/harness-design.md#chain)):
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
      [State ownership](../../animist/harness-design.md#state-ownership)), run through
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
2. **Run the review agent** over this prompt's
   full diff, with the [test-integrity](../../animist/harness-design.md#test-integrity)
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

Commits: `8594378b` (Phase A: conversation/driver/layers on `animist::agent::Agent`),
`e76d6a42` (Phase B: change-feed persister), Phase C (surfaces + deletion, this diff).

### Behaviour changes, with their tests

1. **Every surface drives one `Conversation` per session** through
   `harness::driver::run_turn` (TUI `bootstrap::build_session_state` +
   session-long forwarder; ACP `build_session`; headless/print `cli.rs`;
   subagent child conversations). The per-turn provider rebuild, per-turn
   tool registry, and per-surface commit-on-success are gone — the driver
   and layers own those. *Tests:* `harness/tests.rs` (16),
   `run_tests` (43 live), `tui::provider::live` (55), `acp::tests` (27),
   `subagent::tests` (14), `tools/tests/integration.rs` (2 ported).
2. **Cancellation** is `animist::agent::Cancellation` stored per turn into
   an `Arc<ArcSwap<Cancellation>>` cell that gates, MCP tools, and the
   subagent launcher read at call time. `run::cancel` survives only for
   bang-mode shell runs and the launcher's per-call `CancelMap`. *Tests:*
   harness cancel mid-stream / mid-tool (replay into the same
   conversation), `run_tests::overflow::cancel_mid_stream…`,
   `approval::tests::a_new_turns_cancellation_does_not_cancel_a_parked_approval`.
3. **Failed runs now commit the failed turn** with a `[Run failed: …]`
   observation (the old loop restored a pristine snapshot). *Tests:*
   `run_tests::turns::failed_run_*` (3).
4. **Pre-cancelled input is not admitted**: a turn cancelled before its
   first model call commits the cancel marker only (the old loop's staged
   prompt made the input visible). *Test:*
   `run_tests::turns::cancel_before_the_first_model_call_commits_the_marker_only`.
5. **`Decision::Stop` at the policy boundary maps to
   `TurnOutcome::Failed("Layer stopped the run: …")`** + FAILED marker —
   interim 05 behaviour; 07 revisits stop-vs-cancel identity. *Test:*
   `run_tests::turns::stop_decision_at_the_dispatch_boundary…`.
6. **Dedup replays carry no `[cached]` prefix** at the harness level (the
   `GuardrailLayer` responds with the cached bytes verbatim); the
   dispatch-level `cached_result` path keeps it. *Tests:*
   `run_tests::dispatch` dedup suite.
7. **Model switch compares the full `provider/model` spec**, not the id
   alone (two providers can share an id). *Tests:* harness
   `model_switch_between_turns…`, TUI `run_turn_sends_the_resolved_preamble…`.
8. **TUI session state holds no loop handles**; a session-long forwarder
   maps events, folds `Done` usage into the ledgers, and publishes a
   generation each turn settles on (bounded 5 s, so a dead forwarder
   degrades instead of hanging). *Tests:* `turn::tests::settle_returns…`,
   renderer mapping tests.
9. **`Conversation::shutdown()` runs on every exit path**: TUI loop end,
   ACP `session/close`, print/headless end, and all three subagent spawn
   exits. *Test:* harness `shutdown_is_idempotent_and_terminal`.
10. **MaxTokens replies no longer chain continuations** (09); `/compact`
    and pre-turn compaction return an info notice (08).

Two bugs found by the cutover's tests, both fixed and pinned:

- `SubagentLauncher::spawn`'s cancel/failure path awaited the child event
  forwarder without dropping the child conversation first — the stream's
  last sender never went away, hanging every cancelled subagent. All three
  exits now `shutdown` + `drop(conversation)` before `forwarder.await`.
- `StoredSession::save_to` rewrote the session file (tmp + rename) behind
  the persister's append handle, which kept writing into the unlinked old
  inode: checkpointed messages "appended ok" into a dead file and any
  session that changed model/thinking/mode mid-run lost history on reload.
  `save_to` now bumps the `rewrites` counter, voiding outstanding append
  cursors; the next checkpoint diverges and self-heals with a full
  rewrite. *Test:* `tui…loading_a_persisted_session_repopulates_history`
  (set-thinking-then-checkpoint is the production ordering).

### Ignored tests (28, all in `run_tests`)

- `restored in 07` (4): guardrail warn-then-block, doom grace, doom
  hard-stop, identical-calls-blocked-as-doom-loop.
- `restored in 08` (4): overflow-recovers-compacts-and-retries,
  declined-compaction…, failed-run-after-overflow…,
  overflow-body-without-a-prompt-size….
- `restored in 09` (19): MaxTokens-continuation family (7, `turns.rs`),
  retry/rate-limit/reauth family (8, `overflow.rs`), advisor family (4).
- `restored in 06` (1): `independent_calls_run_concurrently` — parallel
  tool claims return with 06's native tools. (Deviation from the
  07/08/09-only naming rule, kept honest to where the behaviour returns.)

Three run_tests cases were deleted as dead machinery with no future home
(git preserves them): `tool_call_ids_stay_unique_across_model_streams`
(driver-side id re-minting is animist's now), `never_parallel_classification`
(`is_never_parallel` went with the wave dispatcher), and
`served_spec_tests::bare_ids_borrow_the_primary_provider…` (run-policy
accounting internals). The priced-`by_model` half of the spec test is
unportable until a test-priced provider exists (scripted runs always bill
`scripted/<id>`).

### System-prompt strategy

`RequestLayer::turn_start` builds the system bytes once per run via
`prompt::build_system_prompt` with exactly `run_policy`'s old inputs
(cwd/platform/date vars, `config.agent.preamble + instructions`, plan
path) — byte-identical, not re-derived per surface. Pinned by
`harness::request::tests::system_prompt_bytes_match_the_old_policy_output`
and, end-to-end, by the TUI's capture-server preamble test (one
`BASELINE-PREAMBLE`, one `MARKER`).

### Event mapping and derived events

`harness::Event = Agent(animist AgentEvent) | Craft(CraftEvent)`. Surfaces
match animist events directly; `CraftEvent` holds only craft-only variants
(Retry, AutoCompacting, CompactionDone, Nudge, StagnationDetected,
AuthRequired, AdvisorNote, AutoReview\*, PersistenceFailed, Subagent,
Done). Derived events:

- `TextDelta`/`ThinkingDelta` → TUI deltas (+ the streamed flags the
  settle handshake reads); ACP message/thought chunks; print stream-json.
- `ToolStarted` → TUI card (+ `Status::Running` once per run); ACP
  `ToolCall` update.
- `ToolFinished` → TUI done card, `FilesSet` (touched files),
  `PlanSet` (`todo_write`), `SubagentFinished` (`task`); ACP completed
  update; print `user` tool-result event.
- `StepFinished{usage}` → TUI `TokenUsage` label; ACP `UsageUpdate`;
  print assistant usage block.
- `CraftEvent::Done` is consumed by the surface, never rendered: TUI —
  usage rows + `cost.jsonl` + store `record_cost` + the settle bump; ACP —
  responder stop-reason; print — the terminal `result` event.
- `CraftEvent::Subagent` wraps filtered child events, tagged with the
  spawning call's id (task 96).

### Indirection rows closed (harness-design inventory)

`run::run`/`run_with_images`/`RunOutcome` plumbing; `RunParams` +
`runtime::{run_policy, RunPolicyInputs, compaction_ctx,
new_compaction_state}` + `CompactionCtx` (values single-sourced into
`ConversationBuilder` + layer inputs); per-surface turn orchestration
(TUI/ACP compaction copies, shared-handle wiring, per-turn rebuild);
`SessionState.{compaction,dedup,guardrails}`; `run::Event` re-encoding;
`HarnessExecutor` (never built — `SnapshotLayer` is the tail);
`headless::SessionStore` → `storage::sessions` (Phase B); the two
`ResolvedModel` types (runtime's deleted; `model_selection`'s remains).
`CancelToken`-per-run is gone; `CancelMap` kept for
`Command::CancelSubagent`, `CancelToken` kept for bang-mode shell runs.
Still open: `ToolDispatch`/`BeforeExecute` seam (06–07), `ReauthHook`
(09), `impl_tool!`/`Workspace::run` (06), `SharedCompressionStore` (08),
`RecencySource` trait (kept: one real implementation + the test fake).

### Driver extras beyond its listed job

- **Empty-reply continuation** lives in the surfaces (TUI loop,
  `MAX_EMPTY_CONTINUATIONS`), not the driver: it is a surface-level
  rendering/budget concern and 09 moves it into recovery.
- **`emit_done` pricing** (billed cost per model, `settle_session`) runs in
  the driver because only it sees the settled `RunReport`; surfaces read
  `by_model` off the event.
- **`commit_cancelled` validates the interrupted attempt's prefix** with
  `validate_replay` before committing (drops it on mismatch): the driver
  owns history admission.
- **The post-marker `conversation.flush()`** rides the driver because
  terminal markers are driver-written and the flush must order after them.

### Review-pass additions and dispositions

- **Persister mirror rollback** (review P1): `checkpoint` applied a batch
  to the in-memory mirror before the append; an append I/O failure left
  the mirror ahead of the rolled-back file, and the retry would
  double-apply entries into the mirror (baked into the next whole-file
  save). The mirror now rolls back unless the append landed. Pinned by
  the recovery-equality half of the harness checkpoint-failure test; the
  raw I/O-error path itself needs mock I/O to drive portably.
- **Runtime tests restored** (review P1): `workspace_env`'s two tests
  (instruction discovery + reviewer install opt-in) were collateral of
  trimming `runtime.rs` to `workspace_env`; both are back.
- **Force-abort closing** (review P2): the interrupt fallback aborts the
  run future, so `turn_end` never runs; `close_aborted_turn` now flushes
  the change feed and commits the snapshot capture session the same way a
  settled cancelled turn does. The 2 s abort watcher stays detached and
  idempotent (`is_finished` guard makes a second Esc harmless).
- **Esc during turn startup** (review P3): the fresh cancellation is
  stored into the cell before model resolution, so an Esc in the
  network-bound resolve window cancels this turn rather than vanishing.
- **`Conversation::shutdown`** reports through its own
  `ShutdownConversation` error variant (was `BuildConversation`).
- **TUI empty-reply continuation vs 09's `ContinuationLayer`** (review
  P2): the surface-level continuation (`MAX_EMPTY_CONTINUATIONS`) is a
  TUI-only addition, not old-loop parity — the old loop ended the turn on
  an empty terminal reply (`empty_reply_without_recent_tool_results_ends
  the_run`, still live) and nudged only the after-tool-call stall (those
  tests are `restored in 09`). The TUI now nudges exactly the
  terminal-emptiness case, committing a synthetic user message. When 09
  lands `ContinuationLayer`, the surface loop must be removed or the TUI
  will nudge twice.
- **Unbounded conversation sink** (review P2, accepted): the sink is
  single-reader per conversation and every surface drains it — the TUI
  forwarder for the session's life, ACP during the turn, print to
  completion. Bounding it would have to drop events, and the only event
  that must never drop is the terminal `Done` (usage, cost, settle). A
  wedged reader is a bug in the reader, not backpressure the writer can
  resolve; revisit if a surface ever parks a stream mid-session.
- **`AfterExecute`** (review P2, disposition): prompt §A5 expected
  `GuardrailLayer::post_tool_exec` to run "today's `AfterExecute`
  transform", but no parent-level after-transform ever existed — the only
  production installer is `batch`'s internal child table (`TagList`).
  The hook stays for that second implementation; the guardrail hook
  records dedup state only, as before.
- **Stale doc comments** referencing `run::run` (acp/mod.rs,
  subagent.rs) rewritten to the conversation seam; the §C acceptance grep
  is now empty for code and comments.

### Test-inventory diff

`2005 → 1922` listed tests (1918 at the Phase C commit plus the four
review-pass additions: two `runtime::workspace_env` tests, the
responded-call rendering test, and the record-application
non-idempotence pin). Removed: unit tests of deleted modules
(`run::{advisor,nudge,overflow,stream,events,task_set,stats}` — the
run-level behaviours they tested live on as the ported-and-ignored
run_tests shapes; `runtime::run_policy`, with `workspace_env`'s two
surviving tests restored; `run::retry`'s stream driver — its
state-machine test is kept), `headless`'s deleted spawn/interactive API
(8 — multi-turn/cancel/model-swap semantics are covered at the harness
level; `spawn_interactive` had no consumers), the reauth-hook tests (the
hook itself is deleted; 09 restores hook and tests together), 3
documented run_tests deletions, and renames
whose replacements are in the added list (acp turn-tools/slot tests, TUI
interrupt/approval-cell/turn-mapping tests, provider cache tests
restored). Added: 16 harness tests, the ported run_tests suite, policy /
request-layer pins, and the TUI forwarder/settle tests. Ignored: 28
(must be zero after 09; the one `restored in 06` clears with 06).
