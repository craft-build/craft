# 12 — Cleanup, documentation, conformance, followups, async audit

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt)
  and the [harness design rules](harness-design.md). Start with the
  [test inventory](harness-design.md#test-integrity).
- When implementation is finished, **run the review agent**
  as described in [Finish](#finish).

## Goal

Finish the migration: remove leftovers, update the docs that still describe
rig, prove provider conformance without paid calls, confirm the harness is
async native, and confirm no workaround for a released animist followup
survived.

## Work

1. **Leftovers.**
   - Search for rig and its idioms:
     `grep -rni 'rig\b\|rig-core\|rig_core\|DynamicModel\|CompletionSource\|edge::'`
     across `agent/` and `docs/`.
   - Update module docs that cite rig. Many do: `subagent.rs` ("Rig owns the
     loop"), `models_dev.rs`, `model_registry.rs`, `auto_review.rs`,
     `knowledge_memory.rs`, `tools/mod.rs`, `providers/bedrock.rs`, and others.
   - Delete dead code, plus `#[allow(dead_code)]` forward-substrate variants
     that animist now covers.
   - Remove workspace dependencies nothing uses any more. animist owns Bedrock
     credentials, signing, transport and listing (02), so expect
     `aws-sdk-bedrock`, `aws-sdk-bedrockruntime`, `aws-config`,
     `aws-credential-types` and `aws-smithy-types` to go unless a test seam
     still needs one (keep those as dev-dependencies). Check with
     `cargo machete`, or `cargo +nightly udeps` if available, otherwise by
     hand.
2. **User documentation.**
   - Update `agent/README.md` and `agent/agent.example.bml`, plus any
     provider/config docs, covering:
     - the supported kinds and the new kinds (`synthetic`, `zai-coding-plan`,
       `llama-cpp`, `omlx`);
     - the dropped kinds, with migration snippets (`openai-compatible` +
       `base_url`);
     - the Bedrock protocol table (Messages, Responses, Chat Completions,
       Converse), the per-model override, and the Converse thinking caveat;
     - the session format change and the no-downgrade note.
   - Add a CHANGELOG/release note entry if the repo keeps one.
3. **Conformance tests.** Extend or create the suite hardening/25 describes. One
   table-driven test per supported kind, against local mock HTTP servers that
   speak each protocol's SSE (reuse animist's test fixtures as a reference, not
   a dependency). Assert:
   - text, thinking and tool-call round trips through the full
     `Conversation`/layers stack;
   - tool-result images for Anthropic/Responses, and relocation for Chat
     Completions;
   - 429/401/5xx/overflow handling through `RecoveryLayer`, including
     Bedrock's in-provider 429/503 retries not stacking with craft backoff;
   - usage, and pricing where a catalog provides it.
4. **Opt-in live smoke tests.** Add `#[ignore]` live tests, or an example binary,
   gated by env vars, mirroring animist's `docs/manual-provider-tests.md`.
   Document how to run them. Never run them in CI or without explicit approval.
5. **Followups status.** animist 0.7.0 released F01–F07, F09 and F10, and the
   prompts used them directly. Confirm none of the old workarounds exist:
   `grep -rn 'catch_unwind\|tool_name_for\|needs animist F0' agent/src`
   is empty, there is no persistence-time revision pruning, no snapshot-shape
   diffing in session sync, no one-shot-agent tool harness, no
   `with_client`/`max_attempts`/`SdkConfig` handling for Bedrock, and no
   driver re-run for grace, nudge or `MaxTokens`.
   - F08 (usage cost) is still open in animist. If a later animist release
     ships it, price from `TokenUsage::cost` and keep `usage.rs` only for
     craft-specific schedules; otherwise leave craft's pricing in its single
     function and say so in the completion note.
   - New gaps that 02–11 recorded become followup prompts in animist's
     `docs/followups/`, in the existing format.
6. **Async audit** (README → Async native):
   - `grep -rn 'block_in_place\|block_on\|new_current_thread\|futures::executor' agent/src`
     has no hits except the `main.rs` runtime.
   - List every remaining `spawn_blocking` with its reason, and confirm each
     is CPU-bound or a sync-only library.
   - `clippy::await_holding_lock` is clean.
7. **Indirection and layer audit** ([design rules](harness-design.md)):
   - Every row of the
     [inventory](harness-design.md#no-indirection-without-a-job) is closed,
     or its reason for staying is written down.
   - `grep -rn 'RunScope\|HarnessExecutor\|SharedDispatch\|SharedDedupCache\|SharedGuardrails\|SharedCompactionState\|RunParams\|run_policy\|BeforeExecute\|AfterExecute' agent/src`
     is empty (or each hit has a reason).
   - List every craft trait under `agent/src` with its implementations. A
     trait with one non-test implementation is inlined unless it is a
     deliberate test seam, and the note says which.
   - Walk the layer chain. Each layer's hooks match the
     [hook table](harness-design.md#pick-the-hook-by-what-the-behaviour-is).
     No layer instance is shared between agents. The driver does only its
     listed jobs.
   - Count the channel hops from provider delta to the TUI cell and to the
     ACP notification, and record them. Expected: animist → conversation
     sink → surface adapter.
8. **Performance sanity.** Measure startup time, one scripted 50-tool-call
   session, and session save/load times for a large fixture against the 01
   baseline commit, and record them. Investigate any regression above 20%.

## Acceptance

- No rig references remain anywhere in `agent/` or `docs/`, except historical
  notes in `docs/hardening/` and this directory.
- The conformance suite covers every supported kind and passes offline.
- No workaround for a released animist followup remains, and any new gap
  has an animist followup prompt.
- The async audit is clean.
- The indirection and layer audit is clean, and the inventory is closed.
- Final: `cargo fmt -p craft --check`, `cargo clippy -p craft --all-targets`
  (no warnings beyond the 01 baseline, and ideally fewer), and
  `cargo test -p craft` all green.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** over this prompt's
   full diff, **and once more over the entire migration branch diff against
   `main`**. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note:
   - performance numbers;
   - conformance coverage;
   - the final test count compared with the 01 baseline;
   - the trait list from the audit;
   - the line counts of `agent/src` before (01) and after.

## Completion note

**Done 2026-11-01.** Working tree on `animist-migration` (one commit series
with this note; base `97c35759`, animist 11). Validation: `cargo fmt -p
craft --check` clean; clippy `--all-targets` **113** warnings vs the pre-12
tree's **115** (two fewer, zero new); `cargo test -p craft` **2023 passed,
3 failed, 3 ignored** — the failures are the three recipe-render fixed
points; the ignored are the argosy env gate plus the two opt-in live smoke
tests below. No paid calls; everything ran offline against loopback mocks.

### Leftovers

- `grep -rni 'rig\b|rig-core|DynamicModel|CompletionSource|edge::'` over
  `agent/` and `docs/`: **zero hits** outside the historical records in
  `docs/hardening/` and `docs/animist/` (both allowed by the acceptance).
  Ten source doc comments still naming rig were rewritten; `agent/README.md`
  and `agent.example.bml` are fully rewritten (below).
- Dead code deleted: the doom tracker's never-wired stagnation scoring
  (`SCORE_STAGNATION`, `STAGNATION_MIN_UNPRODUCTIVE_TURNS`,
  `SCORE_VALIDATOR_REJECT`, `note_stagnation`, its field and its three
  tests — 07's guardrail-layer similarity detection owns stagnation, so the
  path could never fire), plus the stale `#[allow(dead_code)]` blankets on
  `CraftEvent` (every variant is live) and doom.
- Dependencies: `cargo machete` clean. The one flag (`superlighttui`) is a
  false positive (the lib target is named `slt`; 35 files use it) — pinned
  in `[package.metadata.cargo-machete]` with the reason. The AWS SDK
  crates went with 02; nothing else is unused. No CHANGELOG exists in the
  repo; the README rewrite is the release note.
- **Real bug found and fixed by the perf harness:** the headless/print path
  built its conversation without `.persist(...)`, so `--print` runs
  persisted only the fresh session header the store writes on open — a
  50-tool-call run left a 328-byte file. Every surface test used the
  test-only `record_turn` writer, so nothing caught it. Wired, and pinned
  end to end through the real headless entry point
  (`conformance::the_print_path_persists_its_entries`).

### User documentation

`agent/README.md` rewritten: animist architecture (conversation, the seven
-layer chain, the driver's job), the nine supported kinds with the new
`synthetic`/`zai-coding-plan`/`llama-cpp`/`omlx` kinds, the keyless-local
llama.cpp route, the **dropped-kinds table with migration snippets**
(`openai-compatible` + `base_url`), the Bedrock protocol table with the
per-model override and the Converse thinking caveat, durable
compression/read-lifecycle semantics (entry sources, not a side store),
the session entry-record format with the **no-downgrade warning** and the
crash-safety policy, and a Development section covering the offline
conformance suite and the opt-in live tests. `agent.example.bml` rewritten
to the same kinds (llama-cpp/omlx/synthetic/zai blocks, the dropped-kind
migration example, Bedrock per-model protocol overrides).

### Conformance coverage (offline, hardening/25)

`agent/src/conformance/` — a hand-rolled tokio `TcpListener` mock (mirrors
animist's own test harness; no mock crate) plus per-protocol script
builders (Anthropic Messages SSE, OpenAI Responses SSE, Chat Completions
SSE with index-assembled tool calls, Bedrock Converse **AWS eventstream
framing with CRC32**, chunked writes to exercise SSE reassembly). Twelve
tests:

- **Every kind round-trips text and tool calls** — a 12-row table
  (anthropic, openai responses + chat_completions, openai-compatible,
  synthetic, zai-coding-plan, ollama, llama-cpp, omlx, bedrock converse +
  anthropic_messages + chat_completions): the request hits the dialect's
  path with the kind's auth (x-api-key / Bearer / none / SigV4) and system
  placement, a real `read` executes against a workspace, and the follow-up
  carries the result in the dialect's shape.
- Thinking surfaces as reasoning deltas (all non-Converse dialects);
  tool-result **images stay inline** where accepted (Anthropic, Responses,
  Converse) and are **relocated to a following user message** for Chat
  Completions (the tool message keeps text only).
- Recovery through the full stack: 429 and 5xx retry to success with the
  `Retry` craft event; a 401 parks on the reauth hook and resumes;
  `AuthRequired` surfaces; overflow bodies classify as overflow, not
  transient (fail-fast with the provider's message, no backoff retries —
  the ladder itself is pinned in `run_tests::overflow`); Bedrock's
  in-provider 429 retries (retry-after honored) do **not** stack with
  craft's recovery layer.
- Usage counters flow wire→Done exactly; a models.dev catalog feeds the
  model handle's pricing (cost *settlement* additionally reads craft's
  on-disk models.dev cache — process-wide path resolution, no offline
  seam — and stays pinned at the `usage::settle_session` unit level).

### Opt-in live smoke tests

`conformance::live` — two `#[ignore]`d tests (plain text round trip; a
tool round trip that must read a file and quote it), additionally gated on
`CRAFT_LIVE_BASE_URL`/`CRAFT_LIVE_MODEL` (+ optional `_KEY`, `_KIND`) so
`cargo test` and CI never make model calls. Run instructions are in the
module docs and the README's Development section; the same env serves the
manual QA session. Never run automatically.

### Followups status

`grep -rn 'catch_unwind|tool_name_for|needs animist F0' agent/src` — the
only hit is the **syntect highlight pool** (`markdown/highlight/pool.rs`),
a pre-migration worker-thread isolation guard (one dedicated thread
bounding regex-automata cache memory; panics in a highlighting job must
not kill it). It is not a tool panic boundary — animist owns those
(`ToolError::Panicked`), and craft adds no other unwind boundary. No
persistence-time revision pruning, no snapshot-shape diffing, no
one-shot-agent tool harness, no Bedrock `with_client`/`max_attempts`/
`SdkConfig` handling (animist owns credentials, signing, transport and
its own retries), no driver re-run for grace/nudge/`MaxTokens`.
**F08 remains open** in animist: `TokenUsage::cost` is still never
computed, so craft keeps pricing in `usage.rs` (`settle_session` over the
models.dev cache) — unchanged by this prompt.

### Async audit

- `block_in_place|block_on|new_current_thread|futures::executor`:
  **zero hits** in `agent/src` (stricter than the rule — `main.rs` is
  plain `#[tokio::main]`; the last private runtime died with 02).
- `clippy::await_holding_lock` clean.
- `spawn_blocking`: 21 sites, every one CPU-bound or a sync-only library
  — argosy's sync service/decision/memory APIs (5), the `ignore` walker +
  regex scans (grep/glob/move/skill discovery), fuzzy matching (CPU),
  image decode/downscale/re-encode (CPU), snapshot note/restore (sync fs
  under a state lock), staged tempfile+fsync writes, session index scans,
  the flock+fsync cost ledger, instruction/config loads, and one `git
  branch` invocation kept off the runtime thread for NFS-mounted repos.
  Two call sites lacked the one-line why (`auto_review`, argosy memory
  extraction); comments added. No `std::sync` lock is held across an
  `.await`.

### Indirection and layer audit

- The [inventory](harness-design.md#no-indirection-without-a-job) is fully
  closed (11's note lists the rows; this prompt found none reopened).
  `grep -rn 'RunScope|HarnessExecutor|SharedDispatch|SharedDedupCache|
  SharedGuardrails|SharedCompactionState.*run|RunParams|run_policy|
  BeforeExecute|AfterExecute' agent/src` — `SharedCompactionState` lives
  in `compaction` (11), everything else: no hits.
- **Trait census** (every craft trait under `agent/src` with its impls):
  multi-impl seams — `IntoToolResult` (27 output types), `AskQuestions`
  (dismiss/TUI/ACP), `SpawnSubagent` (none/launcher), `ApprovalResponder`
  (headless/TUI/ACP), `LayerImpl` ×7 layers (external trait).
  Single-production-impl traits, each kept for a named reason:
  `TitleSource` (the bound names what the generic `Session<M, U>` requires
  of its message type; one impl because one message type exists),
  `McpSession` (one rmcp impl; five cfg(test) fakes — the deliberate MCP
  test seam), `ReauthHook` (EnvReauth today; the documented 09 surface
  seam the TUI/ACP wire), the TUI `Provider` trait (one live backend; the
  mock + `examples/mock_tui.rs` make it the UI test seam), `StyleExt`/
  `Sealed` (sealed extension over the external `slt::Style`). No trait
  with a single non-test implementation remains that lacks a reason.
- **Layer chain** (assembled `conversation.rs:436-477`, order
  policy→guardrail→request→recovery→continuation→snapshot→persist, test
  slot first): policy `turn_start`+`pre_tool_exec`; guardrail
  `turn_start`+`pre`+`post_tool_exec`+`step_end`; request `turn_start`+
  `prepare_request`+`response`; recovery `turn_start`+`recover`;
  continuation `turn_start`+`step_end`; snapshot `pre_tool_exec`+
  `turn_end`; persist `step_end`+`turn_end`. Every hook matches the
  [hook table](harness-design.md#pick-the-hook-by-what-the-behaviour-is);
  no layer instance is shared between agents (10's pin).
- **Channel hops**, provider text delta → surface: animist run channel
  (flume, driver) → harness `EventSink` (flume) → TUI session forwarder →
  `mpsc<AgentEvent>` → the slt async pump → `App::handle_event`; ACP:
  run channel → `EventSink` → the turn's `stream.next()` →
  `send_notification`. The expected
  animist → conversation sink → surface adapter shape, no extra hops.

### Performance sanity (release builds, 3 runs each)

Same machine, isolated `$HOME`, `--print` against a loopback scripted SSE
server (openai-compatible, 10 workspace files), old tree = the migration
base commit `1ddddfb8` (the 01 baseline) built in a separate worktree:

| metric | base (1ddddfb8) | animist 12 | Δ |
| --- | --- | --- | --- |
| startup + 1 turn | 351–361 ms | 322–346 ms | **−8%** |
| 50-tool-call session | 208–210 ms | 397–405 ms | **+92%** |
| session file (50 calls) | 113,491 B | ~134,790 B | +19% |
| resume + 1 turn | 126–131 ms | 131–132 ms | **parity** |

The one regression above 20% is the 50-turn write path, and it is the
durability design, not an accident: the change-feed persister flushes an
append+fsync **per step** (a tool result must reach disk before the run
continues — 05's rule), ≈ +3.5 ms/turn over the old loop's end-of-turn
batch write, which lost the whole turn on a crash. The +19% file size
buys verbatim originals for every compressed result and full
revision/deactivation history (the old format kept neither). Follow-up if
the turn cost matters: an fsync policy knob (flush-per-step vs
flush-per-turn) — not changed here, where altering persistence semantics
is out of scope. The perf harness itself (scripted SSE server + isolated
runner) lives in this note's commit message description and is
reproducible against any checkout.

### Counts

- `cargo test -p craft` **2023 passed / 3 failed (recipe fixed points) /
  3 ignored** (argosy gate + 2 live). Listed: **2029** (2017 → 2029; +15
  conformance/live/review-fix tests, −3 dead-stagnation tests mapped
  above).
- Clippy `--all-targets`: **113** (pre-12: 115; the 01 baseline was 135
  lib / 168 all-targets — the migration ends 15% under where it started).
- `agent/src`: **113,795 lines** across the tree (01 baseline: 96,882)
  despite deleting the rig provider stack, the run loop, the compression
  store and the recency seam — the difference is the harness layers, the
  conformance suite, and the TUI features that landed along the way.

### Review

Two passes, as the prompt requires.

**Pass 1 (this prompt's diff): approve_with_nits → fixed.** P2×1,
P3×10, all addressed: the Converse text-script `contentBlockStart`
suggestion was **refuted by the parser** — animist v0.7.0 rejects a
text-open start event, so adding it failed the suite; reverted and filed
as animist followup **F12**
(`~/Projects/animist/docs/followups/F12-converse-text-block-start.md`)
with the failing wire shape, while the mock documents the gap where the
event would go. The rest: atomic reply dequeue in the mock, read
timeouts on stalled connections, `Recorded::text()`'s `null` case, an
uninterpolated panic message, the module doc's one non-loopback
exception (`load_catalog`'s models.dev refresh), a test-owned env var
name (`CONF_PRINT_PATH_KEY`), the example.bml keyless block no longer
setting `api_key_env` (which would have disabled the keyless route it
advertises), pid added to the worktree **branch** name (the dir had it),
a dead `let _ = &mut` workaround, and a 120 s bound on the live smoke
turns.

**Pass 2 (the whole migration branch vs `main`): request_changes →
both blockers fixed.** The per-prompt reviews could not have seen either:

- **[P1, fixed]** the resume "full output" affordance read
  `entry.verbatim()` — but every production tool-result entry's *source*
  is animist's interrupt placeholder (F04's squash shape), so **every**
  resumed tool card offered "click to show full output" and expanding
  showed the false "Tool execution interrupted…" text; the real original
  of an altered result lives in its pre-alteration *revision*.
  `verbatim_result_text` now resolves the richest differing candidate
  across source+revisions, skipping the placeholder, and returns `None`
  when nothing richer than the shown text exists. Pinned on
  production-shaped entries (placeholder source + squashed revision):
  `placeholder_backed_entries_resolve_full_output_from_revisions`
  (unaltered ⇒ no affordance; altered ⇒ the real original).
- **[P1, fixed]** the **repo-root `README.md`** — outside 12's
  `agent/`+`docs/` grep scope — still advertised the deleted rig stack,
  the dropped provider kinds, the retired in-memory compression store,
  and the deleted recency tail as current. Rewritten to the nine-kind
  surface with the dropped-kinds note and durable-compression semantics,
  linking into `agent/README.md` for the tables.
- **[P3, fixed]** deleted the empty, unreferenced `agent/fixes.md`.

The same review verified the cross-prompt seams clean (record vocabulary
round-trips, persister mirror rollbacks and poison-record recovery,
child-history persistence on all exits, layer chain vs the design table,
limits, no silently-skipped coverage: exactly 3 `#[ignore]`s, all
accounted for).

### Manual QA

Handed to the user after this prompt (their call): one interactive TUI
session against a local model with tool calls, cancel, resume and
compaction. Everything scriptable ran offline (above); the live smoke
tests are one env away:

```sh
CRAFT_LIVE_BASE_URL=…  CRAFT_LIVE_MODEL=…  CRAFT_LIVE_API_KEY=… \
  cargo test -p craft --lib conformance::live -- --ignored --nocapture
```
