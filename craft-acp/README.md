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

`agent::build(&provider, model_id, &config.agent, &workspace)` returns a native Rig
`Agent` with the base filesystem tools registered. The caller explicitly chooses
a `tools::Workspace`; it is not controlled by model arguments or agent TOML.
Build inside a Tokio runtime. The selected model ID is passed through exactly,
without discovery or a network request; manual and not-yet-listed models work.
Providers without completion support (Voyage AI) return an error before running.
The provider still validates whether a particular model supports chat at request time.

```rust
use craft_acp::{agent, config::Config, providers::Provider, tools::Workspace};
use rig::completion::{Chat, Message};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load().await?;
    let provider = Provider::from_config(&config.providers["openai"])?;
    let workspace = Workspace::new("/path/to/project")?;
    let agent = agent::build(&provider, "gpt-5.2", &config.agent, &workspace)?;
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
tool execution. There is no second Craft loop or conversation store.
`Chat` appends committed messages to caller-owned history only on success;
native errors are preserved, including budget/cancellation errors with recovery
history where Rig supplies it. For rich results, the caller handles the runner's
returned messages. Rig's streaming API is also available on the returned agent.

`agent::builder` returns the configured Rig builder with base tools attached, so
additional tools and hooks can be registered before `.build()`. Unknown tool
names still fail through Rig. A caller building a custom Rig agent can also use
`workspace.register(builder)` or register individual tool types.

### Filesystem tools

All tools use strict typed JSON arguments. Execution results remain typed Rust
values internally, but each implements Rig's `IntoToolOutput` to produce literal
text for the model. ACP displays that same text without tool-specific decoding.
The formats follow the reference Craft harness:

- `read`: `N: contents`, with `...` on clipped lines and a `Truncated lines: X-Y.
  Use offset=X to read further.` continuation notice when more lines remain.
  Empty files and reads just past EOF return empty text.
- `grep`: matches grouped under `path:` headings, each as `  N: contents`.
  No matches returns `No files found`. Partial-search, skipped-file, and clipped
  excerpt notices preserve this implementation's search-limit information.
- `edit`: `edited path` on success, not replacement-count JSON or a full diff.
- `delete`: `deleted: path` on success.

Error feedback remains literal text with Rig's existing error/refusal semantics.
Additional tools may still return JSON; ACP pretty-prints it without interpreting
its schema. This alignment concerns output, not feature parity: the argument
schemas, filesystem limits, exact-only edits, and nonrecursive deletes below
remain unchanged.

Paths are workspace-relative or absolute beneath the canonical workspace root. `..`,
Git metadata (`.git`), and symlink components are refused. Text tools support
UTF-8 files up to 8 MiB, including CRLF files. File I/O runs on blocking workers,
serialized across clones of the same workspace handle.

| Tool | Arguments | Behavior |
| --- | --- | --- |
| `read` | `path`, optional `offset` and `limit` | Numbered text and continuation hints with an offset for paging. Defaults to 200 lines; maximum 2000 (`limit = 0` means 2000). |
| `grep` | `pattern`, optional `path`, `glob`, `case_sensitive`, `literal`, `max_matches` | Line-based Rust regex search, or literal matching. Defaults to the whole workspace, case-sensitive, at most 100 matching lines (maximum 1000). |
| `edit` | `path`, `old_string`, `new_string`, optional `replace_all` or `occurrence` | Exact replacement in an existing file. Ambiguous matches fail unless disambiguated by a one-based occurrence or replace-all. No fuzzy matching or file creation. |
| `delete` | `path` | Permanently removes one existing regular file. No directories, recursion, or undo. Missing files are errors. |

Reads and searches bound source excerpts to 64 KiB and individual line excerpts
to 2048 bytes, before adding line numbers and notices. Grep respects workspace and
nested ignore rules even when a glob or path narrows the search. It skips hidden
files, symlinks, binary/non-UTF-8 files, non-Unicode filenames, and oversized files; skipped content files
are counted. Search stops at 10000 candidate files or its 64 MiB scan budget
and reports incomplete results rather than implying the search was exhaustive.
Clipped grep excerpts include the first match and report one-based byte columns
for both the match and excerpt start; read output starts each line at its beginning.

Edits stage beside the destination and atomically replace it, preserving basic
file permissions and bytes outside the exact replacement. A final content check
detects changes made while preparing the edit. Read-only files are refused.
There is no read-before-edit enforcement or stale-read tracking yet: agents
are instructed to read first, and ambiguous/missing old text is rejected.

Workspace checks are guardrails for a trusted local project, **not an OS security
sandbox**. Concurrent external filesystem changes can race path validation or
the final content check; the shared lock only coordinates tools using the same
workspace handle. Atomic replacement does not preserve extended metadata or
hardlink identity. Dropping a tool future does not cancel blocking I/O already
in progress. The host must supply any user-approval hooks or sandbox policy
before exposing an agent to untrusted workspaces. No approval UI or undo store
is implemented by this module.

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
