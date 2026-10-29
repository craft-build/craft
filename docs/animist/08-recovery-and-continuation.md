# 08 — Recovery and continuation

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt).
- When implementation is finished, **run the review agent**
  (`.craft/agents/reviewer.md`) as described in [Finish](#finish).

## Goal

Restore every recovery and continuation behaviour that 05 left out, using
animist's mechanisms. In-run recovery goes in `Layer::recover` and
`prepare_request`. `MaxTokens` continuations and nudges are `step_end` appends
in a `ContinuationLayer`. Only the advisor and overflow re-runs stay in the
craft driver. At the end of
this prompt the migrated harness has behavioural parity with `main`, apart from
the deliberate changes already listed.

## Read first

- Craft:
  - `agent/src/run/{retry.rs, turns.rs, overflow.rs, nudge.rs, advisor.rs}`
  - `agent/src/run/mod.rs` (`MAX_OVERFLOW_RECOVERIES`, `MAX_REAUTH_ATTEMPTS`,
    `RunParams::{retry, reauth, compaction, max_continuation_turns, advisor}`)
  - `agent/src/providers/reauth.rs`
  - `docs/hardening/05-complete-request-budgeting.md`,
    `docs/hardening/06-fallback-request-budgets.md`
- animist 0.7.0:
  - `docs/agents.md` → *Step boundaries*, *Execution, history, and
    cancellation*, *Events and statistics* (attempt ids, `Retrying`,
    `AttemptInterrupted`, superseded text);
  - `src/agent/mod.rs`, the retry loop in `run_inner`. Note:
    - `recover` is consulted only for `AgentError::Inference` while
      `retries < max_retries`;
    - the history snapshot is re-taken on every attempt, so compacting inside
      `recover` doesn't conflict.

## Work

1. **`RecoveryLayer::recover`.** Classify `AgentError::Inference { source, attempt }`
   with 03's `ErrorKind`, and apply today's policy:
   - **Rate limited**: rotate the API key if `RetryCtx` has one. Otherwise go
     to the next fallback model, then back off with retry-after. A Bedrock
     429/503 was already retried inside animist (02, 03): skip the same-model
     backoff and go straight to the next fallback, or `Propagate` if there is
     none. Rotation
     rebuilds the provider and changes the model the next `prepare_request`
     uses.
   - **Server/transport**: back off. Sleep inside `recover`, raced against
     `RunContext::cancellation`. Emit `Event::Retry { attempt, message,
     delay_ms }` through `RunScope`.
   - **Timeout**: back off, capped at `MAX_TIMEOUT_RETRIES`.
   - **Auth (401)**: emit `AuthRequired` and await the reauth hook. On success
     either `Provider::reload_auth()` or swap in the rebuilt model, as
     `ReauthHook` returns today. Cap at `MAX_REAUTH_ATTEMPTS`; the counter
     resets after a successful attempt, which you detect from the
     `StepFinished` events or the stats.
   - **Overflow**: recalibrate the estimator and force compaction **inside
     `recover`** through `History::compact` (09 supplies real plans; until then
     use the existing engine via a rewrite adapter). Emit `AutoCompacting` and
     `CompactionDone`, then `Retry`. Cap at `MAX_OVERFLOW_RECOVERIES` per
     successful stream.
   - **Content policy and fatal**: `Propagate`.

   Set `AgentConfig::max_retries` to the run-wide transient budget
   (`TransientBudget`) plus headroom for reauth and overflow, and keep the
   per-kind caps in the layer. Budgets reset per run through `RunScope`.
2. **Fallback model selection** (`RequestLayer::prepare_request`). When
   `RunScope` says a fallback hop is active, set `StepRequest::model` to the
   hop's `animist::provider::Model`, and apply `hop_max_tokens` /
   hardening/06 budgets for that model's window. Record the served spec for
   usage: animist's `StatsSnapshot.by_model` records the actual request model,
   so prefer that over craft's `served_spec`.
3. **Stop-reason statuses.** animist makes `ContextWindowExceeded` (stop reason)
   terminal, and `step_end` may not continue from it (`UnsupportedContinuation`).
   The driver handles it the same way as the overflow error: compact, then
   re-run with a continuation observation. Same caps. This is the one
   continuation that stays a driver re-run.
4. **`ContinuationLayer::step_end`** (in-run, no re-runs):
   - **`MaxTokens`** (`StepOutcome::terminal == Some(MaxTokens)`): up to
     `max_continuation_turns`, append today's continuation text as an
     observation tagged `craft.marker = continuation` and set
     `continue_run`. Report `MaxTokens { reply }` when the budget is spent.
   - **Stalled turn** (`terminal == Some(Completed)` with an empty reply while
     tool results sit in the recent window): append the empty marker and the
     nudge observation (`craft.marker = nudge`), set `continue_run`, and emit
     `Nudge`. The nudge budget resets on real progress (`nudge.rs`
     semantics).
   - Appended messages must be `Role::User` with no tool blocks; animist
     rejects anything else with a specific error.
   - Each continuation consumes a step from `max_steps`, so size `max_steps`
     per README decision 12. A continuation with no step budget left ends the
     run as `StepLimit` with the appended messages still durable; the driver
     maps that to today's outcome for an exhausted budget, and pins it with a
     test.
   - Put this layer after `RecoveryLayer`, and after `GuardrailLayer`'s grace
     append, so a step's observations append in a stable order.
5. **Driver follow-ups** (`harness::driver`), for what `step_end` can't do:
   - **Advisor**: after a terminal `Completed` that wasn't cancelled, run the
     advisor review over this run's delta (`History::snapshot` entries since
     the run started). On `Continue`, run again with the follow-up message, up
     to `max_act_turns`. Keep the no-cancel-race deviation and its comment.
     (The advisor needs a model call and a decision outside the step, so it
     stays a re-run.)
   - **Overflow** re-runs from step 3.
   - Every follow-up run reuses the same `RunScope` counters where today's loop
     shared them across iterations (doom tracker, transient budget). Sum turn
     counts and usage across the runs into one `Event::Done`.
   - **Input-shape constraint**: `Agent::run` input must be `Role::User` with no
     tool blocks, so every follow-up input satisfies that.
6. **Superseded attempts in the surfaces.** On `Event::AttemptSuperseded`, the
   TUI and ACP mark the abandoned attempt's streamed text as superseded. Don't
   concatenate it with the next attempt (`docs/agents.md`). Port today's
   handling of the text that `Retry` leaves on screen, if any.
7. **Usage across attempts.** `Event::Done` uses the settled
   `RunReport::stats`:
   - Price `by_model` with `usage.rs`. animist 0.7.0 still leaves
     `TokenUsage::cost` unset (followup F08 is open), so craft's pricing
     stays; keep it behind 03's single pricing function.
   - Show unpriced attempts as unknown, not zero.
   - Treat `unreported_incomplete_attempts` as unknown usage. Note it in
     `Done` if the TUI can show it; otherwise log it.

## Tests

- Un-ignore every 08-tagged test from 05, and port `run/tests/{overflow,
  advisor, compression}.rs` plus the `retry.rs` state-machine tests to
  `ScriptedProvider`.
- Retry: 429 with retry-after → rotation → fallback → backoff, in that order,
  using paused tokio time (`tokio::time::pause`).
- Reauth: a 401 waits for the hook; the second 401 gives up after
  `MAX_REAUTH_ATTEMPTS`.
- Overflow error and overflow stop reason each compact once and retry, and
  history shows inactive sources plus the summary.
- A cancel during backoff sleep returns `Cancelled` promptly with the
  interrupted attempts.
- `MaxTokens` continuation stops at the budget and runs inside one
  `Agent::run` (one `RunReport`); a nudge fires only after tool results and
  appears as a `MessagesAppended` event; a continuation with `max_steps`
  exhausted ends as `StepLimit` with the marker durable; the advisor
  continues at most `max_act_turns` times.
- Bedrock: a 429 surfaced from animist goes to fallback without a craft
  backoff sleep.
- An abandoned attempt's text is marked superseded and not duplicated (a TUI
  model-level test).

## Acceptance

- No tests remain ignored with an 07/08 reason.
- Behavioural parity with `main`, apart from the deliberate changes collected in
  the 04–07 completion notes. Restate the complete list here.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note: the `max_retries` and `max_steps` sizing, the
   layer order, the complete list of behaviour changes, and follow-ups.

## Completion note

_(fill in when done)_
