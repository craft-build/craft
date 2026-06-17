use craft_providers::model::{ModelEntry, ModelTier, models_for_provider};
use craft_providers::provider::ProviderKind;
use std::fmt::Write;
use strum::IntoEnumIterator;

const TIER_PICKER_NOTE: &str = r#"Open the model picker with `/model` and press `1`, `2`, or `3` on any row to reassign it to strong, medium, or weak. Your overrides are saved to `~/.local/state/craft/model-tiers` and apply across sessions."#;

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

const LONG_CONTEXT_NOTE: &str = r#"Add `-1m` to any Claude model, like `claude-sonnet-4-6-1m`, to use the 1M token context window."#;

const BEDROCK_NOTE: &str = r#"#### Amazon Bedrock

If you already use Claude through AWS Bedrock, you can point Craft at it instead of the direct Anthropic API. Set `CLAUDE_CODE_USE_BEDROCK=1` and Craft will route all Anthropic requests through Bedrock. The same models, the same features, just a different door.

You will need `AWS_REGION` and one of the following for auth:

| Method | Env vars |
|--------|----------|
| IAM credentials | `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (and optionally `AWS_SESSION_TOKEN`) |
| Credentials file | `AWS_PROFILE` (defaults to `default`), reads `~/.aws/credentials` |
| Bearer token | `AWS_BEARER_TOKEN_BEDROCK` |
| Gateway proxy | `CLAUDE_CODE_SKIP_BEDROCK_AUTH=1` + `ANTHROPIC_BEDROCK_BASE_URL` (skips signing, useful behind a proxy that handles auth) |

You can override the model with `ANTHROPIC_MODEL` and the endpoint with `ANTHROPIC_BEDROCK_BASE_URL`. These env var names match Claude Code, so if you were already using Bedrock there, the same setup works here."#;

const MODEL_IDENTIFIERS: &str = r#"## Model Identifiers

Models are referenced as `provider/model_id`:

```
anthropic/claude-sonnet-4-6
openai/gpt-4.1
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

Custom providers appear in `craft auth login` and the model picker just like built-in ones."#;

fn dynamic_providers_section() -> String {
    let valid_values: Vec<String> = ProviderKind::iter().map(|k| format!("`{k}`")).collect();

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

Only `id` is required. Optional fields: `tier` (default `medium`), `context_window` (128K), `max_output_tokens` (16K), `pricing` (`{{input, output, cache_write, cache_read}}`, all per 1M tokens), `supports_tool_examples` (defaults to the base provider's setting). The first model listed per tier is used for sub-agents. Without this subcommand, the base provider's models are used.

Dynamic provider models are namespaced as `{{slug}}/{{model_id}}` (e.g. `myproxy/claude-sonnet-4-6`).

### Script Name Rules

- Must start with a letter or digit
- Only letters, digits, underscores, and hyphens after that
- Can't reuse a built-in provider's slug
- Must be executable"#,
        valid_values.join(", "),
    )
}

fn tier_label(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Weak => "Weak",
        ModelTier::Medium => "Medium",
        ModelTier::Strong => "Strong",
    }
}

fn format_pricing(entry: &ModelEntry) -> String {
    format!("${:.2} / ${:.2}", entry.pricing.input, entry.pricing.output)
}

fn format_context(entry: &ModelEntry) -> String {
    let ctx_k = entry.context_window / 1_000;
    let out_k = entry.max_output_tokens / 1_000;
    format!("{ctx_k}K ctx / {out_k}K out")
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
                    entries: models_for_provider(kind),
                });
            }
            ProviderKind::Copilot => {
                sections.push(ProviderSection {
                    name: kind.display_name(),
                    kind,
                    auth_line: format!(
                        "{} or `~/.config/github-copilot/{{hosts.json,apps.json}}`",
                        format_auth(kind)
                    ),
                    urls: vec![kind.base_url()],
                    features: kind.features(),
                    entries: models_for_provider(kind),
                });
            }
            _ => {
                sections.push(ProviderSection {
                    name: kind.display_name(),
                    kind,
                    auth_line: format_auth(kind),
                    urls: vec![kind.base_url()],
                    features: kind.features(),
                    entries: models_for_provider(kind),
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

    for tier in [ModelTier::Weak, ModelTier::Medium, ModelTier::Strong] {
        let tier_entries: Vec<_> = entries.iter().filter(|e| e.tier == tier).collect();
        if tier_entries.is_empty() {
            continue;
        }

        let models: Vec<String> = tier_entries
            .iter()
            .map(|e| {
                let names = e.prefixes.join(", ");
                if e.default {
                    format!("**{names}** (default)")
                } else {
                    names
                }
            })
            .collect();

        let pricing = tier_entries
            .first()
            .map(|e| format_pricing(e))
            .unwrap_or_default();
        let context = tier_entries
            .first()
            .map(|e| format_context(e))
            .unwrap_or_default();

        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            tier_label(tier),
            models.join(", "),
            pricing,
            context,
        );
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
        ProviderKind::OpenRouter => {
            "OpenRouter aggregates models from many providers behind a single API. Craft asks the OpenRouter API for the list of available models, so there's no built-in catalog. Tiers are guessed from list order: the first model becomes strong, the second medium, and the rest weak."
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
}

pub fn generate() -> String {
    let mut out = String::with_capacity(4096);

    let _ = writeln!(out, "# Providers\n");
    let _ = writeln!(
        out,
        "Craft talks to LLM providers over their HTTP APIs. \
         Models are split into three tiers: **weak** (cheap and fast), \
         **medium** (balanced), and **strong** (highest capability, highest cost).\n"
    );
    let _ = writeln!(out, "{TIER_PICKER_NOTE}\n");
    let _ = writeln!(out, "{AUTH_SECTION}\n");
    let _ = writeln!(out, "## Built-in Providers\n");

    for section in &build_sections() {
        write_section(&mut out, section);
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "{MODEL_IDENTIFIERS}\n");
    let _ = writeln!(out, "{CUSTOM_PROVIDERS_SECTION}\n");
    let _ = writeln!(out, "{}", dynamic_providers_section());

    out
}
