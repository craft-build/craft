# Changelog

All notable changes to **craft** are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.9.4] - 2026-07-18

### Added

- **install**: Windows `install.ps1` installer script (`bf5b690b8`)
- **install**: GitHub token auth supported in installers (`28b9bf8b2`)
- **install**: Git Bash on Windows documented (`df0005144`)
- **update**: resolved install directory shown in the confirmation prompt (`f3b7a4091`)
- **ui**: colored left border for batch subtools (`d34f1bf52`)

### Changed

- **providers**: model-tiers cache uses a multi-tier-friendly format (`beaaa80ed`)
- **tools**: desktop native tool removed (`7cdc45ba3`)
- **agent**: unreliable hashline anchor feature removed (`961de0787`)
- **plugins**: `{ llm_output, is_error = true }` blessed (`4455ef39b`)
- **deps**: ran `cargo update`, refreshing transitive dependencies in the lockfile (`aws-lc-rs` 1.17.3, `aws-lc-sys` 0.43.0, `ignore` 0.4.30, `portable-atomic` 1.14.0, `tokio` 1.53.0)

### Fixed

- **lua**: stray async snapshot and yielding plugin callbacks fixed (`00332b305`)
- **mcp**: OAuth token silently refreshed on 401 instead of restarting (`e9825ffa9`)
- **providers**: JSON object arguments from llama.cpp handled in the Responses API (`9aab5312c`)
- **providers**: reasoning events handled in OpenAI Responses API streaming (`553a47f6e`)
- **cmd**: `auth login` uses a dynamic provider (`255fc4bab`)

## [0.9.3] - 2026-07-17

### Added

- **util**: base58-encoded UUIDv7 entity ids via `CraftId` and `SessionRef` in craft-storage, threaded through the codebase replacing bare session ids, with seamless legacy migration of hex-named sessions (`69ed8d12f`)
- **providers**: `ProviderEvent::PromptProgress` and `AgentEvent::PromptProgress` parse `response.in_progress` SSE events from the llama.cpp `/responses` API and emit prompt-progress data (`dd6f3b2e6`)
- **providers**: OpenRouter discovered model pricing is scaled to $/M and carried through discovery via an optional `ModelPricing` on `ModelInfo` (`63563d17b`)
- **ui**: progress bar widget renders prompt-processing progress in the chat viewport, with a new `progress_bar` theme style (`dea1b71f6`)
- **ui**: prompt-processing progress bar renders its cached portion in a distinct green color (`991afd2f6`)

### Changed

- **deps**: ran `cargo update`, refreshing transitive dependencies in the lockfile (`cfg_aliases` 0.2.2, `self_cell` 1.3.0, `tokio` 1.52.4)

### Fixed

- **providers**: missing `output_index` in the Responses API SSE stream (e.g. llama.cpp) now falls back to `tool_accumulators.len()` for new accumulators (`1151c3749`)
- **providers**: `reasoning` field aliased to `reasoning_content` for vLLM thinking-content compatibility (`f0f4c13a9`)
- **session**: load-path migration drains all legacy files, so a session with both `.json` and `.jsonl` siblings no longer appears twice in the picker (`7bc0c152b`)
- **agent**: `functions.` prefix emitted by Codex-post-trained models is stripped from tool names at the top of dispatch (`caff9d87e`)
- **copilot**: GitHub Enterprise (`*.ghe.com`) hosts supported for token discovery and the Copilot GraphQL endpoint (`c5f9a2eab`)
- **lua**: `json.decode('[]')` now roundtrips back to `[]` instead of `{}` (`312bbb223`)

## [0.9.2] - 2026-07-16

### Added

- **agent**: `insert_lines` split out of `edit_lines` as its own tool for clarity (`6d7f0d838`)
- **agent**: built-in skills are now embedded directly in the binary instead of loaded from disk at runtime (`c66073b40`)
- **agent**: `subagent-briefing` prompt slot and refined flow stage prompts so subagents start with better context (`cb5e0957c`)
- **desktop**: redesigned shell and chat layout with an inline context bar (`6c8042bab`)
- **ui**: redesigned chat layout with a model row, bar indicators, and layered theme colors (`5c0da3dad`)
- **mcp**: headless OAuth via pasted redirect URL, for environments without a browser (`9e4ba1843`)
- **providers**: parse `supports_vision` per model for llama.cpp, OpenRouter, Mistral, and TensorX (`4c1bfd5b6`)

### Changed

- **docs**: warn that `edit_lines` / `insert_lines` must not be used with the batch tool (`ab7aef0cf`)
- **deps**: ran `cargo update`, refreshing transitive dependencies in the lockfile (`bitflags` 2.13.1, `clap` 4.6.2, `globset` 0.4.19, `grep-matcher` 0.1.9, `grep-searcher` 0.1.17, `ignore` 0.4.29, `regex` 1.13.1, `regex-automata` 0.4.16, `uuid` 1.24.0)

### Fixed

- **agent**: guardrail-disable no longer applies to edit tools (`cc8c4cca8`)
- **agent**: compaction buffer is now scaled with the context window (`b13c90fa4`)
- **agent**: the disconnect from the `question` tool to the TUI is fixed (`f4123ebd6`)
- **ui**: streaming buffers are flushed after compaction to prevent message merging (`1a5edb7de`)
- **grep**: path tiebreak added for deterministic ordering on mtime ties (`af65a746b`)
- **desktop**: updated application icon (`ec56e627b`)

### Performance

- **grep**: walker parallelized and mtime sort cached (`4e776073f`)

## [0.9.1] - 2026-07-15

### Added

- **desktop**: YOLO mode (auto-approve permissions) configurable at session start and toggleable at runtime (`08bc2bf5`)
- **desktop**: live todo list rendered in a dedicated panel (`25e8e710`)

## [0.9.0] - 2026-07-15

### Added

- **desktop**: new `craft-desktop` crate, a Tauri + React desktop IDE that drives craft for real over the Agent Client Protocol (one `craft acp` subprocess per session tab, JSON-RPC over stdio); chat streaming, tool calls, and permission prompts are genuine ACP traffic (`53e54e5c`)
- **flow**: Flow mode now works over ACP; `craft-acp` drives `craft_flow::run` directly, surfaces the goal-approval gate as a real permission request, and streams stage/chunk progress as `SessionUpdate::Plan` (`53e54e5c`)
- **agent**: bounded retry (3x, 1s delay) for transient MCP tool failures (`ToolUnavailable`/`Provider` kinds) in tool dispatch, emitting `AgentEvent::Retry` per attempt (`d0e56aa9`)
- **recipes**: `craft recipe list` and `craft recipe run <name>` CLI subcommands, plus a `/recipe` TUI slash command with a recipe picker overlay (`d0e56aa9`)
- **agent**: `/goal` now parses acceptance criteria (`## Acceptance criteria`) and evaluates them via `JudgeOutcome::Criteria`, replacing the prior unimplemented arm (`d0e56aa9`)

### Fixed

- **providers**: TensorX now computes `max_output_tokens` as `context_window - 4096` margin instead of returning `None`, with a 32000 fallback (`d0e56aa9`)
- **agent**: `RecoveryAction::Escalate` at the stream-failure site now emits an `Info` event before returning `Err` instead of being a no-op (`d0e56aa9`)
- **agent**: subagent schema-failure `Stop` is now explicit and non-retriable (`d0e56aa9`)

## [0.8.7] - 2026-07-14

### Added

- **repomap**: new `craft-repomap` crate implementing aider-style repo map via PageRank over a tree-sitter symbol graph; injected as a synthetic message each turn so the model knows the project's shape without spending a tool call (`5e8a00cf`)
- **repomap**: tag extraction for 25 languages (Rust, TypeScript, Python, Go, Java, C/C++, Ruby, Lua, Bash, Kotlin, Swift, C#, Elixir, Scala, PHP, HTML, Gleam, Dart, Starlark, Nix, Zig, CSS, Fish, Perl) with definition/reference captures and personalized PageRank ranking boosted by mentioned identifiers and in-context files
- **repomap**: `/map`, `/map-refresh`, and `/map-toggle` slash commands; `repomap { enabled, max_tokens }` config under the `[agent]` section; mtime-keyed tag cache that persists across turns so unchanged files are not re-parsed
- **watch**: editor-agnostic watch mode (`craft-ui/src/watch.rs`) that detects AI comment markers (`// AI!`, `# AI?`, `-- AI`) in files as you edit, then injects them as prompts; three actions: code change (`!`), ask (`?`), add context (bare); `/watch` slash command toggles the watcher; `watch { enabled }` config (default off)
- **providers**: per-model `supports_vision` flag in the static catalog, threaded into `Model` as the default vision override so `model.vision()` resolves without runtime probing (`d5b8550c`)

### Fixed

- **multiedit**: error messages now quote the failing `old_string` (truncated) instead of a 0-indexed `edit {i}` position, so the model can match by content instead of miscounting array entries (`55ecca85`)

## [0.8.6] - 2026-07-13

### Added

- **agent**: self-calibrating token estimation multiplier that raises the estimate after context overflows, so proactive compaction fires earlier and breaks the overflow/emergency-compact loop (`d8eee519`)
- **permissions**: MCP permission scoping via a `ToolKey` enum replacing flat string identifiers, with `[mcp.server]` TOML nesting, a migrator from old `mcp:server__tool` keys, and wire/internal name conversion at the provider boundary (`4c6c7b84`)
- **providers**: parse per-model thinking support from provider listing endpoints, including the OpenRouter reasoning object, TensorX `supported_openai_params`, and Mistral `capabilities.reasoning` (`e9de682c`)

### Fixed

- **session**: rewind no longer leaves ghost subagent tabs; new `Session::prune_orphans` retains only state reachable from remaining messages, also healing sessions already corrupted by the old bug (`fbe3ec3f`)
- **agent**: todo list now survives compaction via a `## Todo list` section in the summary template (`a9128320`)
- **permissions**: empty tool keys no longer crash the native assert; in-memory migration and deduped TOML helpers (`a49923dc`)
- **agent**: context size estimate is updated after rewind (`0cbe04ba`)

## [0.8.5] - 2026-07-12

### Added

- **wiki**: adopt OKF frontmatter and add project-init orchestrator (`1c21e82a`)
- **flow**: recovery classification taxonomy mapping tool/subagent/reconciliation failures to typed kinds for structured logging (`886fd43e`)
- **flow**: drift reconciliation on resume, detecting and repairing mismatches between persisted workstream state and on-disk stage/chunk docs (`886fd43e`)
- **flow**: `FlowOutcome::NeedsReview` soft-warning path for verifier `needs_review` status, surfaced in the CLI, TUI agent loop, and flow panel (`886fd43e`)

### Fixed

- **flow**: stage doc missing repair now clears `ws.stage` so `detect_drift` stops re-finding it (`886fd43e`)
- **flow**: removed dead `DriftKind::CorruptWorkstreamState` variant and unreachable `UnresolvedDrift` match arms (`886fd43e`)
- **agent**: include the concrete validation error in the subagent schema-exhaustion message (`886fd43e`)

## [0.8.4] - 2026-07-11

### Added

- **wiki**: in-project local wiki knowledge base (`890e7355`)
- **agent**: detect sed/perl in-place edits and snapshot affected files (`06e6915f`)
- **bash**: capture image data URIs in stdout as image content blocks (`eb94a964`)
- **ui**: context-window override commands and persistence (`31c05552`)
- **ui**: tabbed provider layout to model picker (`b6f63f11`)
- **ui**: `show_thinking` option to control model reasoning display (`e78a0043`)
- **providers**: `/v1/responses` endpoint support for local providers (`24c1708f`)
- **providers**: opencode `enable_free_models` option, default false (`2ef435d4`)

### Fixed

- **lua**: run restore async tasks inline so highlighting survives expand (`c7b756c9`)

### Changed

- **lua**: collapse `permission_scope` into `permission_scopes` (`688b045b`)
- **lua**: let plugins talk through events and slots (`14841ffb`)
- **lua**: unify error pairs and keyed `ctx:config` (`b9b54a0e`)
- **config**: derive `Deserialize` for `ToolOutputLines` (`52d1eef2`)
- **agent**: extract `ToolDoneEvent` flattening into `interpreter_bridge` (`328bfcaa`)
- **providers**: centralize auth request header setup (`f09cd0eb`)
- **docs**: add time-traveling stream rules (ttsr) page (`8997671d`)

## [0.8.3] - 2026-07-10

### Added

- **tools**: new `desktop` tool driving native desktop applications through the
  platform accessibility tree (AXUIElement on macOS, AT-SPI2 on Linux,
  UI Automation on Windows) via the `xa11y` crate. A dedicated worker thread
  owns the blocking session so it persists across calls. 19 actions: `status`,
  `apps`, `connect`, `active`, `disconnect`, `tree`, `dump`, `read`, `find`,
  `screenshot`, `click`, `type`, `fill`, `press`, `scroll`, `wait`, `select`,
  `subscribe`, `next_event`. Action failures propagate as errors to the trust
  tracker, guardrails, and the model. (`41b94841`)
- **tools**: tools can now return images the model actually sees, not just text.
  Lua tools return `image = { media_type, data }` alongside `llm_output`, mapped
  to `ToolOutput::Image`. Ships the bundled `view_image` Lua plugin built on two
  new Lua APIs: `craft.base64` (mirrors `vim.base64`) and `craft.image`
  (probe/decode/resize/encode). Models without vision never learn `view_image`
  exists (`Model::vision()` gate, `capability_exclusions`), and image blocks are
  dropped to a text note for text-only models at request time. Ported from maki
  commit 7b2c657 by Dvir Hatabi. (`e96e0d2d`)
- **openai**: register `gpt-5.6-luna` (weak), `gpt-5.6-terra` (medium), and
  `gpt-5.6-sol` (strong) as the new OpenAI defaults, each with a 372K context
  window and 128K max output. Routed through the Coding Plan via
  `coding_plan_context_window` (372K for `gpt-5.6-*`, 272K otherwise). The
  older `gpt-5.4-nano`, `gpt-4.1`, and `gpt-5.5` entries drop to `default: false`.
  (`58c719b9`)

### Fixed

- **ui**: guard image paste (file and clipboard) and the paste-completion
  handler with a vision-capability check, so models without image input support
  are rejected with a status bar message instead of silently attaching.
  (`3d2bbc7a`)
- **deps**: ran `cargo update`, refreshing transitive dependencies in the
  lockfile (`ignore` 0.4.28, `lru` 0.18.1, `regex` 1.13.0,
  `regex-automata` 0.4.15, `zlib-rs` 0.6.6).

## [0.8.2] - 2026-07-08

### Changed (breaking)

- **tools**: The four browser tools (`browser_screenshot`, `browser_navigate`,
  `browser_click`, `browser_text`) are replaced by a single unified `browser` tool
  with an `action` parameter. The underlying driver migrated from `chromiumoxide`
  to `playwright-rs` (Playwright protocol), enabling session persistence across
  calls, multi-tab support, form filling, keyboard input, JavaScript evaluation,
  element discovery, scrolling, waiting, and screenshot regions. 18 actions total.
  Migration: `browser_screenshot {url}` -> `browser {action:"screenshot", url}`;
  `browser_navigate {url}` -> `browser {action:"open", url}`; `browser_click {url,
  selector}` -> `browser {action:"click", url, selector}`; `browser_text {url}` ->
  `browser {action:"get_content", url}`. Pages now persist across calls; use
  `browser close_tab` for cleanup. The browser requires the Playwright driver
  (`npm install -g playwright@1.60.0` or set `PLAYWRIGHT_DRIVER_PATH`).
  (`140ad807`)

### Fixed

- **bedrock**: clients are cached so repeated calls reuse the same provider
  instance instead of rebuilding it. (`e84f1431`)
- **ui**: removed an unused `cell_at` test helper that triggered a dead_code
  clippy error. (`400be1c8`)
- **deps**: ran `cargo update`, refreshing transitive dependencies in the
  lockfile (`bytes` 1.12.1, `der` 0.8.1, `exr` 1.74.1, `inotify` 0.11.4,
  `jiff` 0.2.32, `rustc-demangle` 0.1.28, `zerocopy` 0.8.54).

## [0.8.1] - 2026-07-08

### Added

- **providers**: Bedrock provider via the official AWS SDK for Rust
  (`aws-sdk-bedrockruntime` ConverseStream for the data plane,
  `aws-sdk-bedrock` ListInferenceProfiles for discovery), gated behind the
  `bedrock` cargo feature. Auth uses the full SDK credential chain (env,
  profiles, SSO, IMDS, web identity, container creds) via
  `aws_config::load_defaults`; agent `Timeouts` thread into an SDK
  `TimeoutConfig`. Short Anthropic model ids from the built-in list are
  resolved to fully qualified inference profile ids at request time using a
  region-derived prefix (us./eu./apac./global.) matching the SigV4 region,
  since Bedrock rejects the short ids with a ValidationException; already-
  Bedrock-shaped and unknown ids pass through unchanged. (`47c2ec3e`,
  `00d2da2a`)

### Fixed

- **ui**: stray spaces dropped when copying CJK/emoji selection. Wide
  characters leave continuation cells in the ratatui buffer holding a
  placeholder space, so copying a CJK selection inserted a space between
  every glyph; each glyph's display width is now tracked and its
  continuation cells skipped. (`5c00520e`)
- **agent**: doom stagnation gated on no tool-success progress. Stagnation
  detection fired on legitimate long research sessions because consecutive
  read/grep-heavy turns cluster semantically, accruing +3 per turn with no
  actual loop present. `note_stagnation` now only adds to the score after
  3 consecutive similar turns without an intervening successful tool call;
  `note_tool_success` and `reset_for_new_user_input` clear the counter, so
  productive research breaks the stagnation chain while real loops stay
  caught. (`d854fac2`)
- **tool-macro**: raw identifiers unrawed in JSON schema keys. `r#` prefixes
  (e.g. `r#as` in `browser_click`) leaked into tool input_schema property
  keys via `Ident::to_string()`, and the `#` violated Anthropic's
  `^[a-zA-Z0-9_.-]{1,64}$` pattern (HTTP 400 on Bedrock's Anthropic-
  compatible API). `IdentExt::unraw()` runs before stringifying so emitted
  keys match the source-level name. (`5eefff14`)

## [0.8.0] - 2026-07-06

### Added

- **flow**: Flow mode, a third agent mode (alongside Build and Plan) that
  runs a feature through named stages (Scout, TPM, Plan, Req, Execute,
  Review, QA, Integrator, Verifier), each under a dedicated `flow_*`
  model role, pausing for goal approval before execution and producing a
  verification report at the end. Backed by a new `craft-flow` crate
  (state machine, document schemas, prompt templates), a `flow` storage
  namespace with per-workstream persisted documents and a semantic
  embedding index powering a `flow_search` tool, `FlowConfig` plus nine
  `flow_*` ModelRole variants, `AgentMode`/`Mode`/`StoredMode` Flow
  variants with a 3-way toggle, `StopReason::AwaitingGoalApproval`, and a
  `craft flow` CLI with `gc --older-than` and JSON output for gate/
  verdict stop reasons. (`f1801923`)
- **flow**: resume/retry. Mutable workstream state (stage, approval flag,
  chunk statuses, iteration counts) is persisted to `workstream.json` via
  `FlowStore`, so a crashed or failed run re-enters at the right stage
  instead of restarting. `FlowParams::resume` rehydrates and skips
  Scout/TPM/Plan when their docs are on disk, `run_chunk` emits its own
  Done transition, and `craft flow -s <id> --retry` resumes a failed
  workstream. (`3bc1ef73`)
- **ui**: `/usage` command with a token breakdown per model and quota
  display, backed by a new `ProviderUsage` type and `fetch_usage` trait
  method. (`26319dd8`, `cd5c0aa1`)
- **providers**: Anthropic OAuth usage quota. `fetch_usage` calls the
  beta usage endpoint (bearer auth against `api.anthropic.com` only, since
  API keys have no subscription quota), rendering `Current session`,
  `Current week (all models)`, scoped per-model rows, and `Usage credits`
  via `UsageLimit::detail`; the flat `five_hour`/`seven_day*` windows
  remain a parse fallback. (`0481157d`)
- **ui**: `Ctrl+R` reloads the quota in the `/usage` modal via
  `Action::RefreshUsage`, with a styled hint on the popup border.
  (`497b02cc`)
- **ui**: Ayu light and mirage themes. (`ce8f6d3e`)
- **ui**: recent models section in the `/model` picker. (`6e021ad1`)
- **ui**: `PageUp`/`PageDown` scrolling in pickers. (`b0724a7f`, credit:
  Luca Barbato)
- **print**: image files attached as vision content. (`204104b6`)
- **storage**: per-model token usage accounting in sessions.
  (`9322027b`)

### Fixed

- **flow**: Flow stage subagents now get prompt-level instruction that the
  `flow://` internal URL scheme exists (so they fetch prior-stage docs via
  the read tool), and `find` invoked against a filesystem root
  (`find /`, `find /home`, `find /Users`, ...) is hard-denied in the bash
  plugin, since these full-disk scans hang when the agent falls back to
  them after a `flow://` error. (`48a44105`)
- **providers**: `spec_for_tier` checks the static model table between
  user overrides and positional fallback, fixing tier resolution for
  providers like DeepSeek where the discovered-models order does not match
  annotated tiers (running Opus now correctly picks Sonnet for `medium`
  and Haiku for `weak` without user config). (`3b072d1a`)
- **agent**: the task tool keeps the dynamic provider slug when resolving
  subagent tiers, skipping the base registry lookup when the model carries
  a dynamic slug while tier overrides still apply. (`aea0413e`)
- **plugins**: the memory tool expands to file content instead of the
  summary. (`34280cea`)
- **question**: CJK/multibyte input allowed in the custom answer field.
  (`a175d122`)
- **storage**: tests isolated from the host env — `ToolRegistry` injected
  via `ToolContext` and a pure combiner removes host-env dependence.
  (`c5ba1dcb`, `8347e40e`, credit: Sam Mohr)

## [0.7.6] - 2026-07-05

### Added

- **tools**: `edit_lines` tool for line-number-based replacements. `read`
  and `grep` print lines as `<nr>: <content>`, so `edit_lines` takes
  `path`, `start`, `end` (optional, defaults to insert-before-start), and
  `new_string`; empty `new_string` deletes the range and out-of-range
  starts/ends are rejected before any write. Built as a native Rust tool
  (inclusive-range splicing with boundary validation belongs with
  `edit`/`multiedit`/`write`), and gated behind `OPT_IN_TOOLS` so it
  stays off unless explicitly enabled. (`81d2b7c`, credit: Tony
  Solomonik)
- **agent**: nudge on empty response after tool calls. Weaker models
  sometimes end the turn with no text and no tool uses right after
  running tools, silently abandoning a multi-step task; a synthetic user
  message asking the model to process the tool results and continue is
  now injected, guarded by a per-round flag and a recent-tool-results
  check. (`b5ffa9d`, credit: Gabriel FORTE)
- **providers**: Opencode provider with catalog-based model discovery.
  Models are addressed as `opencode/<sub-provider>/<model-id>` and
  discovered from the remote models.dev catalog plus the Opencode Zen
  API, cached under the cache dir with a 24h TTL; requests route to
  OpenAI-compatible chat/completions or Anthropic Messages endpoints
  based on the catalog npm tag. (`09df769`, credit: Zsombor Gegesy)
- **providers**: per-model `supports_thinking` instead of per-provider.
  `Model` now carries a `supports_thinking_override` (also settable in
  `ModelDef` config and dynamic provider scripts), which Mistral flips
  off for `ministral-*`. The per-dialect effort match arms collapsed
  into `apply_reasoning_effort(body, EffortScale)`, and
  `RequestOptions::clamped(model)` runs before every request so thinking
  and fast flags never reach a model that cannot honor them.
  (`2711ea1`, credit: Tony Solomonik)
- **ui**: `rose_pine_moon` and `rose_pine_dawn` themes, the two missing
  Rosé Pine variants from the canonical rosepinetheme.com palette (Moon
  is darker, Dawn is the light variant). (`d969474`, credit: Sam Mohr)
- **ui**: `SessionReset` autocmd fired on `/new` so plugins can clear
  per-session state (e.g. todos) when a new session starts. (`8a0c5e9`)

### Changed

- **providers/copilot**: explicit login required, ambient `gh` fallback
  dropped. `load_token()` now only checks env vars and saved craft
  credentials; the ambient scan of gh/Copilot config files moves into a
  private `discover_token()` used solely by `login()`, which imports the
  discovered token into craft's credential store. `logout()` now deletes
  the saved credential instead of erroring. (`ef38cbb`, credit: Sam Mohr)
- **providers**: `fetch_and_parse_models` helper hoists the GET `/models`
  + JSON parse + sort boilerplate out of the per-provider
  `list_models_with_info` implementations; Mistral and OpenRouter pass
  their own parsers. (`fdb7303`, credit: Sebastian Dröge)

### Fixed

- **lua**: click handlers scoped to `TaskCell` to stop VM memory
  exhaustion. Every `buf:on("click", cb)` pinned the closure in a global
  `ClickHandlerMap` keyed by tool id forever; the closure captured the
  `ToolView` holding the entire tool output in Lua, so long sessions
  accumulated pinned state until the 512MB `LUA_MEMORY_LIMIT` hit and
  every tool failed with `memory error`. Handlers now live in
  `TaskCell.click` and die with the tool invocation; `gc_collect()` also
  runs on `TurnEnd`. (`a9579ff`, credit: Tony Solomonik)
- **permissions**: plan file writes auto-allowed. Two gatekeeping systems
  (`tool_dispatch` plan-mode restriction and `PermissionManager`
  permission rules) both needed to know the plan file is special but
  only `tool_dispatch` did, so writing the plan file (which lives outside
  `{cwd}`) always triggered a prompt. `AgentMode::plan_path()` now
  threads the plan path through `check`/`check_multi`/`enforce`.
  (`8957d69`, credit: Tony Solomonik)
- **ui**: `/model` picker selection preserved across async refresh.
  `ModelPicker` tracked the current model by index, but `replace_items`
  reset selection to 0, so asynchronously arriving models always jumped
  back to the first item; a `current_spec` field now looks up the
  correct index after items change. (`54b70e4`, credit: Tony Solomonik)
- **ui**: Rosé Pine themes standardized to the canonical palette.
  Regenerated all three variants (base, moon, dawn) from one template so
  structure is identical, with palette values matching the official
  rose-pine/neovim `palette.lua` including highlight-high colors.
  (`df67678`, credit: smores56, tontinton)
- **storage**: state and logs dirs corrected on Windows. `xdg_sibling()`
  walked up from `data_dir` to find sibling folders, which worked on
  Linux (`~/.local/share` -> `~/.local/state`) but on Windows produced
  bogus paths like `~/AppData/state/craft/`; replaced with
  `etcetera::BaseStrategy::state_dir()` with a `data_dir` fallback.
  (`aa681c8`, credit: Tony Solomonik)
- **providers**: `user-agent` header added to the `get_text`/`post_text`
  helpers, which were missing the header that `build_request()` already
  sets, so all outgoing requests now identify the agent consistently.
  (`0748337`, credit: Sebastian Dröge)
- **providers/dynamic**: test scripts fsynced before spawn. Tests wrote
  discovery executables and spawned them immediately, racing the kernel
  page cache flush and surfacing `ETXTBSY` under parallel test load; the
  test helpers now fsync the script file first. (`193eafa`, credit: Chris
  Lee)

## [0.7.5] - 2026-07-02

### Added

- **agent**: no-LLM VCC compaction tier. Ports pi-vcc's deterministic
  structured-summary compaction as a third tier that runs before the LLM
  compactor. On overflow, VCC builds a bracket-tagged summary of the older
  history and keeps a task-boundary-aware tail; if that brings context under
  the limit the LLM call is skipped entirely (cost + latency win), otherwise
  it falls through to the existing `compact_history`, which remains the
  safety net. The new `vcc_recall` tool offers lossless session-JSONL search
  across compactions (regex/term query, expand, page). Also fixes UTF-8
  panics found on review: `compress_bash`, the bash command display, and the
  bash-output scan window sliced at raw byte indices and could panic on
  multibyte input; the shared `util::clip`/`clip_sentence` had the same
  latent bug. All now floor to a char boundary, covered by regression tests.
  (`fb87d80`)

### Changed

- **ui**: model tier shortcuts are now `!@#$` instead of `1234`, since digits
  are far more common in model names than those symbols and the old keys
  blocked searching for model names containing numbers. The corresponding
  keys for various other keyboard layouts are also considered.
  (`f3a992e`)

### Fixed

- **agent**: edits outside the cwd now prompt instead of hard-blocking. The
  old `check_physical_boundary` hard-blocked every write/edit/multiedit
  outside cwd, even normal cross-project edits, labeling them all as symlink
  escape. Renamed to `boundary_block_reason`, it now only hard-blocks truly
  unverifiable paths (where the project root cannot be resolved); everything
  else, including paths outside cwd and even symlink escapes, flows through
  the normal permission prompt. (`9a824a3`)

## [0.7.4] - 2026-06-29

### Added

- **providers/openrouter**: model discovery from the API. `GET /models` is
  parsed into `ModelInfo` entries, skipping anything that is not text-to-text
  (`architecture.input_modalities` and `output_modalities` both containing
  `text`) and reading `context_length` into `context_window`. `list_models`
  now derives from `list_models_with_info`, matching tensorx. (`080de259`,
  credit: Sebastian Dröge)
- **providers/copilot**: `claude-opus-4.7` added to the model table with the
  real Copilot API values (context_window 264_000, max_output_tokens 64_000)
  instead of falling through to the inflated provider fallback that both
  over-proposed request size and pushed the compaction trigger past its
  buffer. (`91d82332`, credit: Noah Baculi)
- **providers/mistral**: refreshed model list, using mistral-medium as the
  strong model, mistral-small as medium, and ministral-14b as the weak model
  (devstral 2 is deprecated in favour of mistral-medium). (`570e4cc6`,
  credit: Sebastian Dröge)
- **providers**: Mistral added to `accepts_arbitrary_models()` so every model
  listed via the `/v1/models` endpoint can be selected and has its information
  stored. (`be4812db`, credit: Sebastian Dröge)
- **agent**: the active model spec is now printed in the Environment block of
  the system prompt (`- Model: <spec>`), so the agent knows which model it is
  running as. `build_system_prompt` takes a `&Model`; headless, the TUI agent
  loop, and the `craft prompt` debug subcommand pass the active (or persisted/
  default) model. (`56de84cb`, credit: Luca Barbato)
- **websearch**: when `EXA_API_KEY` is set it is sent as the `x-api-key`
  header. (`88d20887`, credit: oldhu)

### Changed

- **lua**: `api/` reorganized so each file maps 1:1 to a `craft.*` namespace.
  `async_api.rs` and `fn_api.rs` dropped the `_api` suffix (raw `r#async` /
  `r#fn` identifiers), `buf.rs` and `win.rs` moved under `ui/`, and
  `command.rs`, `ctx.rs`, `setup.rs` moved into a new `util/` subdirectory
  alongside the `err_pair` / `json_to_lua` helpers (formerly in `mod.rs`).
  **Breaking** for plugins reaching into internal module paths. (`943f25e3`)

### Fixed

- **agent**: compaction overflow check no longer reserves
  `max_output_tokens` in the context window. On models where
  `max_output_tokens == context_window` (both 262K) this made usable = 0 and
  triggered auto-compaction every turn; only the user-configurable
  `compaction_buffer` is now reserved. (`4f776b26`)
- **providers/mistral**: reasoning handling. Ministral does not support
  reasoning and setting `reasoning_effort` for it fails the request; models
  that do support reasoning now gate the field correctly. (`0f95c4e3`,
  credit: Sebastian Dröge)
- **providers**: read the context window from `max_context_length` (the field
  name Mistral uses) as a fallback, and skip non-chat Mistral models (those
  without `capabilities.completion_chat: true`, e.g. OCR or voice). (`e8e5d25b`,
  credit: Sebastian Dröge)
- **plugins/skill**: honor `tool_output_lines.other`. The skill plugin read a
  nonexistent `tol.skill` field, so setting `tool_output_lines.other` in
  `init.lua` silently fell through to the hardcoded 20-line default and
  setting `tool_output_lines.skill` was rejected at config load. It now reads
  `tol.other` like glob, grep, and memory, keeping the 20-line fallback.
  (`671ffba2`, credit: Noah Baculi)
- **websearch**: the handler called `os.getenv`, which Luau's sandbox sets to
  nil, failing with "attempt to call a nil value". Added `os_getenv` to the
  `craft.uv` table (guarded by `Permission::Env`, mirroring `vim.uv.os_getenv`)
  and switched the plugin to it. (`660570cb`, credit: oldhu)

## [0.7.3] - 2026-06-29

### Added

- **ui**: user-configurable keybindings via the `ui.keybindings` config field,
  applied as an overlay on the compile-time defaults (`KeybindingResolver`,
  `action_id` on every `KEYBINDS` entry). Canonical chord labels, effective
  chords in the help modal, and disabling an action with an empty list.
  (`b020bc00`)
- **ui**: persistent append-only cost ledger (`cost.jsonl` in craft-storage,
  concurrency-safe atomic appends), a `/stats` modal, and a `craft stats`
  read-only CLI subcommand. (`b020bc00`)
- **cli**: `craft completions <shell>` generating shell completions via
  `clap_complete` (bash/zsh/fish/elvish/powershell). (`b020bc00`)
- **agent**: `ast_edit` and `resolve` tools staging a dry ast-grep rewrite in
  a session-scoped `PendingEditStore` with re-verification against on-disk
  content and a safety backup before writing. (`75142a33`)
- **agent**: `read` tool gains `rule://` and `agent://findings` URL schemes
  (alongside `skill://`, `conflict://`), and `conflict://N` is now numbered
  globally across the repo. (`75142a33`)
- **agent**: model roles (default/smol/slow/plan/commit/advisor) with ordered
  fallback chains loaded from `model_roles.toml`. `stream_with_retry`
  advances to the next chain entry when key rotation is exhausted on
  429/quota, resets `RetryState` on successful rotation, and round-robins
  multiple API keys per provider from `providers.toml` (`api_keys`). Ctrl+P
  keeps cycling globally. (`75142a33`)
- **agent**: always-on lightweight advisor (off by default) reviewing the
  transcript delta each turn and emitting at most one deduped
  nit/concern/blocker note through an emission guard; resets on compaction.
  (`75142a33`)
- **agent**: TTSR stream rules (`ttsr.enabled`) matching the in-flight stream
  text against regex rules from `.craft/rules/*.md` and injecting a firing
  rule as a system reminder, with once/after-gap:N repeat policies. (`75142a33`)
- **agent**: `browser_navigate`, `browser_click`, and `browser_text` tools
  reusing the shared headless-Chromium launch path. `browser_navigate`
  returns URL + title + body markdown; `browser_click` navigates, clicks the
  first CSS-selector match, and returns text or a screenshot via an `as`
  param; `browser_text` reads `document.body` or a scoped element's
  `innerText` (capped at 64 KiB with a truncation marker). All reject
  non-http(s) URLs and carry the same `url:<url>` permission scope as
  `browser_screenshot`. (`a6d1fab2`)
- **ui**: inline images via `ratatui-image` v11 (Kitty/Ghostty/iTerm2, with
  unicode halfblock fallback under tmux/screen), OSC-8 clickable `file://`
  hyperlinks on tool headers, finding file:line refs, and image captions,
  and CSI 2026 synchronized output wrapping every frame draw to eliminate
  partial-frame flicker. Images are modeled as a dedicated `Segment` variant
  with independent height/scroll math. (`6ccd8df2`)

### Fixed

- **ui**: stale `audience_matrix_is_locked` test (expected 26 native tools,
  now 31) refreshed with the 5 missing entries and tool-name constants.
  (`6ccd8df2`)

## [0.7.2] - 2026-06-28

### Added

- **tools**: `browser_screenshot` tool that renders a URL in headless Chromium
  and returns a PNG, plus multimodal tool-result support so images from tools
  reach the model across all provider families (Anthropic content arrays,
  Google `inlineData`, OpenAI Chat Completions follow-on image messages, and
  OpenAI Responses parts arrays). Images are stripped on compaction.
  (`c78da94e`)
- **providers**: TensorX provider, an OpenAI-compatible open-weight host
  (zero data retention, prompt caching) with models discovered from
  `/model/info` and a `thinking` flag for DeepSeek-V4. (`8ddf842c`, credit:
  Sebastian Dröge)
- **cli**: `craft prompt` subcommand that prints the fully rendered system
  prompt (including Lua plugin contributions), with `system`/`research`/
  `general` variants, `--plan`, and `--tools` / `--tools --names` flags.
  The positional `prompt` field was renamed to `initial_prompt` to avoid a
  clap clash. (`1a0d91ba`)
- **lua**: `craft.api.set_prompt` lets plugins and `init.lua` override the
  singleton identity and tone prompt slots (last-plugin-wins with built-in
  defaults), distinct from aggregate slots that join contributions.
  Registering a hint to a nonexistent slot now errors at registration time;
  empty static content is rejected. (`80dac83b`, credit: TheGoddessInari)
- **ui**: fuzzy matching in the list picker via `nucleo_matcher`, replacing
  substring matching. (`8d5acf90`, credit: Sebastian Dröge)

### Changed

- **agent**: `Slot` already derives `EnumIter`, so the hand-maintained `ALL`
  array in the prompt assembler was dropped in favor of iterating the enum
  directly, removing a drift risk where an unhandled variant would leak its
  `{{marker}}` into the prompt. (`8fc7e01e`)

### Fixed

- **agent**: plan-mode files live outside the cwd, so `check_physical_boundary`
  rejected them even after the plan-mode gate approved the path. The boundary
  check was moved out of `enforce_permission()` and co-located with the
  plan-mode gate, sharing one `mutable_path()` block so the two checks can no
  longer drift apart. (`f2207468`, credit: Tony Solomonik)
- **storage/permissions**: Windows path handling and separators. Adds
  symlink-aware cross-platform path normalization (`normalize_path`,
  `canonicalize_clean`, `incremental_canonicalize` stripping `\\?\`
  prefixes), fibonacci-backoff retry renames for atomic writes (Windows virus
  scanner compatibility), `std::fs::File::lock` replacing `libc::flock`
  (MSRV bumped to 1.89), component-based `scope_matches`, and
  `check_physical_boundary` to block symlink-escape on mutable tool paths.
  Gets permissions, config, and memories working on Windows.
  (`0a81ba7f`, credit: TheGoddessInari)
- **providers**: dynamic-vs-custom slug provider resolution centralized into
  a shared `provider_for_slug` helper so the branching cannot diverge.
  (`ebf31639`, credit: oldhu)

## [0.7.1] - 2026-06-27

### Added

- **providers**: Ollama model discovery via `POST /api/show`, detecting context
  window size per model with a two-tier fallback (`model_info.*.context_length`,
  then `num_ctx` parsed from the parameters string). Adds a `post_text()`
  helper to `OpenAiCompatProvider` and consolidates discovery modes into a
  `DiscoveryMode` enum (`None`, `LlamaCpp`, `Ollama`).
  (`cef7c2f2`, credit: Gabriel FORTE)
- **providers**: custom-provider models can now be declared statically in
  `providers.toml` via a `models` array on each `ProviderDef` (`[[slug.models]]`),
  so the model picker is populated without a `GET /models` round trip on every
  startup. Declared values carry `context_window`, `max_output_tokens`,
  `supports_tool_examples`, and flattened pricing through to `ModelInfo` and
  `Model::from_spec`. (`cf4f16d8`, credit: cybershape)
- **providers**: every provider HTTP request now sends a custom
  `craft/v<version>-g<git-short-hash>` User-Agent instead of reqwest's default,
  with the git short hash captured at compile time via a build script.
  (`50055b9d`, credit: Chris Lee)
- **permissions**: "Allow always" on an MCP tool (`mcp:<tool>`) now generalizes
  the stored scope to `*`, mirroring how `bash` generalizes `cargo test` to
  `cargo *`. The rule's `mcp:<tool_name>` tool field still gates which tool it
  applies to, so distinct MCP tools stay distinct. (`07fe8d65`, credit: Matt
  Van Horn)
- **ui**: the dollar cost is hidden from the status bar and per-turn usage when
  all pricing fields are zero (e.g. local Ollama/llama.cpp providers), instead
  of showing a meaningless `$0.000`. (`c38b2b30`, credit: g4bwy)
- **ui**: "Refine plan" option at the top of the plan-completion menu, which
  dismisses the form so the user can keep iterating on the plan before
  committing to implementation. (`4fc32ad3`)
- **languages**: `.ixx` (C++ module interface) recognized as C++ across the
  tree-sitter parser lists, the formatter, and the styleguide language
  resolver, harmonizing the C++ extension set
  (`cpp cc cxx hpp hxx hh ixx`) across all surfaces. (`0dd93c7d`, `db9c3bf7`)

### Changed

- **deps**: ran `cargo update`, refreshing transitive dependencies in the
  lockfile (e.g. `ast-grep-core` 0.43.0, `clap` 4.6.1, `cc` 1.2.65,
  `fastembed` 5.17.2, `criterion` 0.8.2, `ratatui` 0.30, `reqwest` 0.13.4,
  `syn` 2.0.118, `time` 0.3.x).
- **providers**: configured model tiers are now validated.
  (`ae8507e1`, credit: cybershape)
- **agent**: improved wording in the `read`, `outline`, and `grep` tool
  descriptions. (`942734c6`, credit: sdroege)
- **docs**: added MCP tools to the scope-generalization list in the permissions
  page, noting that `*` is per-tool so allowing `mcp:fetch` does not cover
  `mcp:exec`. (`3c92dc5a`)

### Fixed

- **ui**: opening `/model` now refetches the model list from providers instead
  of showing the stale list cached at startup. (`3bb62b60`, credit: g4bwy)
- **providers**: dynamic providers use the models declared by their own
  `models` script (with `context_window` / `max_output_tokens`) when listing
  models, instead of bypassing them and re-fetching from the upstream provider,
  which had dropped per-model metadata for IDs the upstream did not know.
  (`68c996c3`, credit: Chris Lee)
- **providers**: dynamic-provider slugs on Windows no longer include the
  trailing `.exe` / `.bat` / `.cmd` / `.ps1` suffix, which previously failed
  the valid-slug check. (`05b2211f`, credit: TheGoddessInari)

## [0.7.0] - 2026-06-27

### Added

- **agent**: unified project-scoped discovery module that walks the working
  directory ancestors plus the global config dir, with closer scopes shadowing
  farther ones by name. Shared by checks, recipes, and skills.
- **agent**: recipes — declarative, parameterized session blueprints (YAML or
  JSON) with typed parameters, `minijinja` templating, sub-recipe composition,
  and settings overrides. Run via `craft run recipe.yaml`.
- **cli**: `craft review` runs deterministic, parallel review checks discovered
  from `.agents/checks/*.md` (project-scoped, shadowing global), with a
  file-sharded main pass over the current diff. Findings reuse the shared
  `Finding`/`Priority` types so they render through the same pipeline as the
  `review` tool; check frontmatter `turn-limit` and `tools` are passed through
  to each subprocess as `--max-turns` and `--allowed-tools`; the styleguide
  tools and `report_finding` are enabled by default so checks can ground
  findings in rule IDs. Supports `--dry-run`, `--check-filter`, `--severity`,
  and `-m`.
- **cli**: `craft term` shell integration. `craft term init <bash|zsh|fish>`
  prints a hook script that logs every command to a per-directory history and
  defines an `@craft` alias; `craft term run` injects recent history into a
  headless query; `craft term log` and `craft term info` round out the surface.
- **cli**: `craft doctor` diagnoses the current provider/model, self-heals by
  iterating alternative providers and persisting the first working one, and
  exports a structured JSON diagnostics report with `--export`.
- **cli**: `craft run` runs a headless agent query from a prompt or a recipe
  file, with `--param key=value` overrides, `--no-session`, `--quiet`, and
  `--output-format`.
- **agent**: compaction now progressively drops tool responses (10/20/50/100%,
  oldest first, originals stored for `retrieve`) when the LLM compaction call
  itself overflows, after round truncation is exhausted.
- **config**: `agent.format` — opt-in auto-format-on-edit. After the agent
  writes files, runs the formatter mapped to each file's extension (`rustfmt`
  for `.rs`, `prettier --write` for `.ts`/`.json`, `black` for `.py`, `gofmt`
  for `.go`, `shfmt` for `.sh`/`.bash`, `clang-format -i` for C/C++, `stylua`
  for `.lua`), in place and before the compile check. Missing formatters are
  silently skipped; a custom `command` overrides the extension table. Surfaces
  a terse `format` tool result listing changed paths, or an error event on
  failure (no doom-loop signal). Default off.

## [0.6.5] - 2026-06-25

### Added

- **providers**: ported llama.cpp model discovery for all 3 server modes.
  (`e8874f77`)
- **agent**: compaction now retries on context overflow before falling back to
  the static compaction. (`fa5ad220`)

### Changed

- **agent**: old tool results are stripped before compaction, reducing wasted
  context. (`65de3bd3`)
- **agent**: context size is tracked more accurately for auto-compaction.
  (`734f8f99`)
- **config**: default `max_line_bytes` bumped from 500 to 3000. (`98adcf74`)
- **config**: permissions are now deny-by-default via the `default` key in
  `permissions.toml`. (`857ef2c9`)

### Fixed

- **providers**: model lookup now uses longest-prefix matching, and the Codex
  plan context window is respected. (`a0491185`)
- **ui**: discovered context window is now applied to app state on startup.
  (`031ad37f`)
- **highlight**: prefix lines are fed into the parser when reading files from an
  offset, fixing mis-highlighted continuation reads. (`43ed4598`)
- **ui**: platform-appropriate word-move labels are shown on all platforms.
  (`9d2108e2`)
- **ui**: `Ctrl+E` now jumps to end of line on all platforms instead of the old
  scroll-down binding. (`5671e6d4`)

### Refactored

- **providers**: deduplicated llama.cpp HTTP GET into `OpenAiCompatProvider`.
  (`bfe264fd`)

## [0.6.4] - 2026-06-23

### Added

- **providers**: configurable compaction model via the `Compaction` model tier,
  allowing a faster or larger-context model to be selected for compaction.
  Falls back to the current model when no match is found in the tier map.
  (`d96613b`, credit: g4bwy)
- **providers**: `ThinkingConfig` is now provided to llama.cpp via the
  `thinking_budget_tokens` field. (`01542d2`, credit: sdroege)
- **ui**: max context size is now displayed in the status bar. (`d269790`)

### Fixed

- **lua**: the `bash` tool now runs commands through `bash -c` instead of
  `sh -c`, which pointed to `dash` on Debian/Ubuntu and broke bash syntax
  (arrays, `[[`, process substitution) generated by LLMs. (`de84c1b`,
  credit: tontinton)
- **acp, plugins**: corrected wrong `kind` fields on the `edit`, `multiedit`,
  and `write` plugins and dropped the hardcoded name-to-kind fallback in
  `tool_kind()`. (`720abed`, credit: tontinton)
- **skill**: guard against a nil `cwd` in `find_project_ancestors`.
  (`8d00ae6`)
- **providers**: tolerate a missing space after the colon in SSE parsing.
  (`02a5722`)

### Changed

- **ui**: replaced the fixed `ZoneRegistry` array with a stack-allocated
  push-list. Push order now determines navigation order, splits/panels/
  permission prompts/queue render into areas with no representation, and
  `SelectionZone::StatusBar` is merged into `Overlay`. Mouse-down captures
  `highlight_area` so `apply_selection` no longer relies on live zone state.
  (`006d816`, credit: tontinton)

## [0.6.3] - 2026-06-21

### Added

- **providers**: renamed `tier_map` to `model_registry` across all call sites;
  `known_models` now holds `ModelInfo` enabling discovered `context_window` and
  `max_output_tokens` lookups, `Model::from_base` consults discovered metadata
  for unknown models, `write_overrides` emits the human-readable
  `{"spec": "tier"}` disk format, and the active model is re-resolved after
  discovery completes. (`d3338eb`)
- **providers**: one model per tier is now enforced via a structural invariant.
  (`e8e4ca`)
- **agent**: `written_path` field on `ToolExecResult` and `ToolDoneEvent`,
  preferred over the legacy `ToolOutput::WriteCode`/`Diff` path, reported by
  `edit`/`multiedit`/`write` and threaded through native, MCP, dedup-cache, and
  validation constructors. The Lua tool API gains a `mutable_path` spec field
  for plan-mode enforcement. (`6f73a4`)
- **agent**: the full `craft resume` command is printed on exit when a session
  can be resumed. (`84b683`)
- **ui, agent**: double-escape cancels an individual subagent when viewing its
  chat. (`62506f`, credit: tontinton)
- **ui**: pressing `1`/`2`/`3` toggles the tier override on a model that
  already has that exact tier. (`e6c8ce`)
- **ui**: the `/models` popup shows all assigned tiers per model. (`0f706f`)
- **config**: `always_thinking` accepts a numeric token budget (e.g.
  `always_thinking = 8192`). (`ef54fd)

### Changed

- **agent**: hardened `file_tracker` - `get_mtime` returns `Option`,
  `record_read` warns and skips on missing mtime, `check_before_edit` drops
  deleted files and tolerates untracked paths, with new tests for stale reads,
  re-reads, deleted and nonexistent files. (`6f73a4`)

## [0.6.2] - 2026-06-18

### Fixed

- **agent**: corrected the tree-sitter Go query that panicked the `outline`
  tool, and hardened query construction across `outline`/`zoom`/`callgraph` so a
  malformed query degrades to a skipped language instead of crashing the TUI.
  Also fixed latent `nix`, `typescript`, `kotlin`, `elixir`, `gleam`, and `dart`
  queries. (`319d11a`, `f7997f8`)

## [0.6.1] - 2026-06-18

### Added

- **providers**: configurable custom and local provider infrastructure with an
  interactive provider login picker and credential storage. (`be6abec`)
- **lua**: `autocmd` and `keymap` APIs. (`be6abec`)
- **lua**: bottom panel window placement added to the window API. (`be6abec`)

### Changed

- **providers**: replaced the dedicated `zai`, `ollama`, and `llama_cpp`
  providers with a single generic local provider. (`be6abec`)
- **ui**: migrated the todo panel from a Rust component to a Lua plugin.
  (`be6abec`)

### Removed

- built-in `todo_write` tool, now provided by a Lua plugin. (`be6abec`)

## [0.6.0] - 2026-06-17

### Added

- **index**: Dart language indexer with tree-sitter grammar. (`97e2445`)
- **agent**: nine new built-in tools:
  - `outline`: tree-sitter structural outline (24 languages).
  - `zoom`: symbol-aware file reader with AST lookup.
  - `fuzzy_replace`: occurrence parameter and Unicode normalization pass.
  - `ast_grep`: AST pattern search/replace (ast-grep-core, 4 languages).
  - `callgraph`: intra-file call graph (`call_tree`, `callers`, `impact`).
  - `delete`: file and directory deletion with auto-backup.
  - `move`: rename with import reference updates across the project.
  - `inspect`: TODO/FIXME/HACK scanner plus git status.
  - `conflicts`: git merge conflict marker parser. (`50d046e`)
- **agent**: post-edit tree-sitter validation with automatic rollback. (`50d046e`)
- **agent**: checkpoint/restore/list/undo/history commands and per-file
  auto-backup. (`50d046e`)
- **agent**: `background=true` parameter for `bash`, plus `bash_status`,
  `bash_watch`, and `bash_kill` for background task management. (`50d046e`)
- **agent**: `bash` output compression (ANSI stripping, blank line collapse).
  (`50d046e`)
- **agent**: tree-sitter grammars for CSS, Fish, GDScript, GDShader,
  Godot Resource, Objective-C, Perl, Svelte, and Zsh. (`a5bf08e`)

### Changed

- **sandbox**: expanded default writable roots to include per-tool data homes
  (`CARGO_HOME`, `RUSTUP_HOME`, `GOPATH`, `GRADLE_USER_HOME`, `YARN_CACHE_FOLDER`,
  `.npm`, `.m2`), environment-only roots (`CARGO_TARGET_DIR`, `GOMODCACHE`),
  and platform cache homes. (`443aa09`)

### Fixed

- **agent**: corrected tree-sitter draft queries for CSS, Fish, GDScript,
  GDShader, Godot Resource, and Objective-C that referenced wrong node types.
  (`a5bf08e`)

## [0.5.2] - 2026-06-16

### Added

- **index**: HTML and Nix language indexers with tree-sitter grammars.
  (`260b203`)
- **lua**: `fs`/`text`/`treesitter` APIs now return `(value, err)` instead of
  throwing; dropped error-event emission. (`260b203`)
- **lua**: `text_input` wraps long lines at the view edge. (`260b203`)
- **highlight**: `theme_color()` helper, `craft.ui.theme_color` Lua binding,
  and `color` Lua plugin. (`260b203`)
- **agent**: `ToolExecResult` with optional annotation; `ToolDoneEvent` gains
  annotation field. (`260b203`)
- **agent**: extracted `grep_search()` and exposed `craft.fs.grep()` Lua
  binding. (`260b203`)
- **grep**: migrated from native Rust tool to Lua plugin using
  `craft.fs.grep`. (`260b203`)
- **ui**: restore spinner in the status bar with `RestoreComplete` sentinel;
  session restore is now non-blocking via async channel.
  (`260b203`)

### Changed

- **storage**: session meta read from file tail instead of full scan.
  (`260b203`)
- **providers**: `max_tokens` omitted for llama.cpp when output budget is 0.
  (`260b203`)
- Removed `--demo` flag and `mock.rs`; `--all-features` kept for onnx.
  (`260b203`)

### Fixed

- **sandbox**: network is now available by default. (`ea2410e`)
- **sandbox**: reordered bwrap mounts so read-only root (`--ro-bind / /`)
  precedes writable binds (`--bind`), preventing EROFS on workspace
  directories like `.cargo-lock`. (`7f2ff21`)

## [0.5.1] - 2026-06-15

### Added

- **acp**: subagent task output is now folded into the parent task tool call
  result, keeping the transcript compact. (`fbcd4bd`)

### Fixed

- **skill**: the `/distill` command now writes skills to the project's
  `.craft/skills` directory instead of the memory store, and `.craft/skills`
  is now scanned by the skill discovery tool. (`10c0f7e`)
- **providers**: resolve duplicate model defaults on the `zai` and `synthetic`
  providers, and fix a stale long-context test case. (`591c131`)
- **agent**: use the synchronous `start_kill` in the Windows `ChildGuard` drop
  path so the crate compiles on Windows. (`a881042`)
- **agent**: ungate `PrefixCacheTracker::frozen_count` so the default-feature
  test build compiles. (`98e1668`)
- **lua**: disable the sandbox in `bash_timeout_round_trip` so the test passes
  on CI environments without a sandbox backing binary. (`a1ea786`)

## [0.5.0] - 2026-06-14

### Changed
- **versioning**: dropping maki version base from main version. 
  Maki base versions will be mentioned in release notes.
- **docs**: migrated the documentation site from Zola to mdBook. The doc
  generator (`craft-docgen`) now emits mdBook markdown into `site/docs/src`
  without Zola frontmatter, and `site/build.sh` builds with mdBook instead of
  Zola.
- **repo**: moved the canonical repository from GitLab to GitHub
  (`https://github.com/craft-build/craft`). All install commands, release
  URLs, the docs git link, the landing page, the update/version-check
  endpoints, and the changelog reference links now point at GitHub.

### Added

- **docs**: new documentation pages covering previously undocumented
  features: Usage (modes, shell bangs, image paste, palette), Skills
  (`SKILL.md` format and discovery dirs), Plugins (Lua API and built-ins),
  Sessions (storage, resume, checkpoints), Themes (25 bundled themes), and
  CLI (full subcommand and flag reference).

### Fixed

- **sandbox**: allow `/dev/null` and apply the sandbox profile before stdio is
  configured, so the profile is in effect for the entire session. (`52c76c6`)

## [0.3.17+0.4.2] - 2026-06-14

Tagged `v0.3.17+0.4.2`. Splash screen and version refresh for the tagged
release. (`a0e0607`)

## [0.3.17+0.4.1] - 2026-06-13

### Added

- **acp**: stable Agent Client Protocol v1 spec support with client delegation.
  Advertises session capabilities; implements `session/list`, `session/close`,
  and `session/resume`. Adds `StopReason::Cancelled`, session-update title
  generation, an id-keyed pending-request registry, and fs + terminal
  delegation to the client. (`266154e`)
- **sandbox**: new `craft-sandbox` crate with macOS (`sandbox-exec`/SBPL) and
  Linux (`bwrap`) backends, `WorkspaceWrite` and `ReadOnly` modes, and network
  gating. SBPL literals are escaped against injection and `apply()` is
  fail-closed when the backing binary is missing. Also adds a lifecycle hook
  bridge (`session_start`, `pre/post_tool_use`), dynamic tool promotion via
  `list_tools`, and a desktop entry point. (`8be50ca`)

### Changed

- README and banner artwork updated. (`8184736`)

## [0.3.17+0.3.6] - 2026-06-12

### Added

- **providers**: detect native 1M-token context for newer Claude models.
  (`1499778`)
- **agent**: long-horizon planning features ported from Mimo-Code, without new
  dependencies or SQLite:
  - keyword (TF-IDF) + semantic memory recall with budgeted injection;
  - hierarchical tasks (`T1`, `T1.1`) replacing flat todos (`LOG_FORMAT_VERSION`
    bumped to 3, backward-compatible load, tree rendering in the UI);
  - goal/judge stop condition: a second LLM call verifies the goal before the
    agent may stop (capped at 5 continuations, fails open);
  - `/dream` and `/distill` commands for memory consolidation and skill
    discovery;
  - `/checkpoint` writes a reviewable markdown checkpoint injected into the
    system prompt on resume;
  - subagent context modes (`none`/`summary`/`full`);
  - a curated 6-skill bundle (tdd, review, debug, verify, plan, execute).
  (`c055df8`)

## [0.3.17+0.3.5] - 2026-06-12

### Added

- **agent**: port of maki v0.3.17: ACP server over stdio, SDK/stream mode
  (Conductor / claude-agent-sdk compatibility), live shared `History` via an
  `ArcSwap` mirror, malformed-JSON tool repair through `jsonrepair` + schema
  aliases, and `tool_kind` support on the `Tool` trait. (`79ae225`)
- **acp**: model picker populated from the available providers. (`31171c1`)
- **acp**: config-option-based mode switching (`mode` + `model` as separate
  select options). (`0b1542b`)
- **acp**: MCP server passthrough from ACP clients (e.g. Zed), merged with the
  local `mcp.toml` and started per session. (`8008b72`)

### Changed

- **Breaking**: `Agent<'h>` now borrows `&mut History` instead of owning it.
  (`79ae225`)
- **Breaking**: `craft-acp` rewritten on `agent-client-protocol-schema` 0.13
  (was 0.14). (`79ae225`)
- **Breaking**: the question plugin's `multiple` field renamed to `multiSelect`
  (alias retained). (`79ae225`)

### Fixed

- `flash_duration_ms` now set in `merge_tools_overlay`. (`f24a257`)
- Three pre-existing Lua test failures. (`14d2682`)
- Syntax theme loaded in diff context line tests. (`7e264ce`)
- Eight failing tests across five files after the v0.3.17 port. (`7403968`)

## [0.3.16+0.3.5] - 2026-06-12

### Added

- **agent**: session-scoped **DoomTracker** replacing the per-run `max_turns`
  budget. Scores pathological behavior (doom loops, stagnation, ineffective
  compaction, tool errors, validator rejections); decays on success. Injects a
  one-shot grace prompt at score 15 and hard-stops the run at 25. Long-lived
  sessions (UI, ACP) share one tracker across runs. (`6337add`)

### Removed

- `max_turns` / `DEFAULT_MAX_TURNS` / `MIN_MAX_TURNS` from `craft-config`.
  (`6337add`)

## [0.3.16+0.3.4] - 2026-06-12

### Changed

- **agent**: wired up previously-dead cache-aware compression and trust-based
  tool dropping. (`e6948f0`)

### Fixed

- Time collision bug. (`7df8c63`)

## [0.3.16+0.3.3] - 2026-06-11

### Fixed

- Overflow recovery now uses real token usage, adds per-tool guardrails, and
  reduces magic numbers. (`b3a1f59`)
- `read_lifecycle` no longer destroys the active working context. (`4c1c0c1`)

## [0.3.16+0.3.2] - 2026-06-11

### Fixed

- `no_compress` preserved through batch processing; recent reads guarded from
  compression. (`6a91bd0`)

## [0.3.16+0.3.1] - 2026-06-11

### Added

- **agent**: `apply_patch` tool for Codex-style multi-file patches (`*** Begin
  Patch` / `*** End Patch` format) with fuzzy context matching, plan-mode
  protection, `file_tracker` staleness guards on deletes, overlap validation,
  and trailing-newline preservation. (`ac42e6f`)

### Changed

- tree-sitter dependencies updated. (`444e77c`)

## [0.3.16+0.3.0] - 2026-06-10

### Added

- **agent**: optional semantic intelligence via local ONNX embeddings
  (`onnx` feature, fastembed BGE-Base model). Adds a `RelevanceScorer`, semantic
  overlap detection, context curation within the token budget, auto-retrieve of
  compressed content, stagnation detection, and semantic stale overrides for
  reads. The keyword classifier was extracted into `keywords.rs` using
  aho-corasick. (`85d8f71`)
- **agent**: tool outputs compressed at insertion time (content-type detection
  applied immediately, originals preserved for the UI). (`48531ab`)

### Fixed

- `ToolDone` events forwarded from the review subagent to the UI. (`d8e87b9`)
- ONNX models eagerly downloaded before UI startup to avoid blocking. (`ef870fc`)
- Proactive compaction threshold lowered from 80% to 60%. (`099a4e8`)
- fastembed models stored in the XDG directory and download progress suppressed.
  (`01eff6f`)

## [0.3.15+0.2.3] - 2026-06-06

### Added

- Claude Fable 5 model. (`85b3720`)
- **agent**: review findings persisted in a session-scoped store. (`ef1d53b`)

### Changed

- Port of maki v0.3.15: panicked tools are recovered instead of dropped, writes
  are allowed when no prior read is recorded, `--model` from the CLI is no
  longer persisted, and the `code_execution` separator between script and
  output is restored. (`97e17fd`)

## [0.3.14+0.2.3] - 2026-06-04

### Changed

- Blocking I/O replaced with async equivalents across the workspace. (`dcb9605`)

## [0.3.14+0.2.2] - 2026-06-04

### Fixed

- Agent event build error. (`3255c96`)

## [0.3.14+0.2.1] - 2026-06-04

### Changed

- Port of maki v0.3.14 changes. (`86a9076`)

## [0.3.13+0.2.1] - 2026-06-03

### Added

Six features from the smallcode evaluation plan:

- **tool dedup cache**: caches read-only tool results (read/grep/glob/index),
  bounded to 64 entries with FIFO eviction, cleared on compaction.
- **trust decay**: tracks per-tool consecutive failures and demotes/drops tools
  after configurable thresholds (`warn_after=3`, `drop_after=5`).
- **snapshot & rollback**: auto-snapshots files before writes, commits on agent
  Done, rolls back via `/undo`.
- **post-write validation**: detects project type and runs validation commands
  after writes (disabled by default).
- **small model mode**: auto-detects models with context < 32k, reduces tools,
  uses a compact system prompt, compacts at 50%, and applies aggressive JSON
  repair.
- **model escalation**: tracks per-model failure rates and emits a
  `ModelEscalation` event for automatic tier upgrade.

(`7f0781e`)

### Fixed

- Read supersession uses range overlap instead of a same-file check. (`b716faf`)

## [0.3.13+0.2.0] - 2026-06-02

### Added

- **agent**: multi-stage context compression pipeline (Headroom-inspired):
  read lifecycle supersession, tool-output pre-compression, progressive
  compaction, client-side token estimation for proactive compression at 80% of
  the window, prefix-cache awareness, and reversible compression with a
  `retrieve` tool for on-demand decompression. (`066335a`)

## [0.3.13+0.1.3] - 2026-06-02

Maintenance: version bump only, no functional change. (`d9902ff`)

## [0.3.13+0.1.2] - 2026-06-02

### Added

- Port of maki v0.3.13: model picker with bare `1`/`2`/`3` tier keys, XDG
  directories in generated docs, OpenRouter on the site, and a Deno-style
  permission sandbox for user Lua plugins (`FsRead`/`FsWrite`/`Net`/`Run`/`Env`
  from `plugin.toml`). (`f28a74f`)

### Fixed

- Nested `CallbackError` traces stripped from Lua tool error messages via
  `strip_traceback()`. (`f28a74f`)

## [0.3.12+0.1.2] - 2026-06-02

### Added

- **providers**: OpenRouter provider (OpenAI-compatible, with reasoning-effort
  support). (`113e7a9`)
- `craft migrate xdg` command to move `~/.craft` into XDG directories.
  (`113e7a9`)
- Lua APIs `env.config_dir` and `fn.executable`. (`113e7a9`)

### Changed

- Tool snapshots re-baked on theme change instead of keeping stale renders.
  (`113e7a9`)

## [0.3.11+0.1.2] - 2026-06-01

### Added

- UI: multi-directional split layouts (above/below/left/right). (`7a4ea31`)
- Anthropic long context (`-1m` suffix) with the `context-1m` beta header.
  (`7a4ea31`)
- `FastPricing` for accurate fast-mode cost calculation. (`7a4ea31`)
- `always_fast` and `always_thinking` config options. (`7a4ea31`)

### Fixed

- Permission scope matching for space-star patterns and generalized scopes.
  (`7a4ea31`)

## [0.3.9+0.1.2] - 2026-05-29

### Changed

- **ui**: consolidated the shared-queue mutex lock helper into `pub(crate)` and
  removed `expect()` from production paths. (`5a21a57`)

## [0.3.9+0.1.1] - 2026-05-29

### Fixed

- **providers**: eliminated panics and magic strings. Google SSE `stop_reason`
  now uses first-wins semantics; a `lock_unpoison()` helper recovers from
  poisoned mutexes (38 call sites); `http_client()` returns `Result` instead of
  panicking on TLS failure. (`a319b23`)
- **storage**: GitLab API no longer receives a GitHub `Accept` header; errors
  from `persist_model`/`persist_theme_name` are propagated; theme writes use
  atomic writes for crash safety. (`9b6034c`)

## [0.3.9+0.1.0] - 2026-05-29

First craft version. Fork from maki v0.3.8; the `maki-*` crates are renamed to
`craft-*` across the workspace.

### Changed

- **interpreter**: replaced silent `unwrap_or(0.0)` defaults with `expect()`,
  added doc comments to public types, and reused `limits_with_timeout` in the
  `limits()` builder. (`fbf8ad1`)
- **markdown**: extracted `try_extract_table()` from `split_normal_blocks()`,
  unified `wrap_spans()` and `split_line_with_bar()` into a shared helper, and
  removed dead table over-consumption logic. (`5a5c65f`)

### Fixed

- **lua**: SSRF DNS-rebinding TOCTOU fixed via `resolve_to_addrs`; sub-second
  timeouts via `from_secs_f64`; `setsid` return value checked; all global
  plugin directories now visited on load; plugin name derived from the file stem
  instead of a hardcoded `"user"`. (`3ceb90c`)

[Unreleased]: https://github.com/craft-build/craft/compare/v0.9.4...HEAD
[0.9.4]: https://github.com/craft-build/craft/compare/v0.9.3...v0.9.4
[0.9.3]: https://github.com/craft-build/craft/compare/v0.9.2...v0.9.3
[0.9.2]: https://github.com/craft-build/craft/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/craft-build/craft/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/craft-build/craft/compare/v0.8.7...v0.9.0
[0.8.7]: https://github.com/craft-build/craft/compare/v0.8.6...v0.8.7
[0.8.6]: https://github.com/craft-build/craft/compare/v0.8.5...v0.8.6
[0.8.5]: https://github.com/craft-build/craft/compare/v0.8.4...v0.8.5
[0.8.4]: https://github.com/craft-build/craft/compare/v0.8.3...v0.8.4
[0.8.3]: https://github.com/craft-build/craft/compare/v0.8.2...v0.8.3
[0.8.2]: https://github.com/craft-build/craft/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/craft-build/craft/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/craft-build/craft/compare/v0.7.6...v0.8.0
[0.7.6]: https://github.com/craft-build/craft/compare/v0.7.5...v0.7.6
[0.7.5]: https://github.com/craft-build/craft/compare/v0.7.4...v0.7.5
[0.7.4]: https://github.com/craft-build/craft/compare/v0.7.3...v0.7.4
[0.7.3]: https://github.com/craft-build/craft/compare/v0.7.2...v0.7.3
[0.7.2]: https://github.com/craft-build/craft/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/craft-build/craft/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/craft-build/craft/compare/v0.6.5...v0.7.0
[0.6.5]: https://github.com/craft-build/craft/compare/v0.6.4...v0.6.5
[0.6.4]: https://github.com/craft-build/craft/compare/v0.6.3...v0.6.4
[0.6.3]: https://github.com/craft-build/craft/compare/v0.6.2...v0.6.3
[0.6.2]: https://github.com/craft-build/craft/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/craft-build/craft/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/craft-build/craft/compare/v0.5.2...v0.6.0
[0.5.2]: https://github.com/craft-build/craft/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/craft-build/craft/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/craft-build/craft/releases/tag/v0.5.0
[0.3.17+0.4.2]: https://github.com/craft-build/craft/releases/tag/v0.3.17+0.4.2
[0.3.17+0.4.1]: https://github.com/craft-build/craft/compare/v0.3.17+0.3.6...v0.3.17+0.4.1
[0.3.17+0.3.6]: https://github.com/craft-build/craft/compare/v0.3.17+0.3.5...v0.3.17+0.3.6
[0.3.17+0.3.5]: https://github.com/craft-build/craft/compare/v0.3.16+0.3.5...v0.3.17+0.3.5
[0.3.16+0.3.5]: https://github.com/craft-build/craft/compare/v0.3.16+0.3.4...v0.3.16+0.3.5
[0.3.16+0.3.4]: https://github.com/craft-build/craft/compare/v0.3.16+0.3.3...v0.3.16+0.3.4
[0.3.16+0.3.3]: https://github.com/craft-build/craft/compare/v0.3.16+0.3.2...v0.3.16+0.3.3
[0.3.16+0.3.2]: https://github.com/craft-build/craft/compare/v0.3.16+0.3.1...v0.3.16+0.3.2
[0.3.16+0.3.1]: https://github.com/craft-build/craft/compare/v0.3.16+0.3.0...v0.3.16+0.3.1
[0.3.16+0.3.0]: https://github.com/craft-build/craft/compare/v0.3.15+0.2.3...v0.3.16+0.3.0
[0.3.15+0.2.3]: https://github.com/craft-build/craft/compare/v0.3.14+0.2.3...v0.3.15+0.2.3
[0.3.14+0.2.3]: https://github.com/craft-build/craft/compare/v0.3.14+0.2.2...v0.3.14+0.2.3
[0.3.14+0.2.2]: https://github.com/craft-build/craft/compare/v0.3.14+0.2.1...v0.3.14+0.2.2
[0.3.14+0.2.1]: https://github.com/craft-build/craft/compare/v0.3.13+0.2.1...v0.3.14+0.2.1
[0.3.13+0.2.1]: https://github.com/craft-build/craft/compare/v0.3.13+0.2.0...v0.3.13+0.2.1
[0.3.13+0.2.0]: https://github.com/craft-build/craft/compare/v0.3.13+0.1.3...v0.3.13+0.2.0
[0.3.13+0.1.3]: https://github.com/craft-build/craft/compare/v0.3.13+0.1.2...v0.3.13+0.1.3
[0.3.13+0.1.2]: https://github.com/craft-build/craft/compare/v0.3.12+0.1.2...v0.3.13+0.1.2
[0.3.12+0.1.2]: https://github.com/craft-build/craft/compare/v0.3.11+0.1.2...v0.3.12+0.1.2
[0.3.11+0.1.2]: https://github.com/craft-build/craft/compare/v0.3.9+0.1.2...v0.3.11+0.1.2
[0.3.9+0.1.2]: https://github.com/craft-build/craft/compare/v0.3.9+0.1.1...v0.3.9+0.1.2
[0.3.9+0.1.1]: https://github.com/craft-build/craft/compare/v0.3.9+0.1.0...v0.3.9+0.1.1
[0.3.9+0.1.0]: https://github.com/craft-build/craft/compare/d2f23c83...v0.3.9+0.1.0
