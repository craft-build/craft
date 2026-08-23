# Providers

Craft talks to LLM providers over their HTTP APIs. Models are split into three tiers: **weak** (cheap and fast), **medium** (balanced), and **strong** (highest capability, highest cost). There is also a **compaction** tier for choosing a dedicated model to summarize context when the conversation grows long.

Open the model picker with `/model` and press `!`, `@`, `#`, or `$` on any row to assign it to strong, medium, weak, or compaction. Press the same key again to remove the assignment. Your overrides are saved to `~/.local/state/craft/model-tiers` and apply across sessions.

## Authentication

Craft supports several ways to authenticate with providers. Run `craft auth login` to set up a provider interactively. It will prompt for the provider, API key, and any plan or host URL if needed.

Run `craft auth status` to see which providers are configured. A green check means stored credentials, a yellow tilde means an env var is set, and a red cross means no auth was found.

Run `craft auth logout <provider>` to remove stored credentials.

### API Key

Most providers use a simple API key. During `craft auth login`, Craft opens the provider's key page in your browser and asks you to paste the key. Keys are stored in `~/.local/state/craft/credentials/` and are never logged.

You can also skip the login prompt and set the key via the provider's env var. See each provider below for the exact variable name.

### OAuth Device Flow

OpenAI supports OAuth via device code flow. Running `craft auth login openai` opens a browser URL, shows a code to enter, and polls for authorization. Tokens are stored securely and refreshed automatically when they expire.

### Copilot Token Discovery

Copilot does not need a separate login if you already use GitHub Copilot. Craft looks for tokens in this order:

1. `GH_COPILOT_TOKEN` or `COPILOT_GITHUB_TOKEN` env var
2. Stored credentials from `craft auth login copilot`
3. `~/.config/github-copilot/hosts.json` or `apps.json`
4. `~/.config/gh/hosts.yml`

### Auth Reloading

Craft re-reads auth from storage and environment variables each time a new agent spawns (`/new`, retry, session load). If you run `craft auth login` in another terminal or change an env var, the next session picks it up without a restart.

You can set multiple API keys in one env var (`ANTHROPIC_API_KEY=sk-1,sk-2,sk-3`) and they rotate automatically on rate-limit or auth errors.

## Base URL Overrides

Every provider honors a `<SLUG>_BASE_URL` env var (`anthropic` -> `ANTHROPIC_BASE_URL`, `llama-cpp` -> `LLAMA_CPP_BASE_URL`). Set it to the origin of a proxy or a compatible endpoint and Craft appends the API paths itself:

```sh
ANTHROPIC_BASE_URL=https://my-proxy.internal craft
```

It wins over `providers.toml` and built-in defaults. `ANTHROPIC_BASE_URL` and `OPENAI_BASE_URL` are the same names the official SDKs use, so an existing proxy setup carries over as is. Two exceptions: `OPENAI_BASE_URL` only redirects the platform API, never the ChatGPT Coding Plan backend; `XAI_BASE_URL` only redirects the public API-key endpoint, never the OAuth CLI proxy.

You can also set `base_url` for a built-in provider in `~/.config/craft/providers.toml`. It overrides the built-in default and loses to the env var above:

```toml
[openai]
base_url = "http://xxxx:1234/v1"
```

The built-in provider still owns the slug, so `protocol`, `api_key_env`, `discover_models` and `models` are ignored with a warning. Use a custom slug if you need those.

## Built-in Providers

### Anthropic

- **Env var**: `ANTHROPIC_API_KEY`
- **API**: `https://api.anthropic.com/v1/messages`
- **Features**: Prompt caching, thinking mode (adaptive/budgeted), advanced tool use

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **claude-haiku-4-5** (default) | $1.00 / $5.00 | 200K ctx / 64K out |
| Medium | **claude-sonnet-5** (default) | $2.00 / $10.00 | 200K ctx / 128K out |
| Strong | claude-opus-4-8 | $5.00 / $25.00 | 200K ctx / 128K out |
| Strong | **claude-opus-5** (default) | $5.00 / $25.00 | 200K ctx / 128K out |
| Strong | claude-fable-5 | $10.00 / $50.00 | 200K ctx / 128K out |

Defaults: claude-haiku-4-5 (weak), claude-sonnet-5 (medium), claude-opus-5 (strong)

Add `-1m` to any Claude model, like `claude-sonnet-4-6-1m`, to use the 1M token context window.

#### Amazon Bedrock

Craft ships a first-class `bedrock` provider that talks to Bedrock through the official AWS SDK (`ConverseStream`). Use it with a `bedrock/...` model spec:

```
bedrock/us.anthropic.claude-sonnet-4-6-20250514-v1:0
bedrock/anthropic.claude-opus-4-1-20250805-v1:0
```

The model id is a Bedrock inference profile id, passed through verbatim. Model discovery runs `ListInferenceProfiles`, so `/models` lists every active profile your AWS principal can see.

Auth uses the full AWS SDK credential chain, so any of these works:

| Method | Env vars |
|--------|----------|
| IAM credentials | `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (and optionally `AWS_SESSION_TOKEN`) |
| Credentials file | `AWS_PROFILE` (defaults to `default`), reads `~/.aws/credentials` |
| SSO / IMDS / web identity / container creds | resolved automatically by `aws-config` |

Set `AWS_REGION` to your preferred region (for example `us-east-1`).

> **Deprecated:** the older `CLAUDE_CODE_USE_BEDROCK=1` env var still works (it swaps the `anthropic` provider onto Bedrock via hand-rolled SigV4), but new users should prefer the `bedrock/...` provider above. The legacy path does not support SSO, IMDS, or web identity and will not receive new features.

### OpenAI

- **Env var**: `OPENAI_API_KEY` (also supports OAuth device flow)
- **API**: `https://api.openai.com/v1`

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **gpt-5.6-luna** (default) | $1.00 / $6.00 | 372K ctx / 128K out |
| Weak | gpt-5.4-nano | $0.20 / $1.25 | 400K ctx / 128K out |
| Weak | gpt-5.4-mini | $0.75 / $4.50 | 400K ctx / 128K out |
| Weak | gpt-4.1-nano | $0.10 / $0.40 | 1047K ctx / 32K out |
| Medium | **gpt-5.6-terra** (default) | $2.50 / $15.00 | 372K ctx / 128K out |
| Medium | gpt-4.1-mini | $0.40 / $1.60 | 1047K ctx / 32K out |
| Medium | gpt-4.1 | $2.00 / $8.00 | 1047K ctx / 32K out |
| Medium | o4-mini | $1.10 / $4.40 | 200K ctx / 100K out |
| Medium | gpt-5.1-codex-mini | $0.25 / $2.00 | 400K ctx / 128K out |
| Strong | **gpt-5.6-sol** (default) | $5.00 / $30.00 | 372K ctx / 128K out |
| Strong | gpt-5.5 | $5.00 / $30.00 | 1050K ctx / 128K out |
| Strong | gpt-5.4 | $2.50 / $15.00 | 1050K ctx / 128K out |
| Strong | o3 | $2.00 / $8.00 | 200K ctx / 100K out |
| Strong | gpt-5.3-codex | $1.75 / $14.00 | 400K ctx / 128K out |
| Strong | gpt-5.2-codex | $1.75 / $14.00 | 400K ctx / 128K out |
| Strong | gpt-5.1-codex-max | $1.25 / $10.00 | 400K ctx / 128K out |
| Strong | gpt-5.1-codex | $1.25 / $10.00 | 400K ctx / 128K out |

Defaults: gpt-5.6-luna (weak), gpt-5.6-terra (medium), gpt-5.6-sol (strong)

### Google

- **Env var**: `GEMINI_API_KEY`
- **API**: `https://generativelanguage.googleapis.com/v1beta`
- **Features**: Native Gemini API with thinking support

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **gemini-2.0-flash-lite** (default) | $0.07 / $0.30 | 1048K ctx / 65K out |
| Medium | **gemini-2.5-flash** (default) | $0.15 / $0.60 | 1048K ctx / 65K out |
| Strong | **gemini-2.5-pro** (default) | $1.25 / $5.00 | 1048K ctx / 65K out |

Defaults: gemini-2.5-pro (strong), gemini-2.5-flash (medium), gemini-2.0-flash-lite (weak)

### Copilot

- **Env var**: `GH_COPILOT_TOKEN` (or run `craft auth login copilot` to import a token from gh CLI, the Copilot client, or the system keyring)
- **API**: `https://api.githubcopilot.com (or GraphQL-discovered Copilot API endpoint)`
- **Features**: Native Copilot Chat HTTP API with model endpoint discovery

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | gpt-5-mini | $0.25 / $2.00 | 200K ctx / 100K out |
| Weak | gpt-5.4-mini | $0.75 / $4.50 | 200K ctx / 100K out |
| Weak | gpt-5.4-nano | $0.20 / $1.25 | 200K ctx / 100K out |
| Weak | claude-haiku-4.5 | $1.00 / $5.00 | 200K ctx / 64K out |
| Weak | gemini-3.5-flash | $1.50 / $9.00 | 200K ctx / 65K out |
| Weak | mai-code-1-flash-picker | $0.75 / $4.50 | 200K ctx / 100K out |
| Weak | **gpt-5.6-luna** (default) | $0.20 / $1.20 | 200K ctx / 100K out |
| Medium | gemini-3.6-flash | $0.75 / $3.75 | 200K ctx / 65K out |
| Medium | gemini-3.7-flash | $0.75 / $3.75 | 200K ctx / 65K out |
| Medium | claude-sonnet-4.5, claude-sonnet-4.6 | $3.00 / $15.00 | 200K ctx / 64K out |
| Medium | claude-sonnet-5 | $2.00 / $10.00 | 200K ctx / 100K out |
| Medium | kimi-k2.7-code | $0.95 / $4.00 | 200K ctx / 100K out |
| Medium | gemini-3.1-pro-preview | $2.00 / $12.00 | 200K ctx / 65K out |
| Medium | **gpt-5.6-terra** (default) | $2.00 / $12.00 | 200K ctx / 100K out |
| Medium | grok-4.5 | $2.00 / $6.00 | 200K ctx / 100K out |
| Medium | grok-4.6 | $2.00 / $6.00 | 200K ctx / 100K out |
| Strong | gpt-5.5 | $5.00 / $30.00 | 200K ctx / 100K out |
| Strong | kimi-k3 | $3.00 / $15.00 | 200K ctx / 100K out |
| Strong | gpt-5.4 | $2.50 / $15.00 | 200K ctx / 100K out |
| Strong | gpt-5.6-sol | $5.00 / $30.00 | 200K ctx / 100K out |
| Strong | gpt-5.3-codex | $1.75 / $14.00 | 200K ctx / 100K out |
| Strong | **claude-opus-5, claude-opus-4.8, claude-opus-4.7, claude-opus-4.6, claude-opus-4.5** (default) | $5.00 / $25.00 | 200K ctx / 64K out |
| Strong | claude-opus-4.8-fast, claude-fable-5 | $10.00 / $50.00 | 200K ctx / 100K out |

Defaults: gpt-5.6-luna (weak), gpt-5.6-terra (medium), claude-opus-5 (strong)

### Ollama

- **Env var**: `OLLAMA_HOST` for local/remote (e.g. `http://localhost:11434`), `OLLAMA_API_KEY` for auth
- **API**: `http://localhost:11434/v1`
- **Features**: Local or remote inference via OLLAMA_HOST, cloud fallback via OLLAMA_API_KEY

Craft asks the server for the list of installed models, so there's no built-in catalog. Tiers are guessed from list order: the first model becomes strong, the second medium, and the rest weak.

### LlamaCpp

- **Env var**: `LLAMA_CPP_API_KEY`
- **API**: `http://localhost:8080/v1`
- **Features**: Local or remote inference via LLAMA_CPP_HOST, set optional key via LLAMA_CPP_API_KEY

Connects to any OpenAI-compatible `/v1` endpoint. Craft asks the server for the list of installed models, so there's no built-in catalog. Tiers are guessed from list order: the first model becomes strong, the second medium, and the rest weak.

### Mistral

- **Env var**: `MISTRAL_API_KEY`
- **API**: `https://api.mistral.ai/v1`

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **ministral-14b-latest, ministral-14b-2512** (default) | $0.20 / $0.20 | 262K ctx |
| Medium | **mistral-small-latest, mistral-small-2603** (default) | $0.15 / $0.60 | 262K ctx |
| Strong | **mistral-medium-latest, mistral-medium-3.5, mistral-medium-3-5, mistral-medium-2604** (default) | $1.50 / $7.50 | 262K ctx |
| Strong | glm-5-2, zai-glm-5-2 | $1.40 / $4.40 | 1000K ctx |

Defaults: mistral-medium-latest (strong), mistral-small-latest (medium), ministral-14b-latest (weak)

### DeepSeek

- **Env var**: `DEEPSEEK_API_KEY`
- **API**: `https://api.deepseek.com`
- **Features**: Thinking mode toggle (on/off), open-weight models
- **Peak pricing**: the prices below are off-peak; each turn is billed as it happens, at 2x during 01:00-04:00, 06:00-10:00 UTC

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Medium | **deepseek-v4-flash** (default) | $0.22 / $0.66 | 1000K ctx / 384K out |
| Strong | **deepseek-v4-pro** (default) | $0.66 / $1.98 | 1000K ctx / 384K out |

Defaults: deepseek-v4-flash (medium), deepseek-v4-pro (strong)

### OpenRouter

- **Env var**: `OPENROUTER_API_KEY`
- **API**: `https://openrouter.ai/api/v1`
- **Features**: 300+ models from all providers, prompt caching, provider routing

OpenRouter aggregates models from many providers behind a single API. Craft asks the OpenRouter API for the list of available models, so there's no built-in catalog. Tiers are guessed from list order: the first model becomes strong, the second medium, and the rest weak.

### Synthetic

- **Env var**: `SYNTHETIC_API_KEY`
- **API**: `https://api.synthetic.new/openai/v1`
- **Features**: Reasoning effort support (low/medium/high), open-weight models

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | syn:small:vision, hf:Qwen/Qwen3.6-27B | $0.45 / $3.60 | 256K ctx / 131K out |
| Weak | **syn:small:text, hf:zai-org/GLM-4.7-Flash** (default) | $0.10 / $0.50 | 200K ctx / 131K out |
| Medium | **hf:MiniMaxAI/MiniMax-M3** (default) | $0.60 / $1.20 | 512K ctx / 131K out |
| Strong | **syn:large:vision, hf:moonshotai/Kimi-K3** (default) | $0.95 / $4.00 | 512K ctx / 131K out |
| Strong | syn:large:text, hf:zai-org/GLM-5.2 | $1.40 / $4.40 | 512K ctx / 131K out |

Defaults: hf:MiniMaxAI/MiniMax-M3 (medium), syn:large:vision (strong), syn:small:text (weak)

### TensorX

- **Env var**: `TENSORX_API_KEY`
- **API**: `https://api.tensorx.ai/v1`
- **Features**: Open-weight models, zero data retention, prompt caching

Craft asks the server for the list of installed models, so there's no built-in catalog. Tiers are guessed from list order: the first model becomes strong, the second medium, and the rest weak.

### Opencode Zen

- **Env var**: `OPENCODE_API_KEY`
- **API**: `https://opencode.ai/zen/v1`
- **Features**: Dynamically discovered models via [models.dev](https://models.dev/) + all the models provided by Opencode Zen API

Models are discovered dynamically from the [models.dev](https://models.dev/) catalog and the Opencode Zen API, so there's no built-in catalog. Use any model id the catalog exposes, prefixed with the sub-provider (e.g. `opencode/<sub-provider>/<model-id>`).

By default Craft hides free models from the Opencode catalog. To list free models (they use a public fallback, no API key needed), add this to `~/.config/craft/providers.toml`:

```toml
[opencode]
enable_free_models = true
```

The default is `false`.

### xAI

- **Env var**: `XAI_API_KEY` (also supports OAuth via `craft auth login xai`)
- **API endpoints**:
  - `https://api.x.ai/v1`
  - `https://cli-chat-proxy.grok.com/v1`
- **Features**: OAuth login, account-specific model catalog, Grok reasoning (low/medium/high/xhigh)

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Medium | **grok-4.3** (default) | $1.25 / $2.50 | 1000K ctx / 131K out |
| Strong | **grok-4.6** (default) | $2.00 / $6.00 | 500K ctx / 131K out |
| Strong | grok-4.5 | $2.00 / $6.00 | 500K ctx / 131K out |

Defaults: grok-4.6 (strong), grok-4.3 (medium)

OAuth uses the same first-party xAI client as the official Grok CLI (`craft auth login xai`). Browser login (PKCE) is the desktop default; device code is recommended over SSH or in a container. Tokens refresh automatically. After login, Craft fetches your account catalog from `GET /v1/models-v2` on the Grok CLI proxy and caches it for 15 minutes. `XAI_BASE_URL` only redirects the public API-key endpoint, never the OAuth proxy.

If `~/.grok/auth.json` already exists, login offers to reuse it without writing that file.

### Bedrock

- **Env var**: `AWS_REGION`
- **API**: `https://bedrock-runtime.<region>.amazonaws.com (AWS SDK)`
- **Features**: AWS SDK auth (SSO/IMDS/profiles/env), ConverseStream, inference-profile discovery

Craft asks the server for the list of installed models, so there's no built-in catalog. Tiers are guessed from list order: the first model becomes strong, the second medium, and the rest weak.

### Aperture

- **Env var**: `APERTURE_HOST` (e.g. `https://your-host.tailnet.ts.net`)
- **API**: `Aperture gateway (set APERTURE_HOST)`
- **Features**: Tailscale Aperture LLM gateway; set APERTURE_HOST or configure in providers.toml

Aperture discovers models from your gateway. Set `APERTURE_HOST` to your Tailscale Aperture endpoint (e.g. `https://your-host.tailnet.ts.net`). No API key needed, Tailscale handles auth.

### Opencode Go

- **Env var**: `OPENCODE_API_KEY`
- **API**: `https://opencode.ai/zen/go/v1`
- **Features**: Dynamically discovered models via [models.dev](https://models.dev/) + all the models provided by Opencode Go API

No hardcoded model catalog. Use any model ID supported by this provider. An API key is required.

## Model Identifiers

Models are referenced as `provider/model_id`:

```
anthropic/claude-sonnet-4-6
openai/gpt-4.1
xai/grok-4.6
```

If the model name is unique across providers, the prefix can be omitted.

## Custom Providers

You can add providers that are not built in by editing `~/.config/craft/providers.toml`. Custom providers use one of three supported protocols: `openai`, `anthropic`, or `google`.

### Configuration Shape

Each entry under `providers.toml` is a table keyed by the provider slug:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `display_name` | string | No | Human readable name shown in the UI |
| `protocol` | string | Yes | One of: `openai`, `anthropic`, `google` |
| `base_url` | string | Yes | API endpoint URL |
| `plan` | string | No | Plan name for providers with multiple plans |
| `api_key_env` | string | No | Env var name for the API key (defaults to `{SLUG}_API_KEY`) |
| `api_key` | string | No | API key stored inline (not recommended; use `craft auth login` instead) |
| `default_model` | string | No | Default model identifier without the provider prefix |
| `discover_models` | bool | No | Query the provider for model list at startup (default `false`) |
| `models` | array of tables | No | Override context window and max output for specific models |
| `overrides` | table | No | Aperture only. Per-upstream model overrides (see below) |

The `models` table is useful when a provider's `/models` endpoint does not report context sizes, or reports incorrect ones. Each entry has:

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Model identifier (without the provider prefix) |
| `context_window` | integer | Context window in tokens |
| `max_output_tokens` | integer | Max output tokens |

Craft tries three sources in priority order when resolving a custom model:

1. Explicit `models` entry in `providers.toml`
2. Metadata discovered from the provider's `/models` endpoint (when `discover_models = true`)
3. Protocol fallback values

### Example

```toml
[my-proxy]
protocol = "openai"
base_url = "https://api.my-proxy.com/v1"
api_key_env = "MY_PROXY_API_KEY"
discover_models = true

[[my-proxy.models]]
id = "glm-5.2"
context_window = 1_000_000
max_output_tokens = 32_768
```

Use the provider with:

```
craft -m my-proxy/gpt-4.1
```

Custom providers appear in `craft auth login` and the model picker just like built-in ones.

### Aperture overrides

Aperture proxies upstream providers, exposing each model as `aperture/<upstream>/<model>`. Overrides keyed by upstream provider id live under `[aperture.overrides]`:

```toml
[aperture.overrides.llmserver]
base = "llama-cpp"
context_window = 131072
max_output_tokens = 16384

[aperture.overrides.llmserver.models."qwen-3.6"]
context_window = 262144
supports_vision = true
```

Provider-level fields apply to every model from that upstream; per-model entries under `models` win field by field. Fields: `context_window`, `max_output_tokens`, `supports_thinking`, `supports_vision`, `base` (remaps an opaque vendor to a native provider; e.g. `llama-cpp`, `google`, `anthropic`), and `path_prefix`. Model ids containing dots must be quoted (`"qwen3.6"`) since TOML treats a bare dotted key as a nested table.

Craft sends `/v1` (or `/v1beta` for Gemini routes, nothing for Anthropic), and Aperture appends that path to the upstream's base url. If an upstream base url already carries its own path, set `path_prefix = ""` for it to avoid a doubled path.

## Dynamic Providers

To add a provider proxy via an executable script, drop it into `~/.config/craft/providers/`. The script must handle these subcommands:

| Subcommand | Timeout | What it does |
|------------|---------|--------|
| `info` | 5s | Return JSON with `display_name`, `base` provider, `has_auth` |
| `models` | 5s | Return JSON array of model entries (optional) |
| `resolve` | 30s | Return auth JSON (`base_url`, `headers`) |
| `login` | interactive | OAuth or credential flow |
| `logout` | interactive | Clear credentials |
| `refresh` | 30s | Refresh auth tokens |

`resolve` is called each time a new agent spawns, so scripts should read tokens from disk instead of caching them in memory. That way auth changes from other processes get picked up.

The `base` field specifies which built-in provider to inherit the model catalog from. Valid values: `anthropic`, `openai`, `google`, `copilot`, `ollama`, `llama-cpp`, `mistral`, `deepseek`, `openrouter`, `synthetic`, `tensorx`, `opencode`, `xai`, `bedrock`, `aperture`.

If your provider serves models not in the base catalog, add a `models` subcommand returning:

```json
[{"id": "my-model-v2", "tier": "strong", "context_window": 200000, "max_output_tokens": 16384}]
```

Only `id` is required. Optional fields: `tier` (default `medium`), `context_window` (128K), `max_output_tokens` (16K), `pricing` (`{input, output, cache_write, cache_read}`, all per 1M tokens), `supports_tool_examples` (defaults to the base provider's setting), `supports_thinking` (defaults to the base provider's setting), `requires_thinking` (default false; for APIs that reject requests with thinking off, raises it to minimal effort and implies `supports_thinking`), `supports_vision` (defaults to the base provider's setting; when false, image input and the `view_image` tool are disabled). The first model listed per tier is used for sub-agents. Without this subcommand, the base provider's models are used.

A `llama-cpp` model can replace Craft's token-budget mapping with its native thinking fields. Each thinking mode maps to a JSON fragment merged into the request body:

```json
[{
  "id": "reasoning-model",
  "supports_thinking": true,
  "thinking_fields": {
    "off": {"reasoning_effort": "none"},
    "adaptive": {"reasoning_effort": "medium"},
    "low": {"reasoning_effort": "low"},
    "medium": {"reasoning_effort": "medium"},
    "xhigh": {"reasoning_effort": "xhigh"}
  }
}]
```

`off` is used when thinking is off, `adaptive` when thinking is on without a chosen level. Any other key is an effort level, one of `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. The levels you declare are the ones the model accepts: whatever you ask for snaps into them, downwards first, so a level the model never advertised is never sent. Every part is optional.

Fragments are merged into the body, so nesting works too. A template toggle is just a fragment:

```json
"thinking_fields": {
  "off": {"chat_template_kwargs": {"enable_thinking": false}},
  "adaptive": {"chat_template_kwargs": {"enable_thinking": true}}
}
```

Named modes send only these fields, no token budget. An explicit `/thinking <budget>` snaps into the levels you declared; a model that declares none gets the `adaptive` fragment plus `thinking_budget_tokens`. Any mode you left undeclared falls back to the usual `thinking_budget_tokens` mapping, so no request ever ends up saying nothing. Models without `thinking_fields` keep the existing llama.cpp behavior.

Dynamic provider models are namespaced as `{slug}/{model_id}` (e.g. `myproxy/claude-sonnet-4-6`).

### Script Name Rules

- Must start with a letter or digit
- Only letters, digits, underscores, and hyphens after that
- Can't reuse a built-in provider's slug
- Must be executable
