use craft_providers::Effort;
use craft_providers::manifest::ManifestRegistry;
use craft_providers::model::{ModelEntry, ModelTier};
use craft_providers::provider::ProviderKind;
use std::fmt::Write;
use strum::IntoEnumIterator;

const TIER_PICKER_NOTE: &str = r#"Open the model picker with `/model` and press `!`, `@`, `#`, or `$` on any row to assign it to strong, medium, weak, or compaction. Press the same key again to remove the assignment. Your overrides are saved to `~/.local/state/craft/model-tiers` and apply across sessions."#;

const AUTH_SECTION: &str = r#"## Authentication

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

You can set multiple API keys in one env var (`ANTHROPIC_API_KEY=sk-1,sk-2,sk-3`) and they rotate automatically on rate-limit or auth errors."#;

const BASE_URL_OVERRIDES: &str = r#"## Base URL Overrides

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

The built-in provider still owns the slug, so `protocol`, `api_key_env`, `discover_models` and `models` are ignored with a warning. Use a custom slug if you need those."#;

const LONG_CONTEXT_NOTE: &str = r#"Add `-1m` to any Claude model, like `claude-sonnet-4-6-1m`, to use the 1M token context window."#;

const BEDROCK_NOTE: &str = r#"#### Amazon Bedrock

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

> **Deprecated:** the older `CLAUDE_CODE_USE_BEDROCK=1` env var still works (it swaps the `anthropic` provider onto Bedrock via hand-rolled SigV4), but new users should prefer the `bedrock/...` provider above. The legacy path does not support SSO, IMDS, or web identity and will not receive new features."#;

const XAI_OAUTH_NOTE: &str = r#"OAuth uses the same first-party xAI client as the official Grok CLI (`craft auth login xai`). Browser login (PKCE) is the desktop default; device code is recommended over SSH or in a container. Tokens refresh automatically. After login, Craft fetches your account catalog from `GET /v1/models-v2` on the Grok CLI proxy and caches it for 15 minutes. `XAI_BASE_URL` only redirects the public API-key endpoint, never the OAuth proxy.

If `~/.grok/auth.json` already exists, login offers to reuse it without writing that file."#;

const OPENCODE_FREE_MODELS_NOTE: &str = r#"By default Craft hides free models from the Opencode catalog. To list free models (they use a public fallback, no API key needed), add this to `~/.config/craft/providers.toml`:

```toml
[opencode]
enable_free_models = true
```

The default is `false`."#;

const OPENCODE_GO_SECTION: &str = r#"### Opencode Go

- **Env var**: `OPENCODE_API_KEY`
- **API**: `https://opencode.ai/zen/go/v1`
- **Features**: Dynamically discovered models via [models.dev](https://models.dev/) + all the models provided by Opencode Go API

No hardcoded model catalog. Use any model ID supported by this provider. An API key is required."#;

const MODEL_IDENTIFIERS: &str = r#"## Model Identifiers

Models are referenced as `provider/model_id`:

```
anthropic/claude-sonnet-4-6
openai/gpt-4.1
xai/grok-4.6
```

If the model name is unique across providers, the prefix can be omitted."#;

const CUSTOM_PROVIDERS_SECTION: &str = r#"## Custom Providers

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

Craft sends `/v1` (or `/v1beta` for Gemini routes, nothing for Anthropic), and Aperture appends that path to the upstream's base url. If an upstream base url already carries its own path, set `path_prefix = ""` for it to avoid a doubled path."#;

fn dynamic_providers_section() -> String {
    let valid_values: Vec<String> = ProviderKind::iter().map(|k| format!("`{k}`")).collect();
    let efforts: Vec<String> = Effort::ALL.iter().map(|e| format!("`{e}`")).collect();

    format!(
        r#"## Dynamic Providers

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

The `base` field specifies which built-in provider to inherit the model catalog from. Valid values: {}.

If your provider serves models not in the base catalog, add a `models` subcommand returning:

```json
[{{"id": "my-model-v2", "tier": "strong", "context_window": 200000, "max_output_tokens": 16384}}]
```

Only `id` is required. Optional fields: `tier` (default `medium`), `context_window` (128K), `max_output_tokens` (16K), `pricing` (`{{input, output, cache_write, cache_read}}`, all per 1M tokens), `supports_tool_examples` (defaults to the base provider's setting), `supports_thinking` (defaults to the base provider's setting), `requires_thinking` (default false; for APIs that reject requests with thinking off, raises it to minimal effort and implies `supports_thinking`), `supports_vision` (defaults to the base provider's setting; when false, image input and the `view_image` tool are disabled). The first model listed per tier is used for sub-agents. Without this subcommand, the base provider's models are used.

A `llama-cpp` model can replace Craft's token-budget mapping with its native thinking fields. Each thinking mode maps to a JSON fragment merged into the request body:

```json
[{{
  "id": "reasoning-model",
  "supports_thinking": true,
  "thinking_fields": {{
    "off": {{"reasoning_effort": "none"}},
    "adaptive": {{"reasoning_effort": "medium"}},
    "low": {{"reasoning_effort": "low"}},
    "medium": {{"reasoning_effort": "medium"}},
    "xhigh": {{"reasoning_effort": "xhigh"}}
  }}
}}]
```

`off` is used when thinking is off, `adaptive` when thinking is on without a chosen level. Any other key is an effort level, one of {}. The levels you declare are the ones the model accepts: whatever you ask for snaps into them, downwards first, so a level the model never advertised is never sent. Every part is optional.

Fragments are merged into the body, so nesting works too. A template toggle is just a fragment:

```json
"thinking_fields": {{
  "off": {{"chat_template_kwargs": {{"enable_thinking": false}}}},
  "adaptive": {{"chat_template_kwargs": {{"enable_thinking": true}}}}
}}
```

Named modes send only these fields, no token budget. An explicit `/thinking <budget>` snaps into the levels you declared; a model that declares none gets the `adaptive` fragment plus `thinking_budget_tokens`. Any mode you left undeclared falls back to the usual `thinking_budget_tokens` mapping, so no request ever ends up saying nothing. Models without `thinking_fields` keep the existing llama.cpp behavior.

Dynamic provider models are namespaced as `{{slug}}/{{model_id}}` (e.g. `myproxy/claude-sonnet-4-6`).

### Script Name Rules

- Must start with a letter or digit
- Only letters, digits, underscores, and hyphens after that
- Can't reuse a built-in provider's slug
- Must be executable"#,
        valid_values.join(", "),
        efforts.join(", "),
    )
}

fn tier_label(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Weak => "Weak",
        ModelTier::Medium => "Medium",
        ModelTier::Strong => "Strong",
        ModelTier::Compaction => "Compaction",
    }
}

fn format_pricing(entry: &ModelEntry) -> String {
    format!("${:.2} / ${:.2}", entry.pricing.input, entry.pricing.output)
}

fn format_context(entry: &ModelEntry) -> String {
    let ctx_k = entry.context_window / 1_000;
    match entry.max_output_tokens {
        Some(out) => format!("{ctx_k}K ctx / {}K out", out / 1_000),
        None => format!("{ctx_k}K ctx"),
    }
}

struct ProviderSection {
    name: &'static str,
    kind: ProviderKind,
    auth_line: String,
    urls: Vec<&'static str>,
    features: Option<&'static str>,
    entries: &'static [ModelEntry],
}

fn format_auth(kind: ProviderKind) -> String {
    let env = kind.api_key_env();
    if kind == ProviderKind::Ollama {
        format!("`OLLAMA_HOST` for local/remote (e.g. `http://localhost:11434`), `{env}` for auth")
    } else if kind == ProviderKind::Aperture {
        "`APERTURE_HOST` (e.g. `https://your-host.tailnet.ts.net`)".into()
    } else {
        format!("`{env}`")
    }
}

fn build_sections() -> Vec<ProviderSection> {
    let mut sections = Vec::new();

    for kind in ProviderKind::iter() {
        match kind {
            ProviderKind::OpenAi => {
                sections.push(ProviderSection {
                    name: kind.display_name(),
                    kind,
                    auth_line: format!("{} (also supports OAuth device flow)", format_auth(kind)),
                    urls: vec![kind.base_url()],
                    features: kind.features(),
                    entries: ManifestRegistry::get(&kind.to_string()).unwrap().models,
                });
            }
            ProviderKind::Copilot => {
                sections.push(ProviderSection {
                    name: kind.display_name(),
                    kind,
                    auth_line: format!(
                        "{} (or run `craft auth login copilot` to import a token from gh CLI, the Copilot client, or the system keyring)",
                        format_auth(kind)
                    ),
                    urls: vec![kind.base_url()],
                    features: kind.features(),
                    entries: ManifestRegistry::get(&kind.to_string()).unwrap().models,
                });
            }
            ProviderKind::Xai => {
                sections.push(ProviderSection {
                    kind,
                    name: kind.display_name(),
                    auth_line: format!(
                        "{} (also supports OAuth via `craft auth login xai`)",
                        format_auth(kind)
                    ),
                    urls: vec![kind.base_url(), "https://cli-chat-proxy.grok.com/v1"],
                    features: kind.features(),
                    entries: ManifestRegistry::get(&kind.to_string()).unwrap().models,
                });
            }
            _ => {
                sections.push(ProviderSection {
                    name: kind.display_name(),
                    kind,
                    auth_line: format_auth(kind),
                    urls: vec![kind.base_url()],
                    features: kind.features(),
                    entries: ManifestRegistry::get(&kind.to_string()).unwrap().models,
                });
            }
        }
    }

    sections
}

fn write_model_table(out: &mut String, entries: &[ModelEntry]) {
    let _ = writeln!(
        out,
        "| Tier | Models | Pricing (in/out per 1M tokens) | Context |"
    );
    let _ = writeln!(
        out,
        "|------|--------|-------------------------------|---------|"
    );

    // A row per model, not per tier: prices and context sizes differ inside a
    // tier, so one merged row would quote a single model's numbers for all.
    for tier in [ModelTier::Weak, ModelTier::Medium, ModelTier::Strong] {
        for entry in entries.iter().filter(|e| e.tier == tier) {
            let names = entry.prefixes.join(", ");
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                tier_label(tier),
                if entry.default {
                    format!("**{names}** (default)")
                } else {
                    names
                },
                format_pricing(entry),
                format_context(entry),
            );
        }
    }

    let defaults: Vec<String> = entries
        .iter()
        .filter(|e| e.default)
        .map(|e| {
            format!(
                "{} ({})",
                e.prefixes.first().unwrap_or(&"?"),
                tier_label(e.tier).to_lowercase(),
            )
        })
        .collect();

    if !defaults.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Defaults: {}", defaults.join(", "));
    }
}

fn no_catalog_note(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Ollama => {
            "Craft asks the server for the list of installed models, so there's no built-in catalog. Tiers are guessed from list order: the first model becomes strong, the second medium, and the rest weak."
        }
        ProviderKind::LlamaCpp => {
            "Connects to any OpenAI-compatible `/v1` endpoint. Craft asks the server for the list of installed models, so there's no built-in catalog. Tiers are guessed from list order: the first model becomes strong, the second medium, and the rest weak."
        }
        ProviderKind::Aperture => {
            "Aperture discovers models from your gateway. Set `APERTURE_HOST` to your Tailscale Aperture endpoint (e.g. `https://your-host.tailnet.ts.net`). No API key needed, Tailscale handles auth."
        }
        ProviderKind::OpenRouter => {
            "OpenRouter aggregates models from many providers behind a single API. Craft asks the OpenRouter API for the list of available models, so there's no built-in catalog. Tiers are guessed from list order: the first model becomes strong, the second medium, and the rest weak."
        }
        ProviderKind::Opencode => {
            "Models are discovered dynamically from the [models.dev](https://models.dev/) catalog and the Opencode Zen API, so there's no built-in catalog. Use any model id the catalog exposes, prefixed with the sub-provider (e.g. `opencode/<sub-provider>/<model-id>`)."
        }
        _ => {
            "Craft asks the server for the list of installed models, so there's no built-in catalog. Tiers are guessed from list order: the first model becomes strong, the second medium, and the rest weak."
        }
    }
}

fn write_section(out: &mut String, section: &ProviderSection) {
    let _ = writeln!(out, "### {}\n", section.name);
    let _ = writeln!(out, "- **Env var**: {}", section.auth_line);

    if section.urls.len() == 1 {
        let _ = writeln!(out, "- **API**: `{}`", section.urls[0]);
    } else {
        let _ = writeln!(out, "- **API endpoints**:");
        for url in &section.urls {
            let _ = writeln!(out, "  - `{url}`");
        }
    }

    if let Some(features) = section.features {
        let _ = writeln!(out, "- **Features**: {features}");
    }

    let _ = writeln!(out);

    if section.entries.is_empty() {
        let _ = writeln!(out, "{}", no_catalog_note(section.kind));
    } else {
        write_model_table(out, section.entries);
    }

    if section.name == "Anthropic" {
        let _ = writeln!(out, "\n{LONG_CONTEXT_NOTE}");
        let _ = writeln!(out, "\n{BEDROCK_NOTE}");
    }

    if section.kind == ProviderKind::Opencode {
        let _ = writeln!(out, "\n{OPENCODE_FREE_MODELS_NOTE}");
    }

    if section.kind == ProviderKind::Xai {
        let _ = writeln!(out, "\n{XAI_OAUTH_NOTE}");
    }
}

pub fn generate() -> String {
    let mut out = String::with_capacity(4096);

    let _ = writeln!(out, "# Providers\n");
    let _ = writeln!(
        out,
        "Craft talks to LLM providers over their HTTP APIs. \
          Models are split into three tiers: **weak** (cheap and fast), \
          **medium** (balanced), and **strong** (highest capability, highest cost). \
          There is also a **compaction** tier for choosing a dedicated model to summarize context when the conversation grows long.\n"
    );
    let _ = writeln!(out, "{TIER_PICKER_NOTE}\n");
    let _ = writeln!(out, "{AUTH_SECTION}\n");
    let _ = writeln!(out, "{BASE_URL_OVERRIDES}\n");
    let _ = writeln!(out, "## Built-in Providers\n");

    for section in &build_sections() {
        write_section(&mut out, section);
        let _ = writeln!(out);
    }

    // Opencode Go is catalog-backed (no ProviderKind), so it gets a static
    // section right after Opencode Zen, which is the last built-in section.
    let _ = writeln!(out, "{OPENCODE_GO_SECTION}\n");

    let _ = writeln!(out, "{MODEL_IDENTIFIERS}\n");
    let _ = writeln!(out, "{CUSTOM_PROVIDERS_SECTION}\n");
    let _ = writeln!(out, "{}", dynamic_providers_section());

    out
}
