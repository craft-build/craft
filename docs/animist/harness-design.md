# Harness design rules for prompts 04–12

Every prompt from 04 onward links here. Where this file and the
[README](README.md) disagree, this file wins. The README was written before
02 and 03 were merged into one hard cutover, and before the facade was
dropped. [README overrides](#readme-overrides) lists each conflict.

## Shape of the migration from 04 on

- **No facade.** `run::run`/`run_with_images` are never rebuilt on top of
  animist. 05 moves the surfaces (TUI, ACP, headless, print, the subagent
  launcher) onto a long-lived `harness::Conversation` and deletes the old loop
  in the same prompt. A throwaway `Conversation` per turn would lose entry
  sources, revisions and inactive entries between turns. That breaks
  `retrieve`, compaction and persistence in every prompt until the facade
  goes.
- **One session format bump, in 04.** 04 defines the final record vocabulary
  (`entry`, `insert`, `revise`, `deactivate`, `replace`) and writes only
  `entry` records. From 05 on, the persister writes whatever animist's change
  feed reports. There is no intermediate `StoredMessage` record format.
- **Order:** 04 history types → 05 runtime and conversation ownership → 06
  tools → 07 policy layers → 08 compaction and alterations → 09 recovery and
  continuation → 10 subagents and batch → 11 sessions, resume, fork and undo
  → 12 cleanup. Compaction now comes before recovery, so overflow recovery
  in 09 calls real `Compactor`s, and no Vec-rewrite adapter is ever built.
- **Build on what 02 and 03 landed.** Read their completion notes before
  starting any later prompt. Use whatever model, registry and request-options
  types they produced. Don't add a craft wrapper around animist's `Provider`
  or `Model` unless it carries data animist can't. If a later prompt names a
  type that 02 and 03 didn't create (`ModelHandle`, "03's options builder",
  "03's `ErrorKind`"), use the landed equivalent. Never create the type the
  prompt named.

## No indirection without a job

Async-native animist removes the reasons for most of craft's plumbing. Before
you keep or add a trait, wrapper, `Shared*` handle, channel hop or params bag,
name the second implementation or the caller it decouples. If you can't, call
the concrete thing directly.

Inventory of known indirection, and where each one goes:

| Today | Goes in | Replacement |
| --- | --- | --- |
| `run::run` / `run_with_images` / `RunOutcome` plumbing | 05 | Surfaces call `harness::driver::run_turn(&Conversation, …)` |
| `RunParams`, `runtime::{run_policy, RunPolicyInputs, compaction_ctx, new_compaction_state}`, `CompactionCtx` | 05 | `ConversationBuilder` from config, plus per-layer config structs. `run_policy`'s *values* stay single-sourced (hardening/01), but it produces layer configs and `AgentConfig`, not a bag the loop unpacks. |
| Each surface's own turn orchestration (TUI `turn.rs` `compact_history` ×2, ACP `turn.rs` pre-turn compaction, shared-handle wiring, per-turn provider rebuild, commit-on-success) | 05 | The driver, once |
| `SessionState.{compaction, dedup, guardrails}` (`Arc<std::sync::Mutex<…>>` passed from surfaces into the loop) | 05/07 | State owned by the layer that uses it (see [Layers](#layers)) |
| `run::Event` as a full re-encoding of the run's events, then re-encoded again into TUI `AgentEvent` and ACP notifications | 05 | `harness::Event = Agent(animist::agent::AgentEvent) \| Craft(CraftEvent)`. Surfaces match animist events directly; `CraftEvent` holds only craft-only variants (Retry, AutoCompacting, CompactionDone, Nudge, StagnationDetected, AuthRequired, AdvisorNote, AutoReview*, Subagent, Done). |
| `ToolDispatch`, `SharedDispatch`, `BeforeExecute`/`AfterExecute`, `Decision`, `DispatchOutcome`, `run/stream.rs`, `run/task_set.rs` | 05–07 | `ToolRegistry` plus layers |
| `HarnessExecutor` decorator (planned in the old 05) | not built | `SnapshotLayer` at the tail of the chain (05) |
| `RecencySource` trait | 05 | A `RequestLayer` field. Keep the trait only if two real implementations exist. |
| `CancelToken`/`CancelFlag` epoch re-arm, `CancelTrigger` | 05 | `animist::agent::Cancellation` per run. Keep `CancelMap` only if per-subagent cancel (`Command::CancelSubagent`) still needs grouping; link child to parent with an awaited `select!`. |
| `ReauthHook` boxed-future type alias | 09 | An `async fn` on a small trait the TUI and ACP implement, held by `RecoveryLayer` |
| `impl_tool!`, `Workspace::run` (`spawn_blocking` + global mutex) | 06 | Native `ToolImpl` |
| `SharedCompressionStore`, session `tool_outputs` | 08 | Entry sources in `History` |
| `headless::SessionStore`, which the TUI also uses | 05 | `storage::sessions` |
| Two `ResolvedModel` types (`runtime.rs`, `model_selection.rs`) | 05, unless 02/03 already merged them | One type |

Each prompt's completion note lists which inventory rows it closed. 12
confirms that every row is closed.

## Layers

The layer chain is where craft policy lives. Use it on purpose:

### Pick the hook by what the behaviour is

| Behaviour | Hook | Never |
| --- | --- | --- |
| Change what the model sees for one request (recency, in-run read markers, image relocation, token clamp, fallback model) | `prepare_request` | Mutate stored history |
| Authorize, deny, stop, or answer a call without running it | `pre_tool_exec` (`Run`/`Deny`/`Stop`/`Respond`) | Execute side effects for a call that a later layer may deny |
| Record or annotate a result | `post_tool_exec` | Assume `ToolStarted` happened (denied and responded calls reach it too) |
| Add a durable observation, or keep a run going | `step_end` (`StepAction`) | Driver re-runs, or pushing into a `Vec` |
| Retry or swap model after an inference failure | `recover` (`Recovery::Retry`) | Loops in the driver |
| Reset per-run state | `turn_start` | Driver-installed scope objects |
| Final per-run bookkeeping (counters, usage, persistence flush) | `turn_end` | Relying on `Drop` |

The **driver** does only what no hook can:
- build the input;
- run `Agent::run`, forwarding events, and await it after cancelling;
- add terminal markers that `turn_end` can't append (cancel, fail, end);
- between-run alteration and compaction passes (08);
- the advisor and `ContextWindowExceeded` re-runs (09).

Anything else that turns up in the driver needs a sentence in the completion
note explaining why no hook fits.

### State ownership

- **Layer instances belong to one `Conversation`.** They are reused when the
  `Agent` is rebuilt (model or tool-set change happens between runs, so
  access stays sequential). **They are never shared between agents**:
  `RunContext::id` is zero-based *per agent*, so a subagent sharing a parent
  layer collides on run ids and corrupts per-run state. Child agents get
  their own layer instances built from shared *services* (permission
  manager, approval responder, guardrail config, event sink). See 10.
- **Per-run state** is a field the layer resets in `turn_start` and finalizes
  in `turn_end`. An agent allows one active run, so a run-id-keyed map isn't
  needed. Don't build a `RunScope` holder that the driver installs.
- **Per-conversation state** (dedup cache, guardrail counters, doom tracker,
  compaction calibration) lives in the layer that uses it, behind an `Arc`
  that the `Conversation` also holds as a **typed** handle. That gives
  outside triggers (compaction reset, new user input, `/clear`) one method to
  call. Don't downcast `Layer` handles.
- **Settings that change between runs without rebuilding the agent** (plan
  versus build mode, thinking level, fast mode) live in one conversation-scoped
  `ArcSwap<TurnSettings>` that layers read at `turn_start`. Rebuild the
  `Agent` only for what it owns: model, prompt and tool registry.
- `pre_tool_exec` and `post_tool_exec` run **concurrently** once
  `max_concurrent_tools > 1`. Guard shared fields with `std::sync::Mutex`,
  and never hold one across `.await`. A layer that awaits the user (approval)
  holds no lock while waiting.

### Events from layers

animist's `AgentEvent` is closed, so layers send `CraftEvent`s through the
conversation's event sink. The surface subscribes to that sink once, for the
conversation's lifetime, not per run. The driver forwards the animist channel
into the same sink. Ordering holds per source only. A layer event sent before
the animist effect it explains (`Retry` before `Retrying`) arrives first. Don't
make surfaces depend on any other cross-source order without a test.

### Chain

Final order, head to tail. Hooks run head to tail, and a denial short-circuits
everything after it:

1. `PolicyLayer`: rules → auto-review → responder → plan-mode gate (07)
2. `GuardrailLayer`: doom loop, guardrails, dedup `Respond`; grace in
   `step_end` (05 adds dedup, 07 adds the rest)
3. `RequestLayer`: request view only (05; 09 adds fallback-model selection)
4. `RecoveryLayer`: `recover` (09)
5. `ContinuationLayer`: `MaxTokens` continuation and nudge in `step_end` (09)
6. `SnapshotLayer`: `/undo` capture in `pre_tool_exec`, at the tail, so only
   calls that will really run are captured (05)
7. `PersistLayer`: flushes the history change feed in `step_end` and
   `turn_end` (05)

A layer with a single, trivial hook is merged into a neighbour. Two concerns
are never merged just to save a type. Each layer has direct tests: the hook
through a `ScriptedProvider` agent, or `ToolHarness` with `layers`.

## Test integrity

The migration is most likely to go wrong by quietly weakening tests. Every
prompt from 04 on:

1. **Before any code change**, saves the test inventory:
   `cargo test -p craft -- --list --format terse 2>/dev/null | sort > target/animist-tests-before.txt`.
   Run it again after the change and diff the two. Each removed or renamed
   test is listed in the completion note with the test that now covers it.
2. Adds `#[ignore]` only with a reason that names the prompt restoring it
   (`#[ignore = "restored in 09"]`). The completion note gives the ignored
   count. It must be zero after 09.
3. Doesn't change an expected model-visible string, denial text, event
   sequence or JSON output shape unless the prompt names that change. Each
   change is pinned with a test and listed as a behaviour change.
4. Tells the review agent, in its prompt, to check the diff for weakened
   assertions, deleted cases, new `#[ignore]`s and widened tolerances, and to
   rate each unexplained one as high.
5. Treats the four failures already on main (3× recipe render, 1× macOS
   sandbox) as fixed points. "Green" means no new failures.

## README overrides

When 02 and 03 have landed, edit the README to match:

- **Execute in order** table: rows 05, 08, 09 and 11 change to match the
  [order above](#shape-of-the-migration-from-04-on), and the 08 and 09 file
  names swap.
- **Decision 4** (facade 05–10): dropped. Surfaces own a `Conversation` from
  05.
- **Decision 6** (session format): a single bump, in 04, to entry records.
  The change-feed persister starts in 05.
- **Target architecture**: drop `harness::tools HarnessExecutor`, add
  `SnapshotLayer` and `PersistLayer`, and drop `RunScope`. The surface event
  contract is `harness::Event`, not the old `run::Event`.
- **Concept mapping**: `run::run(&mut Vec<Message>, …)` maps to
  `harness::driver::run_turn(&Conversation, …)`. `Compaction engine rewriting
  Vec<Message>` is done in 08, not 09.
- "Prompts 05–08 leave temporary parity gaps" becomes 05–09.
