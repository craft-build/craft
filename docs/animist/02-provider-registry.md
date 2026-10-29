# 02 — Provider registry on animist (built in parallel, not wired)

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt).
- When implementation is finished, **run the review agent**
  (`.craft/agents/reviewer.md`) as described in [Finish](#finish).

## Goal

Build, **asynchronously**, every supported config `kind` into an `animist::provider::Provider`, and
every selected model into an `animist::provider::Model` that carries craft's
model metadata. This lands next to the rig registry without replacing it, so
the build stays green. 03 cuts the inference path over and deletes the rig
registry.

## Read first

- [README → Provider mapping](README.md#provider-mapping): the supported and
  dropped kinds.
- Craft:
  - `agent/src/providers/{mod.rs, registry.rs, dynamic.rs, bedrock.rs,
    catalog.rs, openai_compat.rs, reauth.rs, usage_fetch.rs}`
  - `agent/src/config.rs` (`ProviderConfig`, `ModelConfig`)
  - `agent/src/models_dev.rs`, `agent/src/model_registry.rs`,
    `agent/src/model_selection.rs`, `agent/src/thinking.rs` (model-info
    resolution), `agent/src/setup.rs`
  - `agent/agent.example.bml`
- animist 0.7.0:
  - `src/provider/{anthropic, openai, synthetic, zai_coding_plan, ollama,
    llama_cpp, omlx}.rs`
  - `src/provider/bedrock/{mod.rs, provider.rs, listing.rs}`,
    `src/provider/model.rs`, `src/catalog/`
  - `docs/bedrock.md`, `docs/openai.md`, `docs/ollama.md`, `docs/llama-cpp.md`,
    `docs/omlx.md`, `docs/synthetic.md`, `docs/zai-coding-plan.md`

## Work

1. **Kinds.** Add `providers::animist` (the module name is up to you), with a
   new `AnimistKind` enum for the supported kinds:
   `anthropic`, `openai`, `openai-compatible`, `synthetic`, `zai-coding-plan`,
   `ollama`, `llama-cpp`, `omlx`, `amazon-bedrock`.
   - Parse from the same `ProviderConfig`. A dropped kind gives an actionable
     snafu error that names the kind; for OpenAI-dialect vendors, the error
     suggests `kind = "openai-compatible"` with that vendor's `base_url`.
   - Don't change how `ProviderConfig` deserializes yet; 03 switches the real
     registry over.
2. **Construction.** One builder per kind, mapping `ProviderConfig` fields
   (`api_key_env`, `base_url`, a protocol selection where supported, the
   `max_output_tokens` default) onto the animist config structs.
   - Give each non-Bedrock provider `.with_client(..)` with craft's
     `Timeouts` (connect timeout + stream timeout) on a plain
     `reqwest::Client`, replacing `timeout_client`'s rig wrapper. Bedrock
     keeps animist's own client (below).
   - `openai-compatible` → `OpenAi` with `Protocol::ChatCompletions` by
     default, overridable to Responses.
   - **Keyless local endpoints.** `OpenAiConfig` reads `api_key_env` at
     construction. Check what craft does today for keyless
     `openai-compatible`/`ollama`/`llamafile` hosts, and keep that behaviour:
     - Prefer routing local hosts the user configured as `openai-compatible`
       through a keyless path. `LlamaCpp`/`Omlx` take an optional key; a
       placeholder key is only acceptable if the endpoint ignores it.
     - Document the rule you chose.
   - **Bedrock.** Use animist's Bedrock provider as it ships (README
     decision 13). It is more reliable than the rig-bedrock integration, so
     craft does not tune it:
     - Construct with `Bedrock::new(BedrockConfig)`, mapping the config's
       `profile`, `region` and `endpoint_url` onto `BedrockConfig`. animist
       loads the AWS credential chain itself and resolves credentials per
       attempt.
     - Don't call `with_client`, and don't change `max_attempts`. animist's
       default client (redirects off, 10 s connect timeout) and its 429/503
       retries (3 attempts, `Retry-After`-aware backoff) stay as they are.
       The `RecoveryLayer` (08) must not stack its own same-model backoff on
       a Bedrock 429/503 that animist already retried; it goes straight to
       fallback.
     - Set `listing_mode(ListingMode::ControlPlane)`, and pass craft's
       models.dev catalog as `BedrockConfig::catalog`. animist lists
       foundation models and paginated inference profiles with signed
       requests and merges catalog metadata by exact ID. Craft enriches what
       is still missing (profile → underlying model, longest-prefix lookup
       for dated ids, the `limits_for` context table) on the returned
       `ModelInfo`s, not by listing again.
     - Delete craft's Bedrock SDK chain in `providers/bedrock.rs`:
       `sdk_config`, `sdk_config_blocking`, `load_sdk_config`, the region
       chain, the fast-IMDS setup, the `aws-sdk-bedrock` listing client and
       `aws_error_chain`. Keep only the enrichment helpers and the actionable
       error text (map animist's `BedrockError`, for example a missing region,
       onto craft's existing messages).
     - Pick the protocol per model: Claude → `AnthropicMessages`, OpenAI
       models (for example `openai.gpt-oss-*`) → `Responses`, other models
       with a Bedrock OpenAI surface → `ChatCompletions`, and every other
       model → `Converse`. Allow a per-model config override, because animist
       never infers the protocol. An unsupported protocol is not retried on
       another one.
     - `BedrockConfig::protocol` is per provider instance, so the registry
       holds one `Bedrock` per protocol actually selected, built lazily and
       cached, all from the same config.
     - Converse rejects generic thinking settings other than `Off`. For
       Converse models, map craft's thinking preference to `Off` and surface
       model-specific settings only through an explicit config field passed
       to `converse_additional_model_request_fields`.
3. **Catalog.** Keep `models_dev.rs`'s 24h disk cache and its
   tolerant-of-bad-entries policy (see the module docs: one unknown field must
   never cost the whole table). Parse into `animist::catalog::Catalog`, or keep
   craft's tolerant parser and convert to `ModelInfo`. Prefer animist's parser
   only if it is at least as tolerant; check that with craft's existing
   fixtures.
   - Feed `ProviderCatalog` into provider configs, so animist's `list_models`
     merges metadata.
   - Keep longest-prefix lookup for dated ids.
4. **Model handle.** Define `providers::ModelHandle` (the name is up to you). It
   is a cheap-clone wrapper around `animist::provider::Model`, plus craft-only
   metadata:
   - the `provider/model` spec label;
   - the `ModelConfig` settings;
   - the effective context length;
   - the catalog key used for pricing.

   Map the craft model overrides (context length, max output, thinking support,
   vision, fast) onto `ModelInfo::config` and `Overrides`. Call
   `Provider::adjust_model` once at construction.
5. **Async construction.** Provider construction, discovery and selection are
   `async fn`s end to end (README → Async native):
   - `Bedrock::new` is awaited directly.
   - Delete `sdk_config_blocking` (its `block_in_place`, private-runtime and
     scoped-thread fallbacks) with the rest of craft's Bedrock SDK chain, and
     `block_on_auth`, which leaves with the OAuth kinds.
   - Update the 02 builder's callers in this prompt to await. 03 converts the
     live callers (`runtime.rs`, `model_selection.rs`, TUI/ACP model pickers)
     as it switches over.
6. **Discovery and selection.** Port `list_models` aggregation, and the
   `model_registry` tier table keyed by kind string, to the new kinds.
   `model_selection` keeps its behaviour for every supported kind. Delete
   static tier entries for dropped kinds.
7. **Reauth and usage.** Turn `reauth.rs`'s "rebuild provider" into
   `Provider::reload_auth()` where that is enough. For Bedrock it always is:
   `reload_auth` rebuilds and validates animist's default credential chain. Keep a rebuild fallback for
   config changes. `usage_fetch.rs`: delete DeepSeek and keep the
   `Ok(None)`-for-unsupported contract.

## Tests

- Per kind: construct a provider from a representative `ProviderConfig` with an
  injected credential lookup (no process-env mutation; reuse the
  `from_config_with` pattern).
- Each dropped kind gives its actionable error.
- `list_models` against a local mock `/models` endpoint for `openai-compatible`,
  `ollama` and `llama-cpp`.
- Bedrock: tests build through `Bedrock::from_sdk_config` with a static-
  credentials `SdkConfig` (a test-only seam in the craft builder; production
  always uses `Bedrock::new`), plus local `endpoint_url` and
  `control_plane_endpoint_url`. Pin:
  - protocol selection per model id (Claude → Messages, `openai.*` →
    Responses, an unknown model → Converse, a configured override wins);
  - one cached provider instance per protocol;
  - control-plane listing against a mock `/foundation-models` and paginated
    `/inference-profiles`, with craft's enrichment applied.
- Construction under `#[tokio::test(flavor = "current_thread")]`, the case
  that deadlocked without the old blocking bridge.
- Catalog: existing models.dev fixtures parse with the same skip counts.
- The `ModelHandle` override mapping is pinned for context length, thinking
  and vision.

## Out of scope

Wiring the new handle into `run/`. The rig registry stays the live path until
03.

## Acceptance

- Every supported kind builds, and every dropped kind fails with guidance.
- The user-facing configs in `agent.example.bml` and the shapes in the README
  provider table all build: `openai-compatible` for Synthetic, Z.AI and a
  localhost server, plus Anthropic, OpenAI, Ollama and Bedrock.
- The build and all tests pass, and no rig code was removed yet.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note: the keyless rule chosen, the Bedrock protocol
   table, which `providers/bedrock.rs` helpers survived, and follow-ups.

## Completion note

_(fill in when done)_
