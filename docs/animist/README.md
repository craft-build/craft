# Migrating `agent/` from rig to animist 0.7.0

Craft's harness (`agent/`, package `craft`, binary `crafty`) runs on
`rig-core`/`rig-bedrock`/`rig-reqwest` 0.44 behind a crate-owned loop
(`agent/src/run/`). This workplan moves it onto
[`animist`](https://crates.io/crates/animist) **0.7.0**. animist covers
providers, streaming inference, history and the agent runtime. Craft becomes
animist's main harness, so the target is not "rig swapped for animist behind
the same loop". The target is:

- **Providers and inference**: animist `Provider`/`Model`/`InferenceExchange`
  replace rig clients, `DynamicModel` and `CompletionSource`.
- **History**: animist `history::{Message, ContentBlock, StoredMessage,
  HistoryEntry, History}` replace `crate::history` and the `edge.rs` mapping.
  Compaction becomes `Compactor` plans and request-view rewrites become
  `Alteration` revisions, not in-place `Vec` rewrites.
- **Agent loop**: one `animist::agent::Agent` per conversation drives each run.
  Craft policy (approval, guardrails, request shaping, retry and recovery) moves
  into an ordered `AgentLayer` chain. Turn-level behaviour that animist leaves
  to applications (continuations, nudges, the advisor, markers) goes in a thin
  craft driver.
- **Tools**: native `animist::agent::tool::ToolImpl` implementations whose
  conflicts are expressed as `ToolScheduling` claims, replacing a global
  workspace lock.
- **Async native**: animist is async end to end, and craft becomes so too
  ([below](#async-native)).

Dropping providers is intentional and approved (see the
[provider mapping](#provider-mapping)). The root desktop app (`/src`, gpui) does
not depend on `agent/`, so it is out of scope.

animist 0.7.0 closes the gaps the 0.5.0 version of this plan worked around
(followups F01–F07, F09 and F10 in animist's `docs/followups/`). The prompts
use the released APIs directly, with no interim workarounds for them. One gap
is still open: [F08, usage cost](#remaining-animist-gap).

## Execute in order

Each prompt is self-contained. Run them in order, one per branch commit series.
Every prompt must end with `cargo test -p craft` green. Prompts 05–08 leave
temporary parity gaps (retry and continuations land in 08). **Merge the
migration branch to `main` only after 11.**

| # | Prompt | Outcome |
| --- | --- | --- |
| 01 | [Baseline, dependency, test doubles](01-baseline-and-test-doubles.md) | animist in the workspace; scripted fake provider; recorded baseline |
| 02 | [Provider registry on animist](02-provider-registry.md) | Async construction of every supported kind into animist providers and models, built in parallel with rig; Bedrock entirely on animist (credentials, client, retries, listing) |
| 03 | [Inference seam cutover](03-inference-seam.md) | Every model call goes through animist; rig provider code, mocks and blocking bridges deleted |
| 04 | [History types](04-history-types.md) | `crate::history` *is* animist history; legacy sessions load; `edge.rs` gone |
| 05 | [Agent runtime core](05-agent-runtime-core.md) | `animist::agent::Agent` runs every turn behind a `run::run` facade; a temporary rig-tool bridge; request and dedup layers; event bridge |
| 06 | [Native async tools](06-tools.md) | Tools are `ToolImpl` with scheduling claims; async I/O; global workspace lock and rig removed |
| 07 | [Tool policy layers](07-tool-policy-layers.md) | Approval, plan mode, guardrails, doom loop, auto-review as layers |
| 08 | [Recovery and continuation](08-recovery-and-continuation.md) | Retry, fallback, reauth and overflow via `recover`/`prepare_request`; `MaxTokens` continuation and nudge via `step_end`; advisor in the driver |
| 09 | [Compaction and alterations on History](09-compaction-on-history.md) | `Compactor` plans for LLM/VCC; `Alteration` passes for output compression and read lifecycle; originals replace the compression store |
| 10 | [Subagents and batch](10-subagents-and-batch.md) | Child `Agent`s for `task`; parallel `batch` via `ToolContext::dispatch_all` |
| 11 | [Surfaces own the conversation](11-surfaces-and-sessions.md) | TUI, ACP and headless hold a `Conversation`; sessions persist the history change feed; facade removed |
| 12 | [Cleanup, docs, conformance](12-cleanup-and-conformance.md) | Dead code gone, docs updated, conformance and smoke tests, async audit |

Keep a short completion note at the bottom of each prompt file when it is done:
date, commit range, test counts, review verdict, and any follow-ups.

## Instructions for every prompt

- **Load the `rust-best-practices` skill before writing any code**, and apply
  it to all Rust you add or change.
- **When implementation is finished, run the review agent**
  (`.craft/agents/reviewer.md`) over that prompt's full diff. Fix every blocking
  or high finding. Fix each medium finding or justify skipping it in the
  completion note. Re-run validation, then re-review until the verdict is clean.
- Work in `agent/`. Read `AGENTS.md`/`AGENT.md` files on the touched paths
  before editing.
- animist reference: `~/Projects/animist` at the `v0.7.0` tag (`120db39`, the
  commit released to crates.io; `git fetch --tags && git checkout v0.7.0`).
  Read `docs/agents.md`, `docs/history.md`, `docs/providers-and-protocols.md`,
  the per-provider docs, and `src/`. Depend on the crates.io release, not a
  path. Craft prompts never edit animist; a new animist gap becomes a followup
  prompt in animist's `docs/followups/`.
- **Async native.** Follow the [async rules](#async-native) in every prompt.
- Validation, at minimum:
  - `cargo fmt -p craft --check`
  - `cargo clippy -p craft --all-targets` with no new warnings compared with
    the 01 baseline
  - `cargo test -p craft`
- No paid model calls. Use the scripted provider from 01, local mock HTTP
  servers, and temp dirs. Never write credentials to files.
- Preserve behaviour unless the prompt names the change. Pin every intentional
  behaviour change with a test, and list it in the completion note.
- Match the surrounding code: snafu errors, comment density, naming. Don't
  introduce `anyhow`/`thiserror`.

## Async native

animist's providers, history, agent and tools are all `async`. Craft adopts the
same model:

- **No sync-over-async bridges.** Delete every `block_in_place`,
  `Handle::block_on`, private `new_current_thread()` runtime, and
  `std::thread::scope` + runtime trick. Today they live in
  `providers/bedrock.rs` (`sdk_config_blocking`, which goes with craft's
  Bedrock SDK chain in 02) and `providers/mod.rs` (`block_on_auth`). Provider construction, discovery, auth refresh, catalog
  loading and model selection become `async fn`s, and callers await them. The
  only runtime builder left is the one in `main.rs`.
- **Provider-facing calls are awaited directly.** That covers inference,
  `list_models`, `refresh_auth`/`reload_auth`, decisions (auto-review moves to
  animist's async `DeciderProvider` where it fits), and embeddings.
- **Tools are async.** Use `tokio::fs`, `tokio::process` and async locks. The
  global `std::sync::Mutex` workspace lock taken under `spawn_blocking`
  (`tools/mod.rs` `Workspace::run`) is replaced by `ToolScheduling` claims plus
  an async per-path lock for cross-agent safety (06).
- **No `catch_unwind` in craft.** animist turns tool panics into
  `ToolError::Panicked` results (F07), so craft adds no unwind boundary of its
  own.
- **`spawn_blocking` is allowed only for CPU-bound or sync-only libraries**:
  tree-sitter, syntect, `ignore` walks, large diffs, argosy indexing, and image
  decode. Each call site gets a one-line comment saying why. Never hold a
  `std::sync` lock across `.await`.
- Cancellation is cooperative: race work against `Cancellation`/`CancelToken`,
  and await futures to completion rather than dropping them when partial
  results matter.

12 audits this:

```sh
grep -rn 'block_in_place\|block_on\|new_current_thread' agent/src
```

The only remaining hit may be the runtime in `main.rs`.

## Target architecture

```
surface (TUI / ACP / headless / print)
  └─ harness::Conversation          one per session: History + Agent + layer handles
       ├─ animist::history::History  durable conversation (revisions, inactive entries)
       ├─ animist::agent::Agent      model, Prompt, ToolRegistry, AgentConfig, layers
       │    └─ AgentLayer chain (head → tail)
       │         1. PolicyLayer        pre_tool_exec: approval gate → plan mode → auto-review
       │         2. GuardrailLayer     pre_tool_exec: doom loop / guardrails / dedup (Respond);
       │                               post_tool_exec: record, warn; step_end: doom grace
       │         3. RequestLayer       prepare_request: recency, in-run read-lifecycle/compression view,
       │                               chat-completions image relocation, max-token clamp, options,
       │                               fallback model
       │         4. RecoveryLayer      recover: backoff, key rotation, fallback, reauth, overflow compaction
       │         5. ContinuationLayer  step_end: MaxTokens continuation, stalled-turn nudge
       ├─ harness::tools             HarnessExecutor decorator (snapshots for /undo)
       ├─ harness::events            AgentEvent → craft run::Event bridge (+ craft-only events)
       ├─ harness::persist           History change feed → session records
       └─ harness::driver            Agent::run around: advisor follow-ups, cancel/fail markers,
                                     overflow re-runs; between-run compaction and alteration passes
```

The final layer order is settled in 07/08. Preserve today's
`ToolDispatch::execute` order (approval → plan mode → guardrails →
dedup/execute → after hook) unless a test proves a reorder is safe.

Layers are agent-scoped, but much craft state is per run. Per-run state goes in
a `RunScope` that the driver installs before `Agent::run` and clears afterwards.
Layers read it through a shared handle.

Keep `crate::run::events::Event` as the surface contract. The bridge translates
`AgentEvent` (including `MessagesAppended` from `step_end`) and adds the
craft-only events (Retry, AutoCompacting, Nudge, StagnationDetected,
AuthRequired, AdvisorNote, AutoReview*).

## Concept mapping

| Craft today | animist 0.7.0 |
| --- | --- |
| `providers::{Provider, ProviderKind}` rig client table (sync build) | `animist::provider::Provider` handle per configured provider (async build) |
| `DynamicModel` / `CompletionSource` | `animist::provider::Model` (+ craft metadata wrapper) |
| `edge::to_request` / `own_to_rig` / `rig_to_own` | none; history is the wire input; request-only views in `prepare_request` |
| `run/stream.rs` over `CompletionStream` | `InferenceExchange` inside `Agent`; `ProviderEvent` via `AgentEvent::Provider` |
| `thinking::attach` / `DynamicModel::prepare` | `RequestOptions { thinking, fast, max_output_tokens, temperature, … }` + `Model::overrides` |
| `history::Message::{System,User,Assistant}` | `history::Message { role: Role::{System,User,Assistant}, content: Vec<ContentBlock>, display_text, kind }`; at-depth system messages need `SystemMessagePolicy::MoveToPreamble` on Anthropic and Converse |
| `UserContent::ToolResult(ToolResult{call,name,content,is_error})` | `ContentBlock::ToolResult { tool_use_id, name: Option<String>, content: ToolContents, is_error, loaded_tools }` |
| `ToolResultContent::{Text,Json,Image}` | `ToolContent::{Text,Image}`; JSON is rendered to text by the tool |
| `AssistantContent::Reasoning` (rig `Origin`, native item) | `ContentBlock::Thinking { signature }` / `RedactedThinking` |
| Markers (`CANCEL_MARKER`, `END_MARKER`, `[Run failed: …]`, grace, nudge) | `Message { kind: Observation }` with a `craft.marker` tag; grace, nudge and continuation are `step_end` appends |
| `run::run(&mut Vec<Message>, …)` | `Agent::run(StoredMessage, RunOptions) -> RunReport` + craft driver |
| `RunOutcome::*` | `RunStatus::*` + driver policy |
| `PortableTool` / `DynamicTool` / `ToolExecutionError` | `ToolImpl` / `ToolExecutor` / `ToolError` |
| `ToolDispatch` + `BeforeExecute`/`AfterExecute` | `ToolRegistry` + `pre_tool_exec`/`post_tool_exec` |
| `Decision::{Run,Skip,Stop}` | `ToolDecision::{Run,Deny,Stop}`; dedup hits use `ToolDecision::Respond` |
| Waves with write-path barriers, `is_never_parallel`, global workspace lock | `max_concurrent_tools` + `ToolScheduling` claims (+ async per-path lock across agents) |
| `CURRENT_CALL_ID` task-local | `ToolContext::call` |
| `CancelToken` | `agent::Cancellation` |
| `RetryCtx` / `stream_with_retry` | `Layer::recover` + `AgentConfig::max_retries` + `prepare_request` model swap |
| Mid-run `turn.push` (grace, nudge, `MaxTokens` continuation) | `Layer::step_end` → `StepAction { messages, continue_run }` |
| `batch` parallel waves | `ToolContext::dispatch_all` |
| Compaction engine rewriting `Vec<Message>` | `History::compact(impl Compactor)` → `CompactionPlan` |
| Request-view output compression / stale-read markers recomputed every step | `History::alter(impl Alteration)` between runs (originals kept); request view only within a run |
| `SharedCompressionStore` / session `tool_outputs` raw copies | Entry sources (`HistoryEntry::verbatim`) |
| Session rewrite/epoch diffing | `History::changes_since(HistoryVersion)` change feed |
| `compaction/vcc` | `history::VccCompactor` + craft `VccExtractor` |
| `usage.rs` ledger | `RunReport::stats` (`StatsSnapshot.by_model`) + craft pricing (F08 is still open) |
| `models_dev.rs` / `providers/catalog.rs` | `animist::catalog::Catalog` (craft keeps its 24h disk cache) |
| argosy sync `decide` under `spawn_blocking` (auto-review) | `animist::provider::DeciderProvider` (async), if it covers the request |

## Provider mapping

Your users run `openai-compatible` (Synthetic, the Z.AI coding plan, local
servers), Anthropic, OpenAI, Ollama and Bedrock. Supported after the migration:

| Config `kind` | animist provider | Notes |
| --- | --- | --- |
| `anthropic` | `anthropic::Anthropic` | `api_key_env`, `base_url`, `beta`, `max_output_tokens`, catalog |
| `openai` | `openai::OpenAi` (`Protocol::Responses` default) | config may select `chat_completions` |
| `openai-compatible` | `openai::OpenAi` with `base_url`, `Protocol::ChatCompletions` default | keyless local servers: see 02 |
| `synthetic` (new) | `synthetic::Synthetic` | existing `openai-compatible` + `api.synthetic.new` configs keep working |
| `zai-coding-plan` (new) | `zai_coding_plan::ZaiCodingPlan` | default env `ZHIPU_API_KEY`; existing `openai-compatible` z.ai configs keep working |
| `ollama` | `ollama::Ollama` | |
| `llama-cpp` (new) | `llama_cpp::LlamaCpp` | optional key; also System One decisions |
| `omlx` (new) | `omlx::Omlx` | optional key |
| `amazon-bedrock` | `bedrock::Bedrock` (feature `bedrock`) via `Bedrock::new` | animist owns credentials, the HTTP client, 429/503 retries and control-plane listing. Protocol per model: Claude → `anthropic_messages`; OpenAI models → `responses`; other OpenAI-surface models → `chat_completions`; everything else → `converse`; config may override |

**Dropped:** `azure`, `chatgpt`, `cohere`, `copilot`, `deepseek`, `doubleword`,
`gemini`, `groq`, `huggingface`, `hyperbolic`, `llamafile`, `minimax`, `mira`,
`mistral`, `moonshot`, `openrouter`, `perplexity`, `together`, `venice`,
`voyageai`, `xai`, `xiaomimimo`, `zai` (the general API).

A config naming a dropped kind fails with an actionable error that suggests
`openai-compatible` + `base_url` for OpenAI-dialect vendors. The OAuth/device
flows (`chatgpt`, `copilot`) and their `block_on_auth` bridge go away, along
with DeepSeek's usage endpoint.

## Decisions taken in this plan

Each is a recommendation. Revisit it in the named prompt if the code proves it
wrong.

1. **Tools are native `ToolImpl` (06).** Pure tool logic stays tested through
   its inner functions. Tests that need a `ToolContext` (progress,
   cancellation, child dispatch) use animist's `agent::testing::ToolHarness`
   (the `testing` feature, enabled only for craft's dev-dependencies), so
   today's `invoke(..)` test helper keeps its call sites.
2. **Order: agent runtime (05) before tools (06).** `ToolContext` only exists
   inside `Agent`, so native tools need the agent. 05 bridges the existing rig
   tools into `ToolExecutor` temporarily, and 06 replaces the bridge.
3. **The surface event contract stays craft's `Event` (05)**, fed by a bridge.
4. **A `run::run` facade bridges 05–10**, and 11 removes it.
5. **History editing vocabulary:**
   - `Compactor` plans for summarization (LLM, VCC);
   - `Alteration` revisions, applied between runs, for durable per-message
     rewrites: tool-output compression, stale or superseded read markers,
     collapsing old results;
   - request-view rewriting only for changes that must take effect *within* a
     run.

   Originals stay in entry sources, so `retrieve` reads them from history.
   That retires `SharedCompressionStore` and the session's raw `tool_outputs`
   map (09).
6. **Session format.** 04 moves message records to animist shapes and loads
   legacy records. 11 persists `HistoryEntry` deltas read from animist's
   change feed (`History::changes_since`): append, insert, revise, deactivate,
   and replace (the agent's tool-result squash). One loader handles every
   older shape.
7. **Placeholder revisions.** The agent writes a placeholder result message and
   revises it as each call finishes. Once every call in the step has a result,
   animist squashes the entry to the placeholder source plus one final
   revision and emits `HistoryChange::EntryReplaced`. Craft persists that as
   is; there is no craft-side pruning.
8. **Model switching** happens between runs, by rebuilding the `Agent` on the
   same `History` handle. Fallback models inside a run go through
   `prepare_request`.
9. **Cancellation** awaits the run after `Cancellation::cancel()`. The driver
   commits the interrupted attempt's validated text prefix (no unexecuted tool
   uses) plus a cancel marker.
10. **Turn-level additions are `step_end` appends.** Doom grace, the stalled-turn
    nudge and the `MaxTokens` continuation are durable observations that
    layers append at step boundaries, with `continue_run` where the run would
    otherwise end. They consume `max_steps`. The driver only re-runs for the
    advisor and for `ContextWindowExceeded` (which `step_end` may not continue).
11. **A failed run keeps any compaction it did.** This is a behaviour change.
    History keeps sources, so nothing is lost.
12. **Limits**: `max_steps` comes from `agent.max_turns` plus the continuation
    and nudge budgets (each `step_end` continuation consumes a step),
    saturating to `u32::MAX`. `max_tool_calls` and `max_child_calls` are high
    (`dispatch_all` reserves a whole batch against `max_child_calls` upfront).
    `tool_timeout_ms = None`. `max_concurrent_tools` comes from config
    (default 8) and bounds both model-emitted waves and `batch` children.
13. **Bedrock belongs to animist.** Craft does not inject a `reqwest` client,
    resolve its own `SdkConfig`, change `max_attempts`, or list models itself.
    animist's Bedrock provider has proved more reliable than the rig-bedrock
    integration; craft only maps config onto `BedrockConfig`.

## Gaps closed by animist 0.7.0

These were workarounds in the 0.5.0 plan. The prompts now use the released API
instead; none of the workarounds should be built.

| Followup | 0.7.0 API | Used in |
| --- | --- | --- |
| F01 step-boundary messages | `LayerImpl::step_end` → `StepAction { messages, continue_run }`; `AgentEvent::MessagesAppended`; `AgentError::UnsupportedContinuation` | 07 (grace), 08 (nudge, `MaxTokens` continuation) |
| F02 parallel child dispatch | `ToolContext::dispatch_all(Vec<ToolCall>) -> Result<Vec<ToolOutput>>`; nested calls fail with `NestedConcurrentDispatch` | 10 (`batch`) |
| F03 tool test context | `agent::testing::ToolHarness` (feature `testing`) | 06 |
| F04 revision squash | Agent squashes tool-result entries to source + final revision before `StepFinished` | 11 (no pruning) |
| F05 history change feed | `HistorySnapshot::generation()`, `History::changes_since(&HistoryVersion)`, `HistoryChange::{Appended, Revised, Deactivated, Inserted, EntryReplaced}`; `HistoryError::{ChangesTooOld, ForeignVersion}` | 11 |
| F06 tool decision with a response | `ToolDecision::Respond { output }` | 05, 07 (dedup) |
| F07 panic-safe tools | `ToolError::Panicked { message }`, model-visible error result | 05 (no `catch_unwind`) |
| F09 Bedrock client, listing, Converse | `ListingMode::ControlPlane`, `Protocol::Converse`, injectable client (craft doesn't inject one) | 02 |
| F10 tool name on results | `ContentBlock::ToolResult { name: Option<String>, .. }` | 04 |
| 0.6.0 at-depth system messages | `Role::System`, `RequestOptions::system_message_policy` | 03, 04 |

Chat Completions still rejects tool-result images; `RequestLayer` moves them
into a following user message. That is a provider limitation, not an animist
gap.

## Remaining animist gap

| Gap | Craft workaround | Followup |
| --- | --- | --- |
| `TokenUsage::cost` is never computed (`Pricing` helpers are dead code in 0.7.0) | Craft prices from `ModelInfo::pricing` / `usage.rs` | animist `docs/followups/F08-usage-cost.md` (open) |
