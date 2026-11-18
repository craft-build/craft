# Providers

Providers are configured by alias in the BML config; each `[providers.<alias>]` chooses a `kind`. Multiple aliases may use the same kind with different credentials, URLs, and model catalogs.

`api_key_env` names an environment variable, **not the key itself** — keep secrets in your shell or secret manager. With overrides, `api_key_env` defaults to the provider's usual key variable (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, ...).

Supported kinds: `anthropic`, `openai`, `openai-compatible`, `synthetic`, `zai-coding-plan`, `ollama`, `llama-cpp`, `omlx`, `amazon-bedrock`.

## anthropic

```bml
provider "anthropic" {
  kind = "anthropic"
  api_key_env = "ANTHROPIC_API_KEY"
}
```

Accepts `base_url` for Anthropic-compatible services — supply the complete base URL including any path prefix.

## openai

Speaks the Responses API by default; `protocol = "chat_completions"` selects Chat Completions. Both accept `base_url`, plus `organization` and `project`.

```bml
provider "openai" {
  kind = "openai"
  api_key_env = "OPENAI_API_KEY"
}
```

## openai-compatible

Any OpenAI-dialect endpoint, over Chat Completions:

```bml
provider "deepseek" {
  kind = "openai-compatible"
  base_url = "https://api.deepseek.com/v1"
  api_key_env = "DEEPSEEK_API_KEY"
}
```

A **keyless local endpoint** (loopback or private host, no key configured) automatically rides animist's keyless llama.cpp client — local llama.cpp, vLLM, and LM Studio servers work with no dummy key. Non-local hosts require a key.

This is also the migration path for every dropped vendor kind (below).

## ollama

Local models, no credential needed:

```bml
provider "ollama" {
  kind = "ollama"
}
```

## llama-cpp

Local llama.cpp server. Accepts an optional key; also serves System One decisions on `/systemone`.

```bml
provider "llamacpp" {
  kind = "llama-cpp"
  base_url = "http://localhost:9931/v1"
}
```

## omlx

Local omlx server; thinking knobs map to `enable_thinking`/`thinking_budget`.

```bml
provider "omlx" {
  kind = "omlx"
  base_url = "http://localhost:8000/v1"
  api_key_env = "OMLX_API_KEY"   # optional
}
```

## synthetic

Hosted Synthetic coding plan; default env `SYNTHETIC_API_KEY`. Also accepts `base_url` and `max_output_tokens`.

```bml
provider "synthetic" {
  kind = "synthetic"
}
```

## zai-coding-plan

Hosted ZAI coding plan; default env `ZHIPU_API_KEY`. Existing `openai-compatible` configs pointing at its endpoints keep working.

```bml
provider "zai" {
  kind = "zai-coding-plan"
}
```

## amazon-bedrock

Feature `bedrock`. Authenticates through the AWS default credential chain (env vars, `~/.aws` profiles incl. `AWS_PROFILE`, SSO, assume-role, `credential_process`, web identity, container/IMDS roles) and takes its region from `AWS_REGION`/`AWS_DEFAULT_REGION`, the active profile, or `region` on the provider. Also accepts `profile`, `endpoint_url`, `protocol`, `max_attempts` (in-provider 429/503 retries, default 3), `anthropic_version`, `anthropic_beta`. Model ids are Bedrock model ids or inference-profile ARNs.

| Model family | Protocol | Wire |
| --- | --- | --- |
| Claude (`anthropic.*` or ids containing `claude`) | `anthropic_messages` | `/model/{id}/invoke-with-response-stream` |
| `openai.*` deployments | `responses` | `/openai/v1/responses` |
| other OpenAI-surface models (`deepseek.*`, `moonshotai.*`) | `chat_completions` | `/openai/v1/chat-completions` |
| everything else | `converse` | `/model/{id}/converse-stream` |

A per-model `protocol` override wins (`protocol = "converse"` in the model block). **Converse caveat:** reasoning configs other than off are rejected on Converse — Claude-on-Bedrock thinking requires the `anthropic_messages` protocol.

## Model catalogs

`[providers.<alias>.models."<exact-model-id>"]` declares or partially overrides a model:

```bml
model "my-coding-model" {
  context_length = 32768
}
```

Overridable metadata: `name`, `description`, `context_length`, `max_output_tokens`, `supports_thinking`, `requires_thinking`, `protocol`, `reasoning_options` (e.g. `[{ type = "effort", values = ["low", "high"] }]`). Local backends can declare `thinking_fields` with JSON-shaped BML fragments keyed by `off`, `adaptive`, or effort level; those fragments are deep-merged into the request.

Discovery merges configured models with live provider listings and the [models.dev](https://models.dev) catalog (disk-cached 24 h), sorted by ID; `discover_models = false` disables listing. Discovery errors propagate rather than silently yielding an empty catalog. Token limits are catalog metadata — they feed compaction thresholds and usage display, not request parameters. Set `[agent].max_tokens` for an output cap.

Unknown models fall back to provider/family dialects; discovery is advisory, not a reason to reject an otherwise usable model ID. `craft models` lists what a config resolves to.

## Dropped kinds

The predecessor's vendor-specific kinds are **dropped** (unsupported since the animist cutover): `azure`, `chatgpt`, `cohere`, `copilot`, `deepseek`, `doubleword`, `gemini`, `groq`, `huggingface`, `hyperbolic`, `llamafile`, `minimax`, `mira`, `mistral`, `moonshot`, `openrouter`, `perplexity`, `together`, `venice`, `voyageai`, `xai`, `xiaomimo`, `zai`. A config naming one fails with an actionable error; the OAuth/device flows (`chatgpt`, `copilot`) are gone with their kinds.

Migration for any OpenAI-dialect vendor is to point `openai-compatible` at its base URL:

```bml
# before: kind = "deepseek"
provider "deepseek" {
  kind = "openai-compatible"
  base_url = "https://api.deepseek.com/v1"
  api_key_env = "DEEPSEEK_API_KEY"
}
```
