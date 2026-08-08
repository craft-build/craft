# Feature spec: Resilience / fault-injection test harness

Status: implementation specification, ready to implement. Part of the clark-code
feature review (see `docs/clark-code-review-assessment.md`).

## 0. Summary

Craft is a tool-heavy local agent that drives `bash`, edits files, and spawns
subagents. It has unit and integration tests (`nextest`) and a scripted
`Synthetic` provider, but **no fault injection**: nothing exercises what happens
when a provider stream disconnects mid-chunk, a rate limit fires, a tool call
is duplicated, a transport error lands after a side-effecting tool ran, or the
user cancels mid-turn. Those are exactly the conditions that corrupt history,
leave a partial edit without a tool result, or deadlock the loop.

This spec adds a deterministic fault-injection layer: a `Provider` wrapper that
decorates any provider (real or `Synthetic`) and injects typed faults at the
same seam the agent loop uses, driven from `nextest` integration tests. Port the
*idea* from clark-code, not its Playwright/browser harness (craft has no React
projection to drive from a browser). Each case asserts the agent loop reaches a
clean terminal or recoverable state.

## 1. Why this matters

The agent loop's correctness contract is not "happy path works" — it is
"transient provider and transport failures do not corrupt state." Today that
contract is untested. A mid-stream disconnect after a `bash` tool result is
attached but before the model's next message is persisted could leave history
in a state where a follow-up turn re-runs the tool. A duplicated tool-call ID
could double-execute a side effect. These are the bugs that surface in
production against flaky providers and are nearly impossible to reproduce by
hand. A deterministic power-set over fault dimensions turns them into CI cases.

clark-code runs a 2⁶ = 64-case power set this way and treats it as a
release gate. Craft's loop is structurally simpler (single process, no cloud
sync, no projection divergence), so a smaller, Rust-native version covers the
real risks.

## 2. Reference: how clark-code does it

- **Six fault dimensions, power-set.** clark
  `app/src/core-bridge/resilienceBenchmark.json:7-14` pins the contract:
  `rate_limit`, `duplicated_tool_ids`, `event_stream_disconnect`,
  `provider_process_loss`, `cloud_sync_delay`, `user_cancel`. Each case is a
  bitmask; `2 ** faults.length` = 64 cases.
- **Bitmask selection.** clark `harness/resilience-benchmark.mjs:181-183`
  (`faultsForMask`) selects faults where `mask & (1 << i)`. The runner iterates
  `0..2**faults.length` (`:526`); `--smoke` runs the healthy baseline, each
  fault in isolation, and the all-faults interaction (`:520-521`).
- **Inject at the real typed seam.** Faults are injected in-browser at the same
  typed event seam the native provider uses (`playResilienceSimulation`), so
  simulated cases cost zero API calls.
- **Health assertions.** clark `harness/resilience-benchmark.mjs:247-268`
  (`assertHealthySurface`): conversation body is non-trivial, no forbidden
  implementation-detail strings leak, provider-incident-card count matches
  expectations, the surface is not blank, zero browser console errors.
  `user_cancel` and `provider_process_loss` also verify a *Continue from saved
  progress* recovery path.
- **Live control (optional).** A separate lane runs a real model via `devbridge`
  (`mjs:386-505`) to confirm the simulated seam matches real behavior. Opt-in,
  costs live credits.

## 3. Where this lives in craft

| Concern | Location |
|---|---|
| Provider trait (the seam to wrap) | `craft-providers/src/provider.rs:274` — `Provider::stream_message` returns the event stream |
| Scripted base provider | `craft-providers/src/providers/synthetic.rs:106-145` (`Synthetic`, `impl Provider`) |
| Agent loop under test | `craft-agent/src/agent/run.rs:502` (`run_loop`), turn boundary at `run.rs:556-600` |
| History invariants to assert | `craft-agent/src/agent/history.rs` (`ArcSwap<HistorySnapshot<Vec<Message>>>`); tool-result pairing |
| New: fault injector | `craft-providers/src/providers/fault.rs` (or a `test-support` feature in `craft-providers`) |
| New: harness tests | `craft-agent/tests/resilience.rs` (integration) |

`MockProvider` already exists in `craft-agent/src/agent/run.rs` (used by the
loop tests around `run.rs:2099-2540`). The injector is a generalization of that
pattern: a provider decorator rather than a hand-rolled mock.

## 4. Implementation

### 4.1 The fault dimensions (craft-adapted)

Adopt clark's six, renamed to craft's reality. Craft has no cloud sync and no
separate provider process, so `cloud_sync_delay` and `provider_process_loss`
map to craft's analogues:

| clark fault | craft fault | What the injector does |
|---|---|---|
| `rate_limit` | `RateLimit` | Return an HTTP-429-style error event on the Nth call; optionally with a `Retry-After`. |
| `duplicated_tool_ids` | `DuplicatedToolCallIds` | Emit two tool-use blocks sharing one ID in a single assistant message. |
| `event_stream_disconnect` | `StreamDisconnect` | Drop the stream mid-chunk (return `None`/err before `EndTurn`) after K events. |
| `provider_process_loss` | `ProviderError` | Return a hard `AgentError`/transport error from `stream_message` (e.g. after a tool result is attached). |
| `cloud_sync_delay` | `n/a` | Drop (no cloud sync in craft). Replace with `CompactionInterrupted`: trigger context compaction that fails mid-way. |
| `user_cancel` | `Cancel` | Fire the run's cancel token mid-turn. |

So craft's set is: `RateLimit`, `DuplicatedToolCallIds`, `StreamDisconnect`,
`ProviderError`, `CompactionInterrupted`, `Cancel` — still 2⁶ = 64, but each
maps to a real craft failure mode. Adjust the set during implementation; the
mechanism is dimension-agnostic.

### 4.2 The injector

New `craft-providers/src/providers/fault.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault { RateLimit, DuplicatedToolCallIds, StreamDisconnect,
                 ProviderError, CompactionInterrupted, Cancel }

pub struct FaultSet(pub u64);  // bitmask

pub struct FaultProvider {
    inner: Arc<dyn Provider>,
    faults: FaultSet,
    // counters/state to decide when to fire each fault
}

impl Provider for FaultProvider {
    async fn stream_message(&self, req: RequestOptions, ...) -> ... {
        // Decide, based on self.faults and call counters, whether to:
        //   - inject a rate-limit error before delegating,
        //   - delegate then drop the stream after K events,
        //   - synthesize a duplicated tool-call id in the response,
        //   - return a transport error,
        //   - etc.
    }
}
```

The injector wraps `inner` (a `Synthetic` provider scripted to produce a known
turn sequence with at least one tool call and a tool result). It must be
deterministic: same `FaultSet` + same scripted inner = same observable stream,
no real randomness, no real network.

Design the firing triggers to be *meaningful*, not just present: `StreamDisconnect`
should fire *after* a tool result has been attached to history but *before* the
follow-up assistant turn is persisted — that is the dangerous window. `ProviderError`
should fire after a side-effecting tool call. Each fault's trigger is part of the
spec, not left to implementation whim.

### 4.3 The harness

New `craft-agent/tests/resilience.rs` (integration test, gated behind a
`test-support` or default test feature):

- A helper `run_case(mask: u64)` that builds a scripted `Synthetic` provider for
  a representative turn (user message → assistant tool call → tool result →
  assistant follow-up → `EndTurn`), wraps it in `FaultProvider::new(inner, mask)`,
  runs `Agent::run`, and asserts invariants.
- Iterate all masks `0..2**FAULTS.len()`; `--smoke` equivalent via a
  `#[test_case]` subset (mask 0, each single bit, all-bits).
- Mirror clark's smoke set: a parametric test over
  `[0, 1<<0, 1<<1, ..., all_bits]` for fast CI, plus a full power-set test
  marked `#[ignore]` (run explicitly, like clark's full run) or run in full if
  cheap enough.

### 4.4 Invariants to assert (the "healthy surface" contract)

Port clark's `assertHealthySurface` to craft's invariants. For each case,
after the run settles (terminal stop reason, error, or cancel):

1. **No panic.** The run returns `Ok` or an `Err`, never panics.
2. **History is consistent.** Every tool-use block has a paired tool-result (or
   a synthetic error result the loop inserted). No orphan tool calls. Assert via
   a history walker over `HistorySnapshot`.
3. **No partial edit without a result.** If a side-effecting tool call
   (`edit`, `write`, `bash`) was emitted, its result block is present; the loop
   did not re-prompt the model with an unpaired tool use.
4. **Recoverable.** A follow-up turn (a fresh user message) on the same history
   reaches a clean terminal state — the loop is not wedged.
5. **No leaked implementation details.** Error strings the user would see are
   sanitized (no raw internal paths/IDs in user-facing stop reasons); mirror
   clark's "no forbidden strings" check at the seam that reaches the UI.
6. **Cancel is honored.** For `Cancel`, the run stops promptly with
   `StopReason::Cancelled` and history is committed (`snapshot.commit()`).

Document each fault's *expected* terminal outcome (some faults legitimately end
in an error stop reason; that is healthy as long as invariants hold).

### 4.5 Optional live control

Provide an env-gated live lane (e.g. `CRAFT_RESILIENCE_LIVE`) that runs a
representative subset against a real weak-tier provider, asserting the same
invariants. Mirror clark's discipline: opt-in, costs credits, pinned model,
never part of the default CI gate unless explicitly enabled. This catches
seam drift between the injector and real provider behavior.

## 5. Acceptance criteria

- [ ] `FaultProvider` wraps any `Provider` and injects each of the six faults
  deterministically at meaningful trigger points.
- [ ] An integration test runs the smoke power-set (baseline + each fault in
  isolation + all-faults) and passes in CI.
- [ ] A full power-set test exists (optionally `#[ignore]`d for speed) and
  passes when run explicitly.
- [ ] Each case asserts the §4.4 invariants (no panic, history consistent, no
  orphan tool calls, recoverable follow-up, cancel honored).
- [ ] At least one real bug is found or guarded: e.g. a `StreamDisconnect`
  after a tool result no longer leaves an unpaired tool call in history after
  the fix this harness surfaces.
- [ ] `cargo nextest run --all-features --workspace` and
  `cargo clippy --all-features --all --tests -- -D warnings` pass.

## 6. Risks and notes

- **Scope creep.** Resist turning this into a general property/fuzz harness.
  It is a deterministic power-set over six named faults with asserted
  invariants — that is the whole deliverable.
- **Determinism.** Every fault must be reproducible. No wall-clock randomness,
  no real network. The `Synthetic` inner provides the scripted turns; the
  injector only mutates how/whether they are delivered.
- **Test speed.** 64 cases × a few scripted turns each must stay under a few
  seconds. If too slow, run the full set `#[ignore]`d and the smoke set in CI.
- **What this is not.** It is not a UI/Playwright harness (craft has none), not
  a benchmark, and not a paid-model eval. It is a correctness gate for the agent
  loop's failure handling.
- **Relationship to `Synthetic` provider.** `Synthetic` already scripts turns;
  the injector composes *on top of* it. Do not fork `Synthetic` per fault —
  decorate it.
