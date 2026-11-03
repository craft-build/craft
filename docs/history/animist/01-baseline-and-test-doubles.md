# 01 — Baseline, animist dependency, and test doubles

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt).
- When implementation is finished, **run the review agent**
  as described in [Finish](#finish).

## Goal

Make animist 0.7.0 available to `agent/`, record a baseline the later prompts
compare against, and build the fake provider that replaces rig's
`test_utils::MockCompletionModel` in every later test. No behaviour change.

## Read first

- animist 0.7.0 at `~/Projects/animist` (`v0.7.0`, commit `120db39`):
  `src/provider/mod.rs` (`Provider` trait), `src/provider/types.rs`,
  `src/provider/exchange.rs`, `src/provider/model.rs`, `tests/agent.rs` (how
  animist fakes providers), `src/agent/testing.rs`, `docs/agents.md`.
- Craft: `Cargo.toml` (workspace deps), `agent/Cargo.toml`, and every rig
  `test_utils` user: `grep -rn 'test_utils' agent/src`.

## Work

1. **Baseline.** On a clean `main`, run these and record them in this file's
   completion note: test pass/fail counts, failing test names, the count of
   existing clippy warnings, and fmt status.
   - `cargo test -p craft`
   - `cargo clippy -p craft --all-targets 2>&1 | tail`
   - `cargo fmt -p craft --check`

   Later prompts must not add failures or warnings beyond this baseline.
2. **Dependency.** Add `animist = { version = "0.7.0", features = ["bedrock"] }`
   to `[workspace.dependencies]` and `animist.workspace = true` to
   `agent/Cargo.toml`. In `agent/Cargo.toml`'s `[dev-dependencies]`, add
   `animist = { workspace = true, features = ["testing"] }` so tests can use
   `agent::testing::ToolHarness` (06). Keep `testing` out of the production
   build.
   - Confirm `reqwest` 0.13, `jiff` 0.2 and `flume` unify without duplicate
     major versions (`cargo tree -p craft -d`).
   - Add `flume` as a workspace dependency only if craft code needs to name it
     beyond `animist::flume`. Prefer the re-export.
   - Leave the rig dependencies alone; 03 and 06 remove them.
3. **Scripted provider.** Add `agent/src/testing/` (`#[cfg(test)]`, or a
   `test-support` cfg the integration tests can reach) containing:
   - `ScriptedProvider`: implements `animist::provider::ProviderImpl` and
     replays a queue of scripted attempts. Each attempt is a list of
     `ProviderEvent`s plus a final `Result<StreamResponse, ProviderError>`. The
     provider can block until released, to test cancellation and
     interrupted-attempt partials.
   - It records every request it receives (model id, messages, system, tools,
     `RequestOptions`) behind a shared handle so tests can assert on the exact
     request view.
   - Helpers: `text_turn(text)`, `tool_turn(calls)`, `thinking_turn(...)`,
     `max_tokens_turn(text)`, `http_error(status, body)`,
     `transport_timeout()`, and `context_overflow()` (both the
     `StopReason::ContextWindowExceeded` and the HTTP-400 forms).
   - A `scripted_model(provider, ModelInfo)` constructor that returns an
     `animist::provider::Model`. Include a `ModelInfo` builder helper with
     context window, max output, thinking and vision flags.
   - `name()` returns a stable test id, and `list_models` returns the scripted
     infos. `refresh_auth`/`reload_auth` count their calls so reauth tests can
     assert on them.
4. **Self-tests.** Unit-test the scripted provider through a real
   `animist::agent::Agent` with no layers and no tools. Cover:
   - one text turn completes;
   - a scripted tool call reaches a registered no-op tool;
   - a blocked attempt cancelled through `Cancellation` yields
     `RunReport::interrupted_attempts` carrying the scripted partial.

   This proves the double matches animist's contract before anything depends on
   it.

## Constraints

- Do not touch production code paths beyond `Cargo.toml` and the `testing`
  module wiring in `lib.rs`.
- Don't use a real network. If a test needs HTTP-level behaviour (HTTP error
  bodies, SSE framing), use a local `tokio::net::TcpListener` mock, as
  animist's `tests/` do.

## Acceptance

- `cargo build -p craft` and `cargo test -p craft` pass, and the test count is
  the baseline plus the new self-tests.
- `cargo tree -p craft -d` shows no new duplicate `reqwest`/`tokio`/`serde` majors.
- The baseline numbers are recorded below.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note: baseline numbers, decisions, follow-ups.

## Completion note

**2026-10-29, branch `animist-migration`** (uncommitted at time of writing;
base commit `90c82ae7`).

Baseline recorded on clean `main` before any change:

- `cargo test -p craft`: **2040 passed, 4 failed, 1 ignored**. Failing:
  `recipe::tests::render_includes_subrecipe`,
  `recipe::tests::render_substitutes_parameters`,
  `recipe::tests::render_supports_conditionals`,
  `sandbox::mac::tests::sandboxed_process_cannot_write_outside_workspace`
  (all pre-existing; the sandbox one is environment-dependent).
- `cargo clippy -p craft --all-targets`: **135 warnings (lib), 168 including
  test targets** (33 test-only). No growth allowed beyond this.
- `cargo fmt -p craft --check`: **not clean** — 2 pre-existing diffs
  (`agent/src/tui/mod.rs:745`, `agent/src/tui/probe.rs:157`). Left as-is on
  purpose: fixing them is outside this prompt's allowed file surface.

Delivered:

- `animist = { version = "0.7.0", features = ["bedrock"] }` in
  `[workspace.dependencies]`; `animist.workspace = true` in `agent/[dependencies]`
  and `animist = { workspace = true, features = ["testing"] }` in
  `[dev-dependencies]` (dev-only, forward wiring for 06's `ToolHarness`).
  `cargo tree -p craft -d`: no new duplicate majors — `reqwest`/`tokio`/`jiff`/
  `flume`/`serde` unify; the only `serde v1.0.229` double entry is a same-version
  feature-split that predates this change. `flume` stays a re-export
  (`animist::flume`); no workspace entry.
- `agent/src/testing/` behind `#[cfg(test)] pub mod testing;` (craft has no
  `agent/tests/`; every test lives under `src/`): `ScriptedProvider`
  (`ProviderImpl` with a shared script queue, per-attempt blocking gates with a
  one-shot `release()`, full `RecordedRequest` capture, and
  `refresh_auth`/`reload_auth` counters), `Attempt` helpers (`text_turn`,
  `tool_turn`, `thinking_turn`, `max_tokens_turn`, `http_error`,
  `transport_timeout`, `context_overflow` + `context_overflow_http`),
  `scripted_model`, `model_info`.
- 7 self-tests through a real `animist::agent::Agent`: text turn completes;
  scripted tool call reaches a registered no-op tool (result visible in the next
  request); blocked attempt released completes; blocked attempt cancelled via
  `Cancellation` yields `RunStatus::Cancelled` with the scripted partial
  (`display_text` + the usage checkpoint) in `interrupted_attempts`; helper-shape
  and provider-behavior tests (list_models, auth counters, error shapes,
  overflow-body classification via `compaction::llm`).

Decisions:

- `#[cfg(test)]` over a `test-support` cfg: nothing outside the crate consumes
  the double. If 06's `ToolHarness` work needs `agent/tests/` integration tests,
  revisit then (reviewer forward note).
- Attempt state uses `std::sync::Mutex` (never held across an `await`); the
  queues are tiny and this keeps `push`/`requests` sync-callable from tests.
- `ProviderError` is constructed literally (`Http { .. }` with
  `snafu::Backtrace::generate()`; `Transport` from a real loopback timeout via a
  `.no_proxy()` client) because animist's snafu selectors are `pub(crate)`.
- `transport_timeout()` uses a never-accepted loopback listener + 50 ms client
  timeout; `.no_proxy()` guarantees proxy env vars cannot route it off-machine.
- Turn helpers end with a `ProviderEvent::Usage` checkpoint (cumulative, equal
  to the final usage) per the provider contract, so interrupted attempts keep
  usage in their partial.
- Reviewer P3 skips (accepted per prompt mandate): workspace-level `bedrock`
  feature and dev-only `testing` feature are specified verbatim by this prompt.

Validation after the change: `cargo build -p craft` clean; `cargo test -p craft`
**2047 passed / 4 failed / 1 ignored** (baseline + 7, same baseline failures);
clippy exactly at baseline (135/168, one `result_large_err` silenced with a
reasoned `#[expect]` — `ProviderError` is animist's public type); fmt at
baseline (the same 2 tui diffs only). Review verdict after one fix round:
**approve, 0 findings** (P2 proxy hazard and a fidelity P3 fixed; two P3s
justified above).

Follow-ups:

- Pre-existing main failures (3× recipe render, 1× macOS sandbox) and the 2 tui
  fmt diffs predate this work; later prompts must not be blamed for them.
- 06: if `agent::testing` needs to reach `agent/tests/` integration tests,
  switch the module to a `test-support` cfg alias.
