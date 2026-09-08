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
set a generation cap. Request-level tuning and the agent loop will be added
when inference execution is integrated.

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
