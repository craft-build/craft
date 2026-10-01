# craft

The `craft` library and `crafty` binary: Craft's CLI/TUI coding agent, written from scratch in Rust on top of the [rig](https://github.com/0xPlaygrounds/rig) facade (tokio, reqwest), with the `argosy` knowledge crate linked in-process.

The binary is a full agent: a ratatui TUI (or `--print` headless mode), subagents, MCP, skills, recipes, an ACP server, permissions and snapshots, and a multi-stage context-management pipeline.

## Configuration

Configuration lives in BML format (`barkml`) at `~/.config/craft.bml`, or split across `~/.config/craft/*.bml`. A legacy `~/.craft/` location is still searched. See [agent.example.bml](agent.example.bml). A missing file is an empty configuration; malformed files, unknown fields, invalid URLs, and invalid limits are errors. Loading never writes files or contacts a provider. On first run, providers are auto-detected from credential environment variables when the config declares none.

Each `[providers.<alias>]` chooses a `kind`. Multiple aliases may use the same kind with different credentials, URLs, and model catalogs. `api_key_env` names an environment variable, **not the key itself** — keep secrets in your shell or secret manager.

The following rig-native provider kinds are supported: `amazon-bedrock`, `anthropic`, `azure`, `chatgpt`, `cohere`, `copilot`, `deepseek`, `doubleword`, `gemini`, `groq`, `huggingface`, `hyperbolic`, `llamafile`, `minimax`, `mira`, `mistral`, `moonshot`, `ollama`, `openai`, `openai-compatible`, `openrouter`, `perplexity`, `together`, `venice`, `voyageai`, `xai`, `xiaomimimo`, `zai`.

- `openai` uses Rig's native Responses API client; use `openai-compatible` for Chat Completions. Both accept `base_url`.
- `anthropic` accepts `base_url` for Anthropic-compatible services. Supply the complete base URL including any path prefix; URLs cannot contain credentials, query parameters, or fragments.
- Without explicit credentials/URL, native clients use Rig's environment/auth defaults. With overrides, `api_key_env` defaults to the provider's usual key variable (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, ...).
- Ollama and Llamafile support keyless local use (Llamafile rejects `api_key_env`).
- `amazon-bedrock` (via `rig-bedrock`) authenticates through the AWS default credential chain (env vars, `~/.aws` profiles, SSO) and takes its region from `AWS_REGION`/`AWS_DEFAULT_REGION` or the profile; it rejects `api_key_env` and `base_url` (use `AWS_ENDPOINT_URL` to override the endpoint).
- Azure takes `base_url` + `api_version` (falling back to `AZURE_ENDPOINT` / `AZURE_API_VERSION`), credentials from `AZURE_API_KEY` or `AZURE_TOKEN`.
- ChatGPT uses Rig OAuth by default; `api_key_env` selects an access token, optionally with `account_id`.
- Copilot uses Rig's environment/OAuth defaults; an explicit `api_key_env` selects a Copilot API key.

### Model catalogs

`[providers.<alias>.models."<exact-model-id>"]` declares or partially overrides a model (`name`, `description`, `context_length`, `max_output_tokens`). Discovery merges configured models with live listings and the [models.dev](https://models.dev) catalog (disk-cached), sorted by ID; `discover_models = false` disables listing. Discovery errors propagate rather than silently yielding an empty catalog. Token limits are catalog metadata (they feed compaction thresholds and usage display), not request parameters — set `[agent].max_tokens` for an output cap.

## Context management

### Compression (before history)

- **Tool output pre-compression** — heuristic content-type detection routes each tool result to a dedicated compressor: code, logs, search results, diffs, JSON arrays, plain text. Log/code scoring uses a shared aho-corasick keyword classifier (error, warning, security, definition, import, module, comment, closing brace).
- **Read lifecycle** — file reads are tracked fresh/stale/superseded; stale and superseded reads are collapsed into compact markers on the request path while history stays intact.
- **Reversible compression** — compressed originals are kept in an in-memory, content-hash-keyed store (100 entries by default); the `retrieve` tool restores them on demand.
- **Tool dedup cache** — identical read-only tool invocations return cached output; entries are invalidated structurally when files change.

### Compaction (when the window fills)

`[[compaction]]` array-of-table entries configure staged compaction, each with a `kind` and a `context` fill ratio:

```toml
[[compaction]]
kind = "vcc"
context = "0.6"

[[compaction]]
kind = "llm"
context = "0.8"
```

Before each turn the harness estimates history size and runs every armed stage whose threshold is crossed, lowest ratio first:

- `vcc` — deterministic, no-LLM compaction: the head of the history is replaced by a structured summary message; the recent tail is kept verbatim; repeated runs merge with the previous summary.
- `llm` — one-shot summarization call replacing the head with a handoff summary (deterministic fallback if the call fails).

Effectiveness gating skips a stage whose last run saved under 10% of estimated context until another stage compacts effectively. Omitting `[[compaction]]` defaults to vcc at 0.6 then llm at 0.8. Sessions without a known `context_length` never compact.

### Overflow recovery

When a request still overflows after compaction, an escalating ladder collapses tool results progressively (10%, 20%, 50%, 100%) before failing the turn.

## Run loop

Rig's runner owns model calls, streaming, and tool execution; craft layers on top:

- streaming retries with backoff, API-key rotation, and a model fallback chain;
- doom-loop detection with grace/hard-stop thresholds;
- per-tool guardrails that demote or drop tools after decaying failures;
- a recency tail (`<turn-context>`) appended to the last user message;
- plan-mode write restriction;
- subagents (`task` tool): own context window and cancel token, restricted tool table, model tiers (weak/medium/strong, capped at the parent), optional worktree isolation, optional JSON-schema-validated output.

`agent::build(&provider, model_id, &config.agent, &workspace)` returns a native Rig `Agent` with the base tools registered; `agent::builder` returns the builder so more tools and hooks can be attached before `.build()`.

## Tools

All tools use strict typed JSON arguments and produce literal text for the model. Filesystem tools: `read` (numbered lines, `offset`/`limit` paging, continuation notices), `grep` (regex or literal, ignore-aware, bounded scan budget), `edit` / `inplace_edit` / `multiedit` / `fuzzy_replace` / `apply_patch`, `write`, `delete`, `move_file`, `glob`, `list`. Editing stages beside the destination and atomically replaces it, preserving permissions and bytes outside the replacement.

Execution tools: `bash` (with `bash_status`, `bash_watch`, `bash_kill` for background tasks; sandbox profiles; tree-sitter command parsing for permission scoping), `batch` (parallel dispatch), `question`, `todo_write`, `view_image`, `webfetch` (SSRF-guarded), `websearch`, `sessions`, `skill`, `worktree`, `review`, `task`, `retrieve`, `list_tools`. MCP tools are proxied as `server__tool` portable tools.

### Argosy knowledge tools

The `argosy` crate is linked in-process (no subprocess, no MCP transport). It contributes semantic `search` and `ask` (with an optional decision endpoint), document/memory/rule reads and writes, skill discovery (`list_skills` / `get_skill`), and a review toolset (`start_review`, `review_diff`, `report_finding`, `review_findings`). Embeddings run locally (ONNX, via the argosy crate). Argosy keeps its own state keyed by project root — writes from craft are visible to the standalone `argosy mcp`. The `reviewer` subagent snapshots the working-tree diff and returns prioritized P0-P3 findings; `/review`, `/scan`, `/dream`, and `/memory` slash commands expose the same workflows. Post-turn memory extraction (default on, `agent.memory_extraction`) writes durable facts into the project's local argosy.

### Paths and limits

Paths are workspace-relative or absolute beneath the canonical workspace root; `..`, `.git`, and symlink components are refused. Text tools support UTF-8 files up to 8 MiB, including CRLF. Reads and searches bound excerpts to 64 KiB (individual lines to 2048 bytes) and report incomplete results rather than implying exhaustiveness. Workspace checks are guardrails for a trusted local project, **not an OS security sandbox**; the host supplies approval hooks and sandbox policy before exposing the agent to untrusted workspaces.

## Permissions, snapshots, auto-review

- BML rule blocks (allow / deny / ask, scoped to bash or MCP, wildcards). Bash commands are parsed so compound commands request each sub-command's approval. `--yolo` / `--dangerously-skip-permissions` disables prompting.
- Auto-review mode (`-A/--auto-review`): an LLM reviewer answers permission prompts, fail-closed below a confidence threshold.
- Snapshots: files are snapshotted before mutations (first-wins, ≤5 MiB, text only) and `/undo` walks them back.

## TUI

ratatui interface: 30 built-in themes (`/theme`), rebindable keymaps, command palette (Ctrl-P), fuzzy search (Ctrl-F), subagent chat windows (Ctrl-N / Ctrl-P), clipboard image paste (Ctrl-V), editor integration, plan mode with a plan form and todo cards, `!` / `!!` bash passthrough, input history, and session resume. Slash commands: `/new`, `/resume`, `/continue`, `/sessions`, `/model`, `/clear`, `/undo`, `/mcp`, `/compact`, `/usage`, `/stats`, `/auto-review`, `/theme`, `/recipe`, `/review`, `/scan`, `/dream`, `/memory`, plus custom namespaced commands.

Sessions are crash-safe append-only JSONL files with an index, archive, and usage ledger (`craft stats`, `/stats`).

## CLI

`crafty` (binary) supports `--print` (headless, `--output-format text|stream-json`, Claude Code-compatible), `--image PATH`, `-m/--model provider/model`, `-c` continue, `-s/--session` resume, `--mode build|plan`, `--allowedTools` / `--disallowed-tools`, `--max-turns`, `--system-prompt` overrides, and a set of accepted-but-ignored Claude Code SDK compatibility flags.

Subcommands: `acp` (agent-client-protocol server on stdio), `completions <shell>`, `models`, `stats [--sessions]`, `doctor [--export]` (diagnose and self-heal provider config), `update [-y]` / `rollback` (digest-pinned self-update), `prompt [system|research|general] [--plan|--tools|--names]`, `term init|log|run|info` (shell integration), `recipe list|run`.

Not yet implemented (flagged with warnings at runtime): `--input-format stream-json` (SDK mode), `--fork-session`, verbose transcript output, flow mode.

## Development

```sh
cargo test -p craft
cargo fmt -p craft --check
cargo clippy -p craft --all-targets -- -D warnings
```

`Provider::from_config` builds only the selected native Rig client; parsing configuration never constructs clients or initiates interactive authentication.
