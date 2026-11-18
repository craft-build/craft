# Providers

Add a `provider "alias"` block to your [BML configuration](./configuration.md). The alias is a name you choose; `kind` selects the API implementation. Multiple aliases can use the same kind with different credentials, URLs, or model metadata.

`api_key_env` names an environment variable, **not the key itself**. Keep secrets in your shell or secret manager. Built-in hosted providers use their usual key variable when it is omitted, such as `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`. For a custom service, set the variable name explicitly.

## Choose a provider

- **Hosted APIs:** use `anthropic` or `openai` for those services, or `openai-compatible` for another service that implements Chat Completions.
- **Local models:** use `ollama`, `llama-cpp`, `omlx`, or a keyless local `openai-compatible` endpoint.
- **Coding plans:** use `synthetic` or `zai-coding-plan`.
- **AWS:** use `amazon-bedrock` with a build that enables the `bedrock` feature.

Select a model with `craft --model alias/model-id`, or use `/model` in the TUI. `craft models` lists the catalog resolved from your configuration.

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

A **keyless local endpoint** (loopback or private host, no key configured) works without a dummy key. This supports local llama.cpp, vLLM, and LM Studio servers. Non-local hosts require a key.

For example:

```bml
provider "local" {
  kind = "openai-compatible"
  base_url = "http://localhost:8080/v1"
}
```

This is also the migration path for former vendor-specific kinds when the service offers an OpenAI-compatible API; see [Migrating older configurations](#migrating-older-configurations).

## ollama

Local models, no credential needed:

```bml
provider "ollama" {
  kind = "ollama"
}
```

## llama-cpp

Connect to a local llama.cpp server. An API key is optional. The integration also supports System One decision requests on `/systemone`.

```bml
provider "llamacpp" {
  kind = "llama-cpp"
  base_url = "http://localhost:9931/v1"
}
```

## omlx

Connect to a local omlx server. Craft maps thinking preferences to the server's `enable_thinking` and `thinking_budget` fields.

```bml
provider "omlx" {
  kind = "omlx"
  base_url = "http://localhost:8000/v1"
  api_key_env = "OMLX_API_KEY"   # optional
}
```

## synthetic

Connect to a hosted Synthetic coding plan. The default key variable is `SYNTHETIC_API_KEY`. You can also set `base_url` and `max_output_tokens`.

```bml
provider "synthetic" {
  kind = "synthetic"
}
```

## zai-coding-plan

Connect to a hosted ZAI coding plan. The default key variable is `ZHIPU_API_KEY`. Existing `openai-compatible` configurations pointing at its endpoints also work.

```bml
provider "zai" {
  kind = "zai-coding-plan"
}
```

## amazon-bedrock

Requires a build with the `bedrock` feature enabled. Use Bedrock model IDs or inference-profile ARNs as model IDs.

```bml
provider "bedrock" {
  kind = "amazon-bedrock"
  region = "us-east-1"
}
```

### AWS authentication and region

Authentication uses the AWS default credential chain: environment variables, `~/.aws` profiles (including `AWS_PROFILE`), SSO, assume-role, `credential_process`, web identity, or container/IMDS roles.

Set the region through `AWS_REGION`, `AWS_DEFAULT_REGION`, the active profile, or the provider's `region` setting. Set `profile` to choose a profile and `endpoint_url` to override the endpoint.

### Protocol selection and retries

Craft selects a protocol by model family:

| Model family | Protocol | Wire |
| --- | --- | --- |
| Claude (`anthropic.*` or ids containing `claude`) | `anthropic_messages` | `/model/{id}/invoke-with-response-stream` |
| `openai.*` deployments | `responses` | `/openai/v1/responses` |
| other OpenAI-surface models (`deepseek.*`, `moonshotai.*`) | `chat_completions` | `/openai/v1/chat-completions` |
| everything else | `converse` | `/model/{id}/converse-stream` |

A per-model `protocol` override wins (`protocol = "converse"` in the model block). **Converse caveat:** reasoning configs other than off are rejected on Converse — Claude-on-Bedrock thinking requires the `anthropic_messages` protocol.

You can also set provider-level `protocol`, `anthropic_version`, and `anthropic_beta`. `max_attempts` controls Bedrock's internal retries for HTTP 429/503 responses; its default is `3`.

## Model catalogs

Nest a `model "exact-model-id"` block inside its provider to declare a model or override some of its metadata:

```bml
provider "local" {
  kind = "openai-compatible"
  base_url = "http://localhost:8080/v1"

  model "my-coding-model" {
    context_length = 32768
  }
}
```

### Model metadata

- `name` and `description` set display text.
- `context_length` sets the catalog context window used for compaction and usage display.
- `max_output_tokens` describes the model's output limit. To set a request-level output cap, use `max_tokens` inside the `agent` block.
- `supports_thinking` and `requires_thinking` describe reasoning support.
- `protocol` overrides the provider's protocol for this model.
- `reasoning_options` declares reasoning controls, for example `[{ type = "effort", values = ["low", "high"] }]`.
- `thinking_fields` lets local backends supply JSON-shaped BML fragments keyed by `off`, `adaptive`, or an effort level. The selected fragment is deep-merged into the request.

### Discovery

Discovery merges configured models with live provider listings and the [models.dev](https://models.dev) catalog, then sorts by ID. The catalog is cached on disk for 24 hours. Set `discover_models = false` in a provider block to disable live listing. Discovery failures are reported as errors rather than shown as an empty catalog.

Unknown models fall back to provider/family dialects; discovery is advisory, not a reason to reject an otherwise usable model ID. `craft models` lists what a config resolves to.

## Migrating older configurations

The following legacy `kind` values are no longer supported: `azure`, `chatgpt`, `cohere`, `copilot`, `deepseek`, `doubleword`, `gemini`, `groq`, `huggingface`, `hyperbolic`, `llamafile`, `minimax`, `mira`, `mistral`, `moonshot`, `openrouter`, `perplexity`, `together`, `venice`, `voyageai`, `xai`, `xiaomimo`, `zai`. A configuration using one fails with an error. The former `chatgpt` and `copilot` OAuth/device flows are not available.

If the service offers an OpenAI-compatible API, use `openai-compatible` with its base URL and API key. This does not replace unsupported authentication flows or APIs:

```bml
# before: kind = "deepseek"
provider "deepseek" {
  kind = "openai-compatible"
  base_url = "https://api.deepseek.com/v1"
  api_key_env = "DEEPSEEK_API_KEY"
}
```
