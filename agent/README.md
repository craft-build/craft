# craft

The `craft` library and `crafty` binary: Craft's CLI/TUI coding agent, written from scratch in Rust on top of the [animist](https://crates.io/crates/animist) provider/agent/history stack (async end to end), with the `argosy` knowledge crate linked in-process.

The binary is a full agent: a TUI (or `--print` headless mode), subagents, MCP, skills, recipes, an ACP server, permissions and snapshots, and a multi-stage context-management pipeline.

## Configuration

Configuration lives in BML format (`barkml`) at `~/.config/craft.bml`, or split across `~/.config/craft/*.bml`. A legacy `~/.craft/` location is still searched. See [agent.example.bml](agent.example.bml). A missing file is an empty configuration; malformed files, unknown fields, invalid URLs, and invalid limits are errors. Loading never writes files or contacts a provider. On first run, providers are auto-detected from credential environment variables when the config declares none.

Each `[providers.<alias>]` chooses a `kind`. Multiple aliases may use the same kind with different credentials, URLs, and model catalogs. `api_key_env` names an environment variable, **not the key itself** — keep secrets in your shell or secret manager.

The supported kinds: `anthropic`, `openai`, `openai-compatible`, `synthetic`, `zai-coding-plan`, `ollama`, `llama-cpp`, `omlx`, `amazon-bedrock`.

- `openai` speaks the Responses API by default; `protocol = "chat_completions"` selects Chat Completions. Both accept `base_url`.
- `openai-compatible` speaks Chat Completions against any OpenAI-dialect endpoint. A **keyless local endpoint** (loopback or private host, no key configured) automatically rides animist's keyless llama.cpp client — local llama.cpp/vLLM/LM Studio servers work with no dummy key. Non-local hosts require a key.
- `synthetic` and `zai-coding-plan` are dedicated kinds for those hosted plans (default env `SYNTHETIC_API_KEY` / `ZHIPU_API_KEY`); existing `openai-compatible` configs pointing at their endpoints keep working.
- `anthropic` accepts `base_url` for Anthropic-compatible services. Supply the complete base URL including any path prefix.
- `ollama`, `llama-cpp`, and `omlx` serve local models; `ollama` needs no credential, the other two accept an optional key (`llama-cpp` also serves System One decisions on `/systemone`).
- With overrides, `api_key_env` defaults to the provider's usual key variable (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, ...).
- `amazon-bedrock` (feature `bedrock`) authenticates through the AWS default credential chain (env vars, `~/.aws` profiles incl. `AWS_PROFILE`, SSO, assume-role, `credential_process`, web identity, container/IMDS roles) and takes its region from `AWS_REGION`/`AWS_DEFAULT_REGION`, the active profile, or `region` on the provider. Animist owns the HTTP client, signing, in-provider 429/503 retries (`max_attempts`, default 3), and control-plane model listing. Bedrock's wire protocol is per model:

  | Model family | Protocol | Wire |
  | --- | --- | --- |
  | Claude (`anthropic.*` or ids containing `claude`) | `anthropic_messages` | `/model/{id}/invoke-with-response-stream` (Anthropic Messages) |
  | `openai.*` deployments | `responses` | `/openai/v1/responses` |
  | other OpenAI-surface models (`deepseek.*`, `moonshotai.*`) | `chat_completions` | `/openai/v1/chat/completions` |
  | everything else | `converse` | `/model/{id}/converse-stream` (AWS Converse) |

  A per-model `protocol` override wins (`protocol = "converse"` in the model block). **Converse caveat:** reasoning configs other than off are rejected on Converse — Claude-on-Bedrock thinking requires the `anthropic_messages` protocol.

**Dropped kinds** (unsupported since the animist cutover): `azure`, `chatgpt`, `cohere`, `copilot`, `deepseek`, `doubleword`, `gemini`, `groq`, `huggingface`, `hyperbolic`, `llamafile`, `minimax`, `mira`, `mistral`, `moonshot`, `openrouter`, `perplexity`, `together`, `venice`, `voyageai`, `xai`, `xiaomimimo`, `zai`. A config naming one fails with an actionable error. Migration for OpenAI-dialect vendors — point `openai-compatible` at their base URL:

```bml
# before: kind = "deepseek"
provider "deepseek" {
  kind = "openai-compatible"
  base_url = "https://api.deepseek.com/v1"
  api_key_env = "DEEPSEEK_API_KEY"
}
```

The OAuth/device flows (`chatgpt`, `copilot`) are gone with their kinds.

### Model catalogs

`[providers.<alias>.models."<exact-model-id>"]` declares or partially overrides a model (`name`, `description`, `context_length`, `max_output_tokens`, `supports_thinking`, `requires_thinking`, `protocol`). Discovery merges configured models with live listings and the [models.dev](https://models.dev) catalog (disk-cached 24 h), sorted by ID; `discover_models = false` disables listing. Discovery errors propagate rather than silently yielding an empty catalog. Token limits are catalog metadata (they feed compaction thresholds and usage display), not request parameters — set `[agent].max_tokens` for an output cap.

## Context management

### Compression and the read lifecycle (durable, on history)

- **Tool-output compression** — between runs, an alteration pass rewrites oversized tool results into compact markers as history *revisions*; the verbatim originals stay in the entry sources, and the `retrieve` tool restores them by content hash (`[N lines compressed from M. Retrieve original: hash=…]`). Nothing lives in a side store: a marker survives session reloads, and retrieval works in any later turn.
- **Read lifecycle** — file reads are tracked fresh/stale/superseded; stale and superseded reads get durable marker revisions so old snapshots stop occupying context, while `retrieve` still serves the original bytes.
- **Tool dedup cache** — identical read-only tool invocations return cached output; entries are invalidated structurally when files change.
- Within a run, the request layer recomputes only the run's own suffix (everything older arrives pre-altered) and relocates tool-result images for Chat Completions targets.

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

Between runs the driver estimates history size and runs every armed stage whose threshold is crossed, lowest ratio first:

- `vcc` — deterministic, no-LLM compaction: the head of the history is replaced by a structured summary entry (tagged `craft.compaction`); the recent tail is kept verbatim; repeated runs merge with the previous summary.
- `llm` — one-shot summarization call replacing the head with a handoff summary (deterministic fallback if the call fails).

Effectiveness gating skips a stage whose last run saved under 10% of estimated context until another stage compacts effectively. Omitting `[[compaction]]` defaults to vcc at 0.6 then llm at 0.8. Sessions without a known `context_length` never compact. Compacted-away entries are *deactivated*, never deleted — their originals stay in entry sources for `retrieve`.

### Overflow recovery

When a request still overflows after compaction, an escalating ladder collapses tool results progressively (10%, 20%, 50%, 100%) before failing the turn; a failed run keeps whatever compaction it did (originals are never lost).

## Runtime setup contract

Every execution surface (TUI, `--print` headless, ACP) obtains the shared pieces of a session from `agent/src/runtime.rs`, so equivalent sessions receive equivalent run policies:

- `runtime::workspace_env(cwd, mcp)` — instruction discovery, the workspace (MCP handle installed), and the permission engine. Every surface constructs the `PermissionManager` and consults it: the TUI prompt overlay, the ACP `session/request_permission`, and the headless gate below.
- `model_selection::resolve_or_default` — a model choice plus its catalog window; output ceilings ride the model handle, where request shaping reads them. Lookup only: `--model` ids the catalog does not list stay usable.

## The harness

One long-lived `harness::Conversation` per session drives an animist `Agent` over an animist `History`; surfaces never run a loop of their own. Craft policy lives in an ordered `AgentLayer` chain (head → tail):

1. **PolicyLayer** — permission rules → auto-review → the approval responder → the plan-mode gate.
2. **GuardrailLayer** — doom-loop detection (grace/hard-stop), per-tool guardrails, dedup `Respond`.
3. **RequestLayer** — the per-request view: in-run read-lifecycle and compression rewrites, Chat-Completions image relocation, the output-token clamp, sampling options, fallback-model selection.
4. **RecoveryLayer** — `recover`: linear backoff, API-key rotation, model fallback chain, reauth wait, overflow compaction (calling the compactors above).
5. **ContinuationLayer** — `MaxTokens` continuation and stalled-turn nudges as `step_end` appends.
6. **SnapshotLayer** — `/undo` pre-captures at the tail, so only calls that really run are captured.
7. **PersistLayer** — flushes the history change feed to the session log.

A thin driver (`harness::driver::run_turn`) does what no hook can: builds the input, runs and awaits the agent, adds terminal markers, runs the between-run alteration/compaction passes, and re-runs for the advisor and `ContextWindowExceeded`. Subagents (`task` tool) run as their own child `Conversation` with a fresh history, a restricted tool registry, and their own layer instances built from the parent's shared services; `batch` fans children out concurrently under the parent's claims and budgets. Model switching happens between runs by rebuilding the agent on the same history handle; fallback models inside a run go through the request layer.

## Tools

All tools use strict typed JSON arguments and produce literal text for the model. Filesystem tools: `read` (numbered lines, `offset`/`limit` paging, continuation notices), `grep` (regex or literal, ignore-aware, bounded scan budget), `edit` / `inplace_edit` / `multiedit` / `fuzzy_replace` / `apply_patch`, `write`, `delete`, `move_file`, `glob`, `list`. Editing stages beside the destination and atomically replaces it, preserving permissions and bytes outside the replacement.

Execution tools: `bash` (with `bash_status`, `bash_watch`, `bash_kill` for background tasks; sandbox profiles; tree-sitter command parsing for permission scoping), `batch` (parallel dispatch), `question`, `todo_write`, `view_image`, `webfetch` (SSRF-guarded), `websearch`, `sessions`, `skill`, `worktree`, `review`, `task`, `retrieve`, `list_tools`. MCP tools are proxied as `server__tool` portable tools.

### Command sandbox

`bash` runs under an OS sandbox configured by the `[sandbox]` config section (`enabled`, `mode` = `workspace_write` (default) | `read_only` | `danger_full_access` | `off`, `network` (default `true`)). macOS uses the stock `sandbox-exec`; Linux wraps argv with `bubblewrap` (`bwrap`, prerequisite). Enforcement is **fail-closed** on supported platforms: if the backend binary is missing while confinement is requested, the command errors before it starts (install it or set `sandbox.mode = "off"`). On platforms without a sandbox implementation, commands run unsandboxed with a visible one-time note. `--yolo` disables the sandbox entirely (predecessor parity), and `CRAFT_SANDBOX=off` is the documented env escape hatch with highest precedence (debug/CI). Plan-mode turns and read-only workers (reviewer, research subagents) run bash read-only; the allocated plan file stays writable via an exact-path grant.

### Argosy knowledge tools

The `argosy` crate is linked in-process (no subprocess, no MCP transport). It contributes semantic `search` and `ask` (with an optional decision endpoint), document/memory/rule reads and writes, skill discovery (`list_skills`; loading goes through the builtin `skill` tool), and a review toolset (`start_review`, `review_diff`, `report_finding`, `review_findings`). Embeddings run locally (ONNX, via the argosy crate). Argosy keeps its own state keyed by project root — writes from craft are visible to the standalone `argosy mcp`. The `reviewer` subagent snapshots the working-tree diff (or the current state of named paths) and returns prioritized P0-P3 findings; `/review`, `/scan`, `/dream`, and `/memory` slash commands expose the same workflows. Post-turn memory extraction (default on, `agent.memory_extraction`) writes durable facts into the project's local argosy.

### Paths and limits

Paths are workspace-relative or absolute beneath the canonical workspace root; `..`, `.git`, and symlink components are refused. Text tools support UTF-8 files up to 8 MiB, including CRLF. Reads and searches bound excerpts to 64 KiB (individual lines to 2048 bytes) and report incomplete results rather than implying exhaustiveness. Workspace checks are guardrails for a trusted local project, **not** an OS security sandbox**;** the host supplies approval hooks and sandbox policy before exposing the agent to untrusted workspaces.

## Permissions, snapshots, auto-review

- BML rule blocks (allow / deny / ask, scoped to bash or MCP, wildcards). Bash commands are parsed so compound commands request each sub-command's approval. `--yolo` / `--dangerously-skip-permissions` disables prompting.
- Headless runs (`--print`, `term run`, `recipe run`) enforce the same rules through a permission gate with no interactive approver: unresolved asks fail closed with guidance naming the opt-outs, and denied calls never execute. `--yolo` (bypass everything) and `-A/--auto-review` (an LLM or decision-endpoint reviewer answers asks, fail-closed) are the deliberate opt-outs — mutually exclusive, rejected at `Cli::validate`. The gate covers batch children and subagent dispatch.
- Auto-review mode (`-A/--auto-review`): an LLM reviewer answers permission prompts, fail-closed below a confidence threshold.
- Snapshots: files are snapshotted before mutations (first-wins, ≤5 MiB, text only) and `/undo` walks them back. Undo restores file contents only; deactivated history entries survive in session sources but are not restored.

## TUI

TUI (superlighttui): 30 built-in themes (`/theme`), rebindable keymaps, command palette (Ctrl-P), fuzzy search (Ctrl-F), subagent chat windows (Ctrl-N / Ctrl-P), clipboard image paste (Ctrl-V), editor integration, plan mode with a plan form and todo cards, `!` / `!!` bash passthrough, input history, and session resume. Slash commands: `/new`, `/resume`, `/continue`, `/sessions`, `/model`, `/clear`, `/undo`, `/mcp`, `/compact`, `/usage`, `/stats`, `/auto-review`, `/theme`, `/recipe`, `/review`, `/scan`, `/dream`, `/memory`, plus custom namespaced commands.

## Sessions

Sessions are crash-safe append-only JSONL files with an index, archive, and usage ledger (`craft stats`, `/stats`). Messages persist as animist **entry records** (log format 4): `entry`, `insert`, `revise`, `deactivate`, `replace`, mapping one to one onto the history change feed — plus `sub_entry` subagent histories and the trailing meta record. One loader reads every older shape (pre-animist message records, `.json` files) and folds their raw `tool_outputs` copies into entry sources; saving rewrites the current shape. A corrupt or truncated trailing record is skipped with a warning; a record whose index is out of range for the replayed list **fails the load** with an actionable error. **One-way upgrade:** older `crafty` builds cannot read the new saves — upgrade before rolling back. Resume (`/sessions`, `-c`, `-s`, ACP `session/load`) replays the same shared transcript builder: inactive entries hidden, compaction summaries as cards, markers by tag, altered tool results showing their altered text with the verbatim original one click away. `--fork-session` copies a `--session`/`--continue` source under a new id without touching the source.

## CLI

### Thinking controls

`/thinking` opens the TUI selector; `/thinking low`, `/thinking adaptive`, `/thinking off`, or `/thinking 4096` apply directly. Alt+E cycles the same settings. The preference is persisted with the session, restored on resume, and inherited by subagents. ACP exposes a **Thinking** session option.

For headless or startup overrides use one of `--thinking <setting>`, `--effort <level>`, or `--max-thinking-tokens <positive-count>`. They are mutually exclusive. Seed new sessions in `craft.bml`:

```bml
always_thinking = "low"
```

Modes are `off`, `adaptive`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, or a positive token count. An unset preference means **off**, matching the reference. Providers that cannot disable reasoning use their lowest supported mode; "off" is not a universal promise of zero reasoning.

Per-model `models.dev` reasoning metadata resolves accepted effort levels. Requests snap to the closest lower supported level, or the minimum accepted level if none is lower. Effort-only and toggle-only APIs cannot enforce a thinking-token budget; numeric settings map to an effort/toggle there. Budget APIs clamp to their published limits and half the effective output window, reserving room for an answer. No wall-clock thinking cutoff is added: transport timeouts and cancellation remain separate.

Manual model declarations can override `supports_thinking`, `requires_thinking`, and `reasoning_options` (e.g. `[{ type = "effort", values = ["low", "high"] }]`). Local backends can declare `thinking_fields` with JSON-shaped BML fragments keyed by `off`, `adaptive`, or effort level. Those fragments are deep-merged into the request. Unknown models fall back to provider/family dialects; discovery is advisory, not a reason to reject an otherwise usable model ID.

`crafty` (binary) supports `--print` (headless, `--output-format text|stream-json`, Claude Code-compatible), `--image PATH`, `-m/--model provider/model`, `-c` continue, `-s/--session` resume, `--fork-session` (copy a `--session`/`--continue` source under a new id without touching the source file), `--session-id` (persistence id for a NEW session; it never resumes), `--mode build|plan`, `--allowedTools` / `--disallowed-tools`, `--permission-mode default|plan|bypassPermissions` (`acceptEdits` rejected as unsupported), `--no-commands`, `--max-turns`, `--system-prompt` overrides, `--yolo` / `-A/--auto-review` (permission policy; enforced in headless runs too), and a set of accepted-but-ignored Claude Code SDK compatibility flags.

Startup prompt: the positional `PROMPT` is honored by `--print` **and** the interactive TUI — `crafty "do x"` submits it as the first message right after provider startup, and piped stdin (`echo "do x" | crafty`, no `--print`) is read the same way (empty/whitespace input starts the TUI normally). The positional wins over piped stdin. In resume sessions (`-c` / `-s`) the prompt waits for the session to load, so its first turn runs in the loaded mode.

Session mode persistence: the TUI persists its mode (build/plan) in the session file on every user-initiated change (Tab toggle, implementing a plan). A resumed session restores its stored mode — the stored mode wins over `--mode`/`--permission-mode plan`, which seed fresh sessions and legacy sessions with no stored mode. Print-mode resume (`-c`/`-s`) restores history and the stored model (unless `-m` overrides it), but not the stored plan/build mode or thinking config — those are TUI/ACP surfaces.

Tool policy flags: `--allowed-tools` **pre-approves** tools (scope-universal allow rules — every tool stays advertised; it does not restrict the tool set), `--disallowed-tools` **denies** tools at the permission gate (calls fail with a permission-denied message; batch children and subagents are covered by the same gate). Both accept PascalCase or snake_case native names (`Read`, `bash`) and MCP forms (`server__tool`, `mcp__server__tool`, `mcp__server` for a whole server); unknown names error with the valid-tool list, and a tool named by both flags is a hard error. The rules live for the process only — they never persist into sessions or `permissions.bml`. `--permission-mode bypassPermissions` matches the `--yolo` posture (mutually exclusive with `-A`), `plan` selects plan mode (conflicts with an explicit `--mode build`), and `--no-commands` skips custom slash-command discovery in the TUI.

Subcommands: `acp` (agent-client-protocol server on stdio), `completions <shell>`, `models`, `stats [--sessions]`, `doctor [--export]` (diagnose and self-heal provider config), `update [-y]` / `rollback` (digest-pinned self-update), `prompt [system|research|general] [--plan|--tools|--names]`, `term init|log|run|info` (shell integration), `recipe list|run`.

Not yet implemented (flagged with warnings at runtime): `--input-format stream-json` (SDK mode), verbose transcript output, flow mode.

## Development

```sh
cargo test -p craft
cargo fmt -p craft --check
cargo clippy -p craft --all-targets -- -D warnings
```

`ProviderHandle::from_config` builds only the selected animist provider; parsing configuration never constructs clients or initiates interactive authentication.

**Provider conformance (offline).** `src/conformance/` drives every supported kind through a real `Conversation` against a local mock HTTP server speaking each protocol (Anthropic Messages, OpenAI Responses/Chat Completions, Bedrock Converse with AWS eventstream framing): text, thinking and tool-call round trips, tool-result image placement (inline where accepted, relocated for Chat Completions), retry/reauth/overflow recovery, Bedrock's in-provider retries, usage, and catalog pricing. It is part of `cargo test` and needs no credentials.

**Opt-in live smoke tests.** Real model calls, `#[ignore]`d and env-gated so CI never pays for inference:

```sh
CRAFT_LIVE_BASE_URL=http://127.0.0.1:8000/v1 \
CRAFT_LIVE_MODEL=orion-26b \
CRAFT_LIVE_API_KEY=…            # optional for keyless local servers
  cargo test -p craft --lib conformance::live -- --ignored --nocapture
```

`CRAFT_LIVE_KIND` selects the config kind (default `openai-compatible`; `llama-cpp`, `omlx`, `ollama`, `anthropic`, ...). The two tests: a plain text round trip, and a tool round trip that must read a file and quote it back. The same env serves a manual QA session: run the TUI against a local model (`provider` block with the same `kind`/`base_url`), exercise tool calls, cancel (`Esc`), resume (`/sessions`), and `/compact`.
