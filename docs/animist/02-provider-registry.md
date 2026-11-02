# 02 — Provider cutover on animist (fused with the inference seam)

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt).
- When implementation is finished, **run the review agent**
  as described in [Finish](#finish).

## Goal

Rewrite craft's provider layer directly on animist 0.7.0 and move the inference
seam in the same change: every model call (the run loop, compaction summaries,
auto-review, the advisor, memory extraction) goes through animist, and the rig
provider stack — `providers/`, `DynamicModel`, `CompletionSource`, the rig test
mocks, `models_dev.rs` — is deleted outright. No parallel registry, no
middle-man wrappers: animist *is* the provider layer; craft adds only what
animist cannot carry (config parsing, the disk cache, UI projections).

Craft's own loop and `crate::history` stay; `edge.rs` becomes the craft↔animist
mapping, which 04 then removes.

## Read first

- [README → Provider mapping](README.md#provider-mapping): the supported and
  dropped kinds.
- Craft: `agent/src/providers/*`, `agent/src/models_dev.rs`,
  `agent/src/config.rs`, `agent/src/edge.rs`, `agent/src/run/*`,
  `agent/src/thinking.rs`, `agent/src/setup.rs`, `agent/agent.example.bml`.
- animist 0.7.0: `src/provider/{anthropic,openai,synthetic,zai_coding_plan,
  ollama,llama_cpp,omlx}.rs`, `src/provider/bedrock/*`, `src/provider/{model,
  types,exchange,error}.rs`, `src/catalog/`, `docs/providers-and-protocols.md`,
  `docs/bedrock.md` and the per-provider docs.

## Work

1. **Config.** `ProviderConfig` becomes a serde-tagged enum over
   `ProviderEntry<animist config type>` — animist's own config structs, fields
   and defaults, flattened. A kind-first `Deserialize` gives dropped kinds an
   actionable error naming the replacement, rejects inline `api_key` (keys live
   in the environment), detects unknown-key typos against the serialized
   defaults, and defaults `openai-compatible` to Chat Completions unless the
   protocol is explicit.
2. **Construction (`provider.rs`).** `ProviderHandle::from_config` builds each
   kind's animist provider asynchronously; `ModelHandle` wraps an
   `animist::provider::Model` plus the projection craft needs (context window,
   catalog key, protocol flags). Bedrock keeps one lazily-built provider per
   protocol (the protocol table: `anthropic.*`/claude → Anthropic Messages,
   `openai.*` → Responses, deepseek/moonshot → Chat Completions, else Converse;
   the per-model `protocol` override wins), with eager default-protocol
   construction so region and credential problems surface at startup.
   Keyless loopback/RFC1918 `openai-compatible` endpoints ride animist's
   keyless llama.cpp client.
3. **Catalog.** No craft mirror of models.dev: `provider::load_catalog()` is
   application policy over animist's `Catalog` — 24 h disk cache, stale
   fallback on a failed refresh, never cache an empty parse — returning one
   `Arc<Catalog>` per session. Surfaces pass `Option<&Catalog>` explicitly.
4. **Reauth.** `provider::reauth_hook` polls the credential env var for a
   change, then rebuilds the provider and returns the refreshed model —
   animist binds credentials at construction, so a refreshed key reaches the
   wire only through a new provider.
5. **Edge mapping (`edge.rs`).** craft `history::Message` ↔ animist messages:
   adjacent user messages coalesce; tool-result images relocate only for
   Chat Completions targets; empty-after-filter assistant turns drop;
   reasoning maps to `Thinking`/`RedactedThinking`; `SystemMessagePolicy::
   MoveToPreamble` is set for the protocols that need it;
   `validate_replay` runs over the mapped view in debug builds and tests.
6. **Streaming (`run/stream.rs`).** Drive `InferenceExchange` against
   craft's cancellation: forward deltas and tool starts with the timing the
   surfaces expect, mint run-stable internal tool-call ids, map `StopReason`
   (`MaxTokens` → continuation, `ContextWindowExceeded` → overflow), and carry
   the partial text out on cancel. Classify `ProviderError` into the recovery
   taxonomy (HTTP status first, credential kinds as auth, bedrock sources
   through), marking Bedrock's internally-retried 429/503 so craft's backoff
   does not stack on it.
7. **Request options.** One function builds `RequestOptions` (thinking, fast,
   output cap with the Claude default, temperature, system-message policy)
   from run params plus the `ModelHandle`. `thinking.rs` shrinks to the
   preference types plus `choices_for`/`reconcile_for` over
   `Option<&ModelInfo>`; animist's adapters own the wire fields.
8. **Side completions.** `provider::complete_text` serves the no-tools calls
   (compaction, auto-review, advisor, memory extraction); callers keep their
   deadlines and fail-closed behavior.
9. **Usage pricing.** The static price table prices turns by the served
   model's catalog key; provider usage endpoints are gone (`usage_fetch`
   keeps the `Ok(None)` contract the panels already handle).
10. **Tests.** Port every rig-mock test to the scripted provider from 01;
    deleted suites get named replacements where the behaviour survives.
    Missing upstream behaviour becomes an animist followup, never a silent
    drop.

## Acceptance

- Every supported kind builds; dropped kinds fail with guidance; the example
  config parses.
- `cargo test -p craft` green (modulo the pre-existing failures recorded in
  01), fmt and clippy at baseline, review verdict clean.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note: the keyless rule, the Bedrock protocol table,
  behaviour changes, the removed-test→replacement list, and follow-ups.

## Completion note

**2026-10-30, branch `animist-migration`**, commit `08ed7546` (base
`a5547bc3`, the rig-bedrock/OAuth excision).

One cutover, no parallel registry: `src/providers/`, `src/models_dev.rs` and
every rig mock are deleted; `src/provider.rs` is the seam (construction,
`ModelHandle`, `load_catalog`, `complete_text`, `reauth_hook`), `config.rs`
carries the tagged-enum `ProviderConfig` over animist's own config structs,
`edge.rs` maps craft↔animist history, `run/stream.rs` drives
`InferenceExchange`, and `thinking.rs` is the preference module over
`Option<&ModelInfo>`.

- **Keyless rule**: `openai-compatible` with no credential configured, no
  `OPENAI_API_KEY` in the environment, and a loopback/RFC1918/`.local`/
  `.internal` host rides animist's keyless `LlamaCpp` client; any injected
  key or public host disqualifies (pinned by
  `provider::tests::keyless_local_requires_a_private_host_and_no_key_anywhere`).
- **Bedrock protocol table**: `anthropic.*` or `*claude*` → Anthropic
  Messages, `openai.*` → Responses, `deepseek.*`/`moonshotai.*` → Chat
  Completions, else Converse; a per-model `protocol` in the config wins;
  providers are cached per protocol and the default protocol is built eagerly
  so region/credential problems surface at startup (pinned by
  `provider::tests::bedrock_protocol_follows_the_family_table_and_per_model_override`).
- **Catalog cache**: 24 h TTL, stale fallback on failed refresh, empty parses
  never cached; the cache holds the *normalized* serialization and loads via
  the derived `Deserialize` because `Catalog::from_slice` parses the
  models.dev source shape (pinned by four `provider::tests`).
- **Review**: one P1 found and fixed during review — the reauth hook built a
  fresh provider, reloaded *it*, and dropped it, so the run retried with the
  stale key (animist binds credentials at construction). The hook now returns
  the rebuilt model, and two paused-clock tests pin the wait/refresh/unrefresh
  behavior. One P3 accepted: `edge::assert_replay_valid` also executes
  `validate_replay` in release builds (a no-op branch); it is cheap and keeps
  debug/release paths identical.
- **Validation**: `cargo fmt -p craft --check` clean; clippy **93/119**
  warnings (lib/all-targets) against the 01 baseline's 135/168;
  `cargo test -p craft --lib` **1977 passed, 3 failed** — the three
  pre-existing recipe-render failures recorded in 01.

**Behaviour changes** (all intentional):

1. No connect/stream timeouts on provider clients until F12 lands; Bedrock
   keeps animist's bounded client.
2. Picker display names come from the config's manual table or fall back to
   the model id (models.dev names ride the catalog slice only).
3. Thinking-picker levels come from `ModelInfo.features.thinking.efforts`;
   the per-provider dialect tables are gone, and a required-reasoning model
   offers no `off` choice even when it lists no efforts.
4. Tool-result images stay inline for every protocol except Chat Completions
   targets (previously relocated for all).
5. No longest-prefix dated-id lookup against models.dev; the catalog merges
   by exact ID (animist's rule).
6. Re-authentication rebuilds the provider instead of reloading in place
   (animist binds keys at construction); surfaces already resolve fresh
   handles per turn.
7. Unsigned-reasoning `origin` provenance no longer rides the mapping;
   animist's replay validation owns gating.
8. Provider usage endpoints are gone (`usage_fetch` answers `Ok(None)`);
   usage panels show the session ledger only.

**Removed tests → replacements** (75 removed, all accounted):

- `edge::tests` (17) → the six new `edge::tests::test_*` bidirectional
  mapping/replay tests; preamble/tools/sampling assertions moved into
  `run::stream` and `provider::tests`.
- `models_dev::tests` (14) → animist's catalog suites own parsing; craft's
  cache policy is pinned by `provider::tests::{a_fresh_cache_is_served…,
  an_empty_parse_is_never_cached, a_failed_refresh_serves…, cache_freshness…}`;
  parsing tolerance and the normalized round trip are followups F13/F14.
- `providers::*` (23) → construction is animist's; craft's shares
  (keyless routing, protocol table, manual-model merge) are pinned in
  `provider::tests`; the networked custom-anthropic/openai inference tests
  are subsumed by the TUI preamble SSE test over a real `openai-compatible`
  provider; the timeout-policy tests died with the policy (F12); the
  DeepSeek usage-endpoint tests died with the endpoints.
- `thinking::tests` (12) → five new tests over `Option<&ModelInfo>`;
  dialect-specific cases died with the dialects (change 3).
- `run::stream::tests` (6) → the scripted-driver tests plus the turns/
  overflow suites that already covered finish reasons, usage, and cancel.
- `runtime::tests::catalog_metadata…` → deleted with the helper; windows now
  ride `ModelHandle::context_length` (covered by bootstrap/spec tests).
- `compaction::llm::…rig_error_variants` → renamed/re-expressed as
  `…provider_error_bodies`.

**Follow-ups** (animist `docs/followups/`): F11 Bedrock `MissingRegion`
guidance, F12 provider client timeouts, F13 tolerant models.dev parsing,
F14 catalog normalized-form round trip.

