# Changelog

All notable changes to **craft** are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/craft-build/craft/compare/v0.6.3...HEAD
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
