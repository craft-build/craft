# craft-acp

Foundation for Craft's CLI coding agent. Uses the `rig` facade (0.42.0) and
Tokio. The binary currently loads and validates configuration, then exits;
it does not start an ACP server or inference loop yet.

## Configuration

Create `~/.config/craft/agent.toml` using [agent.example.toml](agent.example.toml).
This exact home-relative path is used on every platform (not macOS Application
Support or `$XDG_CONFIG_HOME`). A missing file is an empty configuration.
Malformed TOML, unknown fields, invalid URLs, and invalid token limits are errors.
Loading never creates files, reads credentials, or contacts a provider.

Each `[providers.<alias>]` chooses a `kind`. Multiple aliases may use the same
kind with different credentials, URLs, and model catalogs. `api_key_env` names
an environment variable, **not the key itself**. Supply credentials in your
shell or secret manager; do not put secrets in this file.

All 26 public provider integrations in `rig::providers` are supported:

`anthropic`, `azure`, `chatgpt`, `cohere`, `copilot`, `deepseek`, `doubleword`,
`gemini`, `groq`, `huggingface`, `hyperbolic`, `llamafile`, `minimax`, `mira`,
`mistral`, `moonshot`, `ollama`, `openai`, `openrouter`, `perplexity`, `together`,
`venice`, `voyageai`, `xai`, `xiaomimimo`, `zai`.

Optional companion crates (e.g. Bedrock, Vertex AI, Candle, Gemini gRPC and
vector stores) are not enabled by this initial setup. The facade lets us enable
those features as we integrate them without changing dependency families.

### Protocols and authentication

- `openai` uses Rig's native Responses API client. Use `openai-compatible` for
  the Chat Completions protocol. Both accept `base_url`.
- `anthropic` accepts `base_url` for Anthropic-compatible services.
- Supply the complete base URL including any path prefix your server needs.
  URLs cannot contain credentials, query parameters, or fragments.
- Without explicit credentials/URL, native clients use Rig's environment/auth
  defaults. With overrides, `api_key_env` defaults to the provider's usual key
  variable (e.g. `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`,
  `VOYAGE_API_KEY`, `XIAOMI_MIMO_API_KEY`).
- For a keyless OpenAI-compatible server, set the referenced environment
  variable to a non-secret placeholder if the server ignores authentication.
- Ollama and Llamafile support keyless local use. Llamafile rejects `api_key_env`.
- Azure accepts `base_url` as its Azure endpoint and `api_version`; these fall
  back to `AZURE_ENDPOINT` and `AZURE_API_VERSION`. Credentials default to
  `AZURE_API_KEY` or `AZURE_TOKEN`.
- ChatGPT uses Rig OAuth by default; `api_key_env` selects an access token,
  optionally paired with `account_id`.
- Copilot uses Rig's environment/OAuth defaults. An explicit `api_key_env`
  selects a Copilot API key, not a GitHub access token. Use Rig's native
  environment configuration for GitHub-token auth.

### Model catalogs

`[providers.<alias>.models."<exact-model-id>"]` declares a model or partially
overrides one returned by discovery. Supported fields are `name`, `description`,
`context_length`, and `max_output_tokens`. An empty table registers just the ID.
IDs containing dots or slashes should be quoted.

`Provider::models` queries Rig's models API where supported, merges by exact ID,
and sorts by ID. Only explicitly configured fields replace discovered values;
unmentioned models and metadata remain intact. Providers without a listing API
use only configured models. Discovery errors propagate: they are not silently
treated as an empty catalog. Set `discover_models = false` for manual-only use
or compatible endpoints that do not implement model listing.

Token limits are catalog metadata, not request parameters. They do not silently
set a generation cap. Set `[agent].max_tokens` for a request-level output cap.

## Agent loop

`agent::build(&provider, model_id, &config.agent)` returns a native Rig `Agent`.
Build inside a Tokio runtime. The selected model ID is passed through exactly,
without discovery or a network request; manual and not-yet-listed models work.
Providers without completion support (Voyage AI) return an error before running.
The provider still validates whether a particular model supports chat at request time.

```rust
use craft_acp::{agent, config::Config, providers::Provider};
use rig::completion::{Chat, Message};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load().await?;
    let provider = Provider::from_config(&config.providers["openai"])?;
    let agent = agent::build(&provider, "gpt-5.2", &config.agent)?;
    let mut history = Vec::<Message>::new();
    let reply = agent.chat("Help me plan a refactor.", &mut history).await?;
    println!("{reply}");

    // Rich results: final text/content, aggregate usage, and per-call metadata.
    let response = agent.runner("Summarize the plan.")
        .history(history)
        .run()
        .await?;
    println!("{}", response.output());
    Ok(())
}
```

Rig's runner owns model calls, continuations, retries, hook cancellation, and
future tool execution. There is no second Craft loop or conversation store.
`Chat` appends committed messages to caller-owned history only on success;
native errors are preserved, including budget/cancellation errors with recovery
history where Rig supplies it. For rich results, the caller handles the runner's
returned messages. Rig's streaming API is also available on the returned agent.

`agent::builder` returns the configured Rig builder so future tools and hooks can
be registered before `.build()`. No tools are registered yet, and unexpected tool
calls fail through Rig rather than executing anything.

### Agent settings

The optional `[agent]` table has these defaults:

| Field | Default | Meaning |
| --- | --- | --- |
| `preamble` | `"You are Craft, an AI coding assistant."` | System instructions; `""` disables them |
| `max_turns` | `16` | Total model calls per run, including initial call, retries, and continuations |
| `temperature` | Omitted | Provider/model sampling default |
| `max_tokens` | Omitted | Provider/model output-token default |

Limits must be positive; temperature must be finite and nonnegative. Individual
providers/models may impose additional limits or reject sampling parameters.
`max_turns` is a **per-run model-call budget**, not a limit on conversation
messages. Rig's native per-run overrides remain available through `.runner(...)`.
Prompt/response content telemetry is disabled; structural metadata and usage
remain available. The binary remains config-validation-only until CLI/ACP wiring
is added; the agent module is ready for its caller to select a provider/model.

## Development

```sh
cargo test -p craft-acp
cargo fmt -p craft-acp --check
cargo clippy -p craft-acp --all-targets -- -D warnings
```

`Provider::from_config` builds only the selected native Rig client. Match its
variant to access Rig's capability-specific APIs (including non-completion
providers such as Voyage AI). Merely parsing configuration does not construct
clients or initiate interactive authentication.
