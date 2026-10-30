# 03 — Inference seam cutover

> **Absorbed into [02](02-provider-registry.md).** The cutover was approved as
> one change: providers are rewritten in place on animist and the inference
> seam moves with them, so this prompt has no separate work. Everything below
> is kept for the record; 02's fused prompt and completion note cover it.


## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt).
- When implementation is finished, **run the review agent**
  (`.craft/agents/reviewer.md`) as described in [Finish](#finish).

## Goal

Every model call (the run loop, compaction summaries, auto-review, advisor,
memory extraction) goes through animist. Delete the rig provider registry,
`DynamicModel`, `CompletionSource` and every rig test mock. Craft's own loop
and `crate::history` types stay for now. `edge.rs` changes from craft↔rig to
craft↔animist mapping, which 04 then removes.

## Read first

- Craft:
  - `agent/src/edge.rs`
  - `agent/src/run/{stream.rs, turns.rs, retry.rs, stats.rs, mod.rs}`
  - `agent/src/thinking.rs` (`attach`, `take`, model-info resolution)
  - `agent/src/providers/dynamic.rs` (`prepare`: Claude max-token defaults,
    thinking resolution)
  - `agent/src/compaction/llm.rs`, `agent/src/auto_review.rs`,
    `agent/src/run/advisor.rs`, `agent/src/knowledge_memory.rs`
  - `agent/src/error.rs`
- animist 0.7.0:
  - `src/provider/exchange.rs` (`InferenceExchange`, `PartialResponse`)
  - `src/provider/types.rs`, `src/provider/error.rs`, `src/protocol/replay.rs`
  - `docs/providers-and-protocols.md` (replay rules, sampling controls,
    *System message placement*)

## Work

1. **Mapping (`edge.rs`).** Write craft `history::Message` ↔ animist
   `history::Message`/`ContentBlock` conversions and drop the rig
   `CompletionRequest` builder. Preserve today's request-shaping rules:
   - Adjacent user messages coalesce into one wire message (Anthropic and
     Bedrock need each tool result in the message right after its tool use).
   - Images inside tool results move to a following user message when the
     target protocol rejects tool-result images. animist Chat Completions
     rejects them; Anthropic Messages and Responses accept them. Relocate only
     for Chat Completions.
   - Assistant turns that become empty after filtering are dropped.
   - Reasoning maps to `Thinking { thinking, signature }` /
     `RedactedThinking { data }`. Port the signature extraction. Rig `Origin`
     replay gating disappears, because animist protocols validate replay
     themselves.
   - `System` messages map to `Role::System` text messages (animist 0.6.0).
     Production code doesn't produce any today (`compaction/llm.rs`,
     `estimate.rs`, `strip.rs`, `vcc/` and `run/overflow.rs` only consume
     them), but legacy sessions may hold them. Anthropic Messages and Bedrock
     Converse reject at-depth system messages by default, so the options
     builder (step 3) sets `SystemMessagePolicy::MoveToPreamble` for those
     protocols. Pin that with a recorded-request test.
   - Run `animist::protocol::validate_replay` on the mapped view in debug
     builds and tests.
2. **Streaming (`run/stream.rs`).** Drive `InferenceExchange` over
   `Provider::stream_message`.
   - Map `ProviderEvent::{TextDelta, ThinkingDelta, ToolUseStart,
     ToolInputDelta, Usage, PromptProgress}` onto today's `Event`s
     (`ToolPending`/`ToolStart` timing must match what TUI and ACP expect).
   - Build `TurnOutput` from `StreamResponse`.
   - Map `StopReason` onto today's finish handling: `MaxTokens` → the
     continuation path, `ContextWindowExceeded` → the overflow path.
   - On cancel, use `exchange.cancel()`'s `PartialResponse` instead of rig's
     partial aggregation.
3. **Request options.** Replace `thinking::attach`/`take` and
   `DynamicModel::prepare` with a function that builds `RequestOptions`
   (thinking, fast, max_output_tokens, temperature, system_message_policy)
   from `RunParams` plus the `ModelHandle`.
   - Keep the Claude max-token default and its warning, applied when neither
     the catalog nor config names a ceiling.
   - Shrink `thinking.rs` to preference parsing plus the
     `ThinkingConfig` → `animist::provider::ThinkingConfig` mapping. Craft's
     enum matches animist's shape, so consider re-exporting animist's.
   - Let animist adapters own the wire fields.
4. **Retry classification (`run/retry.rs`).** Reclassify
   `animist::provider::ProviderError` into `ErrorKind`:
   - `Http { status }` with 429 → rate-limited (parse retry-after from the
     body/text the same way as today);
   - 401 → auth;
   - 5xx and `Transport` → server/transport;
   - timeout `reqwest` errors → timeout;
   - context-length messages, both HTTP 400 bodies and
     `StopReason::ContextWindowExceeded` → overflow;
   - `Bedrock` errors via their source. A Bedrock 429/503 has already been
     retried by animist (02); classify it as rate-limited/server but mark it
     as provider-retried, so 08 skips same-model backoff and goes to
     fallback.

   Port the existing classification tests to animist errors. Fallback hops and
   key rotation rebuild `ModelHandle`s through 02's registry.
5. **Auto-review decisions.** `auto_review.rs` runs argosy's synchronous
   `decide` under `spawn_blocking`. Check whether the request (a `noul`
   yes/no plus calibrated confidence) maps onto animist's async
   `DeciderProvider` (`docs/deciders.md`; System One via `llama-cpp`).
   - If it maps, call it with `.await` and the existing deadline, and keep
     fail-closed behaviour.
   - If it doesn't (argosy-specific calibration), keep argosy behind the same
     seam but note it as a remaining blocking site with a reason.
6. **Side completions.** Add one helper (in `providers/` or `harness/infer.rs`)
   that runs a no-tools completion: drain `stream_message` events concurrently,
   return the assembled `StreamResponse`, and bound it by a deadline. Port
   compaction summarization, auto-review, the advisor and memory extraction to
   it. Keep each call site's existing fail-closed and fallback behaviour.
7. **Switch the registry.**
   - `providers::Provider` / `ProviderKind` now mean the animist registry from
     02. Delete the rig `providers!` table, all the `build_*` rig factories,
     `dynamic.rs` and `rig_reqwest` usage.
   - Update `runtime.rs`, `model_selection.rs`, `setup.rs`, the TUI model
     picker, and ACP config options to the new kinds, and await the async
     construction and discovery from 02. No `block_in_place`/`block_on` may
     remain in `providers/`.
   - Update `agent.example.bml` and user docs for the new and dropped kinds.
8. **Usage.** `stats.rs` gets usage from `StreamResponse::usage` (animist
   `TokenUsage`), mapped into `history::Usage`. Pricing stays in `usage.rs`,
   because animist 0.7.0 still leaves `cost` unset (followup F08 is open).
   Keep the pricing behind one function so F08 can replace it. Fill
   `Event::Done::context_window` from `ModelInfo::config.context_window`
   instead of the `0` sentinel.
9. **Tests.** Replace every rig `MockCompletionModel`/`MockStreamEvent`/
   `MockError` use with 01's `ScriptedProvider`. That covers
   `run/tests/*`, `stream.rs`, `retry.rs`, `compaction/*`, `auto_review.rs`,
   `advisor.rs`, `subagent.rs`, `headless.rs`, `runtime.rs`,
   `providers/mod.rs`, `acp/tests.rs`, and `tui/provider/live/tests.rs`.
   - Port assertions on the request shape to the recorded animist requests.
   - Keep the test intent identical. Where a test asserted rig-specific
     internals, assert on the observable request instead, and note it.

## Constraints

- rig remains only for the tool traits (`PortableTool`, `DynamicTool`,
  `ToolExecutionError`, `ToolDefinition`). `ToolDefinition` → animist
  `provider::Tool` conversion lives in one function until 06 removes rig.
- No behaviour change visible to users except removed provider kinds and a real
  `context_window` in `Done`.

## Acceptance

- `grep -rn 'rig_core::\(streaming\|providers\|test_utils\|driver\|wire\|http_client\|model\|ProviderError\)\|rig_bedrock\|rig_reqwest' agent/src`
  returns nothing. The only remaining `rig_core` imports are tool traits and
  types: in `tools/`, `run/dispatch.rs`, `compaction/estimate.rs`, and the one
  `ToolDefinition` conversion function.
- `rig-bedrock` and `rig-reqwest` are removed from both `Cargo.toml`s.
- The test count is ≥ 01's baseline (ported tests count once). No new failures.
- `grep -rn 'block_in_place\|block_on\|new_current_thread' agent/src/providers agent/src/auto_review.rs`
  is empty.
- A recorded-request test proves that coalescing, image relocation and
  reasoning signatures survive the round trip for the Anthropic, Responses and
  Chat Completions protocols.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note: test-intent changes, and any classification
   differences.

## Completion note

_(fill in when done)_
