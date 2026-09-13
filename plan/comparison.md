# Craft reference vs. new agent — feature gap analysis

Reference: `~/Projects/craft` (the full harness, v0.14.1, ~16 crates).
New agent: `~/Projects/craft-code/agent` (crate `craft-acp`, ~13k LOC, Rig-based).

This document lists **what the reference harness has that the new agent does not**,
so each item can be triaged as keep/port/drop. It is one-directional by design:
features already present in both are only summarized in the baseline section below.

Every gap item is a checkbox with:

- **What/why** — what the feature does and the notable logic behind it.
- **Ref** — where it lives in the reference (crate + path).
- **Effort** — rough port size: `S` (hours), `M` (1–3 days), `L` (1–2 weeks), `XL` (subsystem).
- **Depends on** — other gaps that must come first (only when non-obvious).

Effort numbers assume porting into the new Rig-based architecture, not a line-by-line copy.

---

## Baseline: what the new agent already has

Recorded here so the gap sections don't re-state it.

| Area | New agent today |
| --- | --- |
| CLI | `craft` launches TUI; `craft acp` serves ACP over stdio. No other flags/subcommands. `agent/src/main.rs` |
| Config | Single `~/.config/craft/agent.toml`: `[providers.<alias>]` (kind, `api_key_env`, `base_url`, model catalogs, `discover_models`), `[agent]` (preamble, `max_turns`, temperature, max_tokens), `[[compaction]]` stages. Strict validation, no file creation. `agent/src/config.rs` |
| Providers | 26 Rig provider kinds + `openai-compatible`; model discovery merged with configured overrides; token-limit catalog metadata feeds compaction. `agent/src/providers.rs` |
| Agent loop | Thin layer over Rig's native agent/runner: Rig owns continuations, retries, tool execution; caller-owned history; streaming; cancel hook; `merge_history`. `agent/src/agent.rs` |
| Tools | `read`, `grep`, `edit`, `edit_lines`, `insert_lines`, `write`, `delete` — workspace-rooted, `..`/`.git`/symlink refusal, 8 MiB text cap, 64 KiB output clipping, serialized blocking I/O, atomic staged writes with final content check. `agent/src/tools/` |
| Tool approval | TUI `ApprovalHook` approve/reject overlay for edit-class tools (`edit`, `edit_lines`, `insert_lines`, `write`, `delete`). `agent/src/tui/provider/live.rs:296` |
| Compaction | Staged `vcc` (ported from reference VCC) + `llm` compaction with context fill-ratio thresholds, effectiveness gating (10% savings re-arm rule), summary merging, static fallback for failed LLM calls. `agent/src/compaction/` |
| TUI | Chat transcript, context sidebar, composer with paste, slash commands (`/clear /compact /undo /model /sessions /help`), command palette, model menu with efforts, collapsible tool cards, diff approve/reject, mouse text selection/copy with regions, git branch + usage label. `/compact`, `/undo`, `/sessions` are currently no-ops (`agent/src/tui/app.rs:410`). `agent/src/tui/` |
| ACP | stdio server: initialize, new/close session, session config options (provider → model), prompt with streaming updates (text chunks, tool call start/update, usage, stop reasons), cancel. In-memory sessions only. `agent/src/acp.rs` |
| Testing | Tool unit tests, TUI app tests with a mock provider. `agent/src/tools/tests.rs`, `agent/src/tui/provider/mock.rs` |

Notable architectural difference: the new agent delegates the run loop to **Rig**
(retries, continuations, tool execution), while the reference owns its own loop.
Several reference features below live inside that custom loop and will need to be
re-expressed as Rig hooks/runner wrappers or by taking loop ownership back.

---

## A. Tools the reference exposes that we lack

The reference registers tools from two places: native Rust tools in
`craft-agent/src/tools/` and Lua plugins in `plugins/*/init.lua`
(bash, glob, grep, question, skill, todo_write, view_image, webfetch, websearch,
sessions, task are Lua-side in the reference).

### A.1 Execution & environment

- [Y] **bash** — shell command execution with streaming output, background tasks (`background`, `bash_status`/`bash_watch`, `bash_kill`), output capture and timeout (300s interactive), permission scoping per parsed command. Ref: `plugins/bash/`, permission parsing in `craft-agent/src/tool_dispatch.rs`. Effort: **L** (execution + background task lifecycle + approval integration). Depends: permission system (B.1), ideally sandbox (B.2).
- [Y] **apply_patch** — Codex-style `*** Begin Patch` multi-file add/update/delete patch application with fuzzy context matching. Ref: `craft-agent/src/tools/apply_patch.rs` (30k). Effort: **L**.
- [Y] **batch** — atomic multi-file edit batch (edits applied together or not at all), shared write-path conflict detection. Ref: `craft-agent/src/tools/batch.rs`. Effort: **M**.

### A.2 Search & navigation

- [Y] **glob** — glob-pattern file finder respecting ignore rules. Ref: `plugins/glob/`. Effort: **S**.
- [Y] **list** — directory listing (names, dirs first, filters instruction files). Ref: `craft-agent/src/tools/list.rs`. Effort: **S**.

### A.3 Editing extras (beyond our exact-match tools)

- [Y] **multiedit** — multiple find/replace edits to one file applied atomically in sequence. Ref: `craft-agent/src/tools/multiedit.rs`. Effort: **S**.
- [Y] **fuzzy_replace** — fuzzy-matched replacement (trailing whitespace, indentation, Unicode tolerance) used to make edits robust to model drift; fuzzy pass reporting. Ref: `craft-agent/src/tools/fuzzy_replace.rs` (36k), `edit_helpers.rs`. Effort: **M** (port) — improves edit success rate materially.
- [Y] **move_file** — move/rename with project-wide import/reference updates (Rust `use`, TS `import`, etc.). Ref: `craft-agent/src/tools/move_file.rs`. Effort: **M**.
- [Y] **inspect** — project health check: TODO/FIXME/HACK/XXX scan + git status (porcelain). Ref: `craft-agent/src/tools/inspect.rs`. Effort: **S**.
- [Y] **recursive/anchored delete** — our `delete` is single-file, non-recursive by design; the reference delete has its own guard set. Compare before porting. Ref: `craft-agent/src/tools/delete.rs`. Effort: **S** (delta only).

### A.4 Web & media

- [Y] **webfetch** — URL fetch (markdown/text/html), 5MB/120s caps. Ref: `plugins/webfetch/`. Effort: **M**. Depends: SSRF protection (E.8) strongly recommended.
- [Y] **websearch** — Exa-backed web search. Ref: `plugins/websearch/`. Effort: **S–M** (API key plumbing).
- [Y] **view_image** — inline image viewing (kitty graphics protocol with unicode-halfblock fallback). Ref: `plugins/view_image/`, `craft-ui/src/image.rs`. Effort: **M**. Depends: image pipeline in TUI (F.13).

### A.5 Agent-support tools

- [ ] **task (subagents)** — spawn `research`/`general` subagents with own context window; model tiers/roles (weak/medium/strong); optional git-worktree isolation; `output_schema` enforced JSON results with jsonrepair retry (3x); findings store; cost chained to parent run. Ref: `craft-agent/src/tools/subagent.rs`, `task.rs`, `worktree.rs`, `findings_store.rs`. Effort: **XL** (or **L** for single-tier, no isolation). Depends: model roles (H.4), agent spawn API (C.14).
- [Y] **todo_write** — hierarchical task tracker with parent/child tasks, owners, statuses. Ref: `plugins/todo_write/`. Effort: **S**.
- [Y] **question** — structured ask-the-user tool (options, multi-select, custom answers) over events + response channel; headless variant. Ref: `craft-agent/src/tools/question.rs`, `plugins/question/`. Effort: **M** (needs UI + ACP elicitation surfaces).
- [Y] **skill** — surface skill bodies (`skill://<name>`) from discovered SKILL.md bundles. Ref: `plugins/skill/`, `craft-agent/src/builtin_skills.rs`. Effort: **S–M**. Depends: skill discovery (J.4).
- [Y] **retrieve** — fetch back the original text of a compressed tool output by content hash (`retrieve#<hash>` markers). Ref: `craft-agent/src/retrieve.rs`, `compression_store.rs`. Effort: **S**. Depends: reversible compression (D.5).
- [Y] **list_tools** — introspection tool listing available tools for the model. Ref: `craft-agent/src/tools/list_tools.rs`. Effort: **S**.
- [Y] **sessions (model-side)** — list/switch sessions from the model. Ref: `plugins/sessions/`. Effort: **M**. Depends: session persistence (I.2).

## B. Tool infrastructure & permissions

- [Y] **B.1 Permission rule engine** — persistent `permissions.toml` with `[tool]` allow/deny rules, wildcard scopes (`git *`), global vs project targets, MCP server/tool keys; user "always allow" answers written back with `toml_edit` (comment-preserving); session rules (`AllowOnce`/`Always`/`Deny…` with encode/decode); builtin deny rules (project boundary escapes, `SHELL_KEYWORDS`); approval gate with 30-min ask timeout; plugin-contributed rules. Ref: `craft-agent/src/permissions.rs`, `craft-config/src/lib.rs` (permissions.toml). Effort: **L**. This is the backbone for bash/web/task tools.
- [Y] **B.2 Compound-command permission parsing** — tree-sitter bash parsing splits `git diff && rm -rf /` into separate scopes (`git *`, `rm *`) so approval is per-command, not per-string. Ref: bash parsing in `craft-agent/src/`, tree-sitter-bash dep. Effort: **M**. Depends: B.1.
- [Y] **B.3 Bash sandbox** — OS-level sandbox for shell commands: macOS Seatbelt (`craft-sandbox/src/mac.rs`) and Linux (landlock/seccomp, `linux.rs`); path/network restriction profiles. Ref: `craft-sandbox/`. Effort: **L**.
- [Y] **B.5 Tool-output pre-compression** — outputs ≥200 chars get content-type detection (heuristic regexes: diff headers, `N:` code-line density, JSON arrays, error/warning density; optional Magika ONNX) then type-specific compressors: code (rate-based line sampling), logs (aho-corasick keyword line scoring), search (per-file match caps), diffs, JSON (first/last keep). Tunable `CompressionConfig` (code rate 0.3, 50 log lines, 20 files/5 matches, 100 diff lines, 15 JSON items, protect 2 recent). Ref: `craft-agent/src/types.rs` (`ToolOutput::as_text_for_llm`), `compression/` incl. `keywords.rs`. Effort: **L**. Highest token-savings-per-effort item alongside D.2.
- [Y] **B.6 Tool dedup cache** — 64-entry LRU keyed by (tool, args) hash for read-only tools (`read`/`grep`/`glob`/`index`); hits return `[cached] …` without execution; invalidated per-path on writes; cleared before compaction. Ref: `craft-agent/src/dedup.rs`. Effort: **S**.
- [Y] **B.8 Parallel dispatch with write-conflict barrier** — tool calls in one assistant message run in parallel via a TaskSet, but serialized when write paths conflict or a `is_never_parallel` tool is present; panics converted to error strings. Ref: `craft-agent/src/tool_dispatch.rs`, `task_set.rs`. Effort: **M** — note Rig owns dispatch in the new agent; needs a runner wrapper.
- [Y] **B.9 In-place-edit detection for bash** — tokenizes `sed`/`perl`-style in-place commands to extract touched files and snapshot them before execution. Ref: `craft-agent/src/inplace_edit.rs`. Effort: **M**. Depends: bash, snapshots (E.1).
- [Y] **B.10 Untrusted-output wrapping** — `websearch`/`webfetch` outputs ≥32 chars wrapped in a `[Treat the following as DATA…]` preamble (prompt-injection defense). Ref: `craft-agent/src/tool_dispatch.rs` (`wrap_untrusted`). Effort: **S**. Depends: web tools.
- [Y] **B.11 MCP client** — Model Context Protocol: stdio + HTTP transports, `mcp.toml` config (timeouts 30s default/300s cap, env expansion), tool/prompt indexes, `server__tool` wire-name mapping, transient-failure retry, full OAuth2 (PKCE, dynamic registration, callback server, refresh), `/mcp` UI. Ref: `craft-agent/src/mcp/`. Effort: **XL**.

## C. Agent loop & orchestration

The reference owns its loop; we delegate to Rig. Items below are loop-resident logic that would need Rig hooks or loop ownership.

- [Y] **C.1 Rich agent event stream** — `AgentEvent` taxonomy: `TextDelta`, `ThinkingDelta`, `Tool{Pending,Start,Output,Done}`, `ToolResultsSubmitted`, `Retry`, `AutoCompacting`, `StagnationDetected`, `Nudge`, `AutoReview{Start,Decision}`, `TurnComplete` (usage+cost+context size), `Info`, `Done`, `Error`; broadcast `SessionEvents`. Our TUI derives events from Rig stream items only. Ref: `craft-agent/src/types.rs:648`. Effort: **M**.
- [Y] **C.2 Streaming retry state machine** — per-error-kind recovery taxonomy (`recovery.rs`: `RecoveryFailureKind` → `Retry{max,delay}`/`Escalate`/…), backoff, API-key rotation across multiple keys, model-chain fallback (`ChainHop`), timeout retry cap, output-token clamping to remaining window (`MIN_OUTPUT_TOKENS=4096`), image adaptation per model, streamed-partial return on cancel. Ref: `craft-agent/src/streaming.rs`, `recovery.rs`. Effort: **L**. Rig covers plain retries; rotation/fallback/clamping are extras.
- [Y] **C.3 MaxTokens continuation** — truncated responses re-prompted to continue up to `max_continuation_turns`. Ref: `craft-agent/src/run/turn.rs:274`. Effort: **S–M** (Rig may cover; verify).
- [Y] **C.4 Stalled-turn nudging** — empty assistant reply → push empty marker + nudge prompt (max 20, only if a tool result is in the recent-5 window). Ref: `run/turn.rs:317`. Effort: **S**.
- [Y] **C.5 Context-overflow recovery** — on overflow: calibrate token-estimate multiplier (×1.1, clamp 5.0), auto-compact, retry turn (max 1 recovery). Ref: `run/compaction.rs`, `run/mod.rs`. Effort: **M**. Depends: D.x compaction extras.
- [Y] **C.6 Cancellation architecture** — `CancelToken` (event-listener, child tokens), `CancelTrigger` (cancel on drop), `CancelMap` (per-session/per-tool slotted cancellation, pre-cancel), history sanitization on cancel (partial text kept + cut-off marker, dangling tool calls closed, rollback truncation). Ref: `craft-agent/src/cancel.rs`, `history.rs`. Effort: **M**.
- [Y] **C.8 Doom-loop tracker** — additive score (doom loop +15, stagnation +3, ineffective compaction +2, tool error +1…; successes −1), one-shot "summarize and stop" grace prompt at 15, hard stop at 25 (commits snapshots, ends run). Ref: `craft-agent/src/doom.rs`. Effort: **M**.
- [Y] **C.9 Per-tool guardrails** — exact-repeat warn/block (2/4), same-tool-failure warn/block (3/6), no-progress (read-only) warn/block (2/4); resets after compaction. Ref: `craft-agent/src/guardrails.rs`. Effort: **S–M**.
- [Y] **C.12 Advisor (post-turn review)** — small model reviews the last ~6 messages / 8k chars delta on Done, returns severity `blocker|concern|nit|ok`; threshold decides continue-vs-stop; emission guard dedupes repeats; continuation cap. Ref: `craft-agent/src/advisor.rs`. Effort: **M**. Depends: model roles.
- [Y] **C.14 Headless session API** — `spawn`/`spawn_interactive` handles with flume event receivers, `SessionStore` open/save/record_turn — the substrate ACP/print/SDK modes share. Ref: `craft-agent/src/headless.rs`. Effort: **M**.
- [Y] **C.15 Instructions discovery (AGENTS.md)** — AGENTS.md / AGENTS.local.md / global, ancestor-dir walk, subdirectory-scoped injection, dedupe; `{{instructions}}` slot in the system prompt. Ref: `craft-agent/src/instructions.rs`, `prompt.rs`. Effort: **M**.
- [Y] **C.16 Prompt slot system** — templated system/plan/research/general/compaction prompts with plugin-overridable slots. Ref: `craft-agent/src/prompt.rs`. Effort: **S** (native) — with plugins, depends J.1.
- [Y] **C.17 Modes: Build / Plan / Flow** — plan mode blocks all writes except the plan file and gates on goal approval (`AwaitingGoalApproval`); Flow is a typed multi-stage pipeline (scout/tpm/plan/req/execute/review/qa/report/integrator/verifier) with per-stage prompts, thread manager, typed append-only workstream log + projections, goal-approval gate, `[agent.flow]` config (max review/QA iterations, parallel chunks). Ref: `craft-agent/src/{plan wiring in tool_dispatch.rs, turn_type.rs, transitions.rs, threads.rs, flow_loop.rs, typed_log.rs, flow_index.rs}`. Effort: **XL** (Flow) / **M** (Plan only).
- [Y] **C.20 Cost ledger** — per-run usage/cost (list vs billed price), parent-chained for subagents, accumulated per model; feeds `/usage`, `/stats`. Ref: `craft-agent/src/types.rs` (`RunLedger`), `craft-providers/src/pricing.rs`. Effort: **M**. Depends: pricing data (H.6).

## D. Context management & compaction extras

We have staged VCC + LLM compaction with effectiveness gating. The reference layers
considerably more around it.

- [Y] **D.1 Token estimation & calibration** — `chars/4` + 1500 tokens/image; prompt estimate includes system + tool-schema bytes; on overflow the estimate multiplier is recalibrated (actual/estimated × 1.1, clamped at 5.0). Our `estimate.rs` has no calibration loop. Ref: `craft-agent/src/run/compaction.rs:58-110`. Effort: **S**.
- [Y] **D.2 Read lifecycle** — before each turn, old reads classified **Stale** (file later edited outside the working set of last 4 assistant messages) or **Superseded** (subsumed by a later read; never in the most recent turn to avoid re-read loops) and replaced with `[Stale read: …]`/`[Superseded read: …]` markers. Ref: `craft-agent/src/read_lifecycle.rs`. Effort: **M**. Cheapest big win for context freshness.
- [Y] **D.5 Reversible compression store + retrieve** — originals of compressed outputs kept in a 100-entry LRU keyed by content hash; history carries `retrieve#<hash>` markers; the `retrieve` tool restores originals on demand; **auto-retrieve** re-injects originals when the marker's context becomes relevant to the current intent (cosine ≥ 0.6). Ref: `craft-agent/src/compression_store.rs`, `retrieve.rs`, `semantic.rs::auto_retrieve`. Effort: **M** (+S for auto-retrieve once embeddings exist).
- [Y] **D.7 Recency tail** — volatile facts (turn counters etc.) rendered as `<turn-context>` appended only to the last user message per request. Ref: `run/recency.rs`. Effort: **S**.
- [Y] **D.8 Targeted LLM compaction** — LLM compaction variant embedding the top-10 relevant topics into the summary prompt; overflow retry ladder (collapse results → drop oldest round → strip by ratio → static fallback). Ref: `craft-agent/src/compaction/llm.rs`. Effort: **M** (we have the base path).
- [Y] **D.9 Carry-protection & buffers** — unanswered user input protected from summarization (`carry_from`); compaction buffer subtracted from context window; proactive (early) compaction thresholds; VCC-falls-through-to-LLM ordering; ineffective-compaction accounting feeding doom. Ref: `run/compaction.rs`, `compaction/mod.rs`. Effort: **S–M** (we have gating; the deltas are carry + buffer + proactive trigger).

## E. Reliability & guardrails

- [Y] **E.1 Snapshots & rollback (`/undo`)** — pre-write capture of file content (≤5 MB, workdir-scoped) around `write`/`edit`/`multiedit`/bash-detected in-place edits; undo stack (`SnapshotStore`, `safety.rs` exposes safety/checkpoint tooling); commit on clean Done / doom hard-stop / cancel; `/undo` restores. Our `/undo` is a no-op and tools have no undo store. Ref: `craft-agent/src/snapshot.rs`, `tools/safety.rs`. Effort: **M**.
- [Y] **E.5 JSON repair pipeline** — `jsonrepair`-based extraction for schema-constrained subagent output and auto-review decisions. Ref: `tools/subagent.rs`, `auto_review.rs`. Effort: **S**.
- [Y] **E.7 LLM auto-review of permissions** — permission mode where a small model decides allow/deny with risk + rationale (30s deadline, fails closed), decisions recorded into session rules; `--auto-review`/`/auto-review`. Ref: `craft-agent/src/auto_review.rs`. Effort: **M**. Depends: B.1.
- [Y] **E.8 SSRF protection on webfetch** — internal-URL/private-range blocking with a 17k-line rule set (redirect chasing, DNS rebasing considerations). Ref: `craft-agent/src/tools/internal_urls.rs`. Effort: **M**. Depends: webfetch.
- [Y] **E.9 Process safety** — `ChildGuard` (kill + reap on drop, 5s wait, signal escalation) for spawned processes. Ref: `craft-agent/src/child_guard.rs`. Effort: **S**. Depends: bash.
- [M] **E.10 Auth-error reauth wait** — auth failures pause the loop for re-auth instead of failing the run. Ref: `run/turn.rs` (auth error handling). Effort: **S**. Depends: auth flows (H.5).

## F. TUI & terminal experience

Our TUI is a single chat view with basic overlays. Reference notes: the "tui/"
directory in the reference repo holds the Lua plugins; the actual terminal app is
`craft-ui` (`app/mod.rs` 2.5k lines, `event_loop.rs` 2.3k lines).

### F.1 Architecture

- [Y] **Dirty-flag repaint / cadence model** — every component returns `Dirty`; select()-loop over crossterm events + channels + tick timers instead of unconditional redraws; synchronized-output escape wrapping; off-UI-thread render worker. Ref: `craft-ui/src/{event_loop.rs, repaint.rs, render_worker.rs, terminal.rs}`. Effort: **M** — matters for 60 FPS + low CPU.
- [Y] **Data-driven keybindings** — `ActionId` + `KEYBINDS` table + 13 `KeybindContext`s, user-rebindable via config, platform-aware labels, auto-generated help modal. Ref: `craft-ui/src/components/keybindings.rs`, `help_modal.rs`. Effort: **M**.

### F.2 Modes & input

- [Y] **Mode cycling (Tab)** — Build / Plan / Flow with status label + per-mode theme color. Ref: `craft-ui/src/app/mode.rs`. Effort: **S** (+ C.17 for semantics).
- [Y] **Bash bang-mode** — input starting `!` runs a shell command with streamed output (300s timeout, [BASH] label); `!!` runs hidden from context. Ref: `craft-ui/src/app/shell.rs`. Effort: **M**. Depends: bash execution path.
- [Y] **Composer upgrades** — persistent input history (↑/↓, 100 entries), word motions (Ctrl-W, Alt-←/→), Ctrl-K kill line, Alt-O edit in `$EDITOR`, configurable max lines, line-wrap measurement. Ref: `craft-ui/src/components/{input.rs, text_buffer.rs}`, `craft-storage/src/input_history.rs`. Effort: **M**.
- [Y] **Ctrl-C tri-state** — cancel streaming / clear input / quit. Ref: `app/mod.rs:749`. Effort: **S**.

### F.3 Navigation & history

- [Y] **Subagent task chats** — every subagent gets its own chat window keyed by tool-use id; Ctrl-N / Ctrl-P navigate; Escape-Escape inside a task chat cancels that subagent; task status exposed to plugins. Ref: `craft-ui/src/app/{tasks.rs, mod.rs}`. Effort: **L**. Depends: task tool (A.5).
- [Y] **Fuzzy message search (Ctrl-F)** — nucleo matcher over transcript, jump-to-segment, next/prev. Ref: `components/search_modal.rs`. Effort: **M**.
- [Y] **File picker (Ctrl-S)** — async cached walkdir + nucleo path matcher, empty-dir walk-up. Ref: `components/file_picker.rs`. Effort: **M**.
- [Y] **Session resume / picker** — background checkpointing with soft delay, session picker, draft-input preservation across checkpoints, subagent transcript restore, resume-latest-by-cwd. Ref: `craft-ui/src/app/{session.rs, session_state.rs}`, `storage_writer.rs`. Effort: **L**. Depends: I.2.
- [Y] **Todo/plan panel (Ctrl-T) & plan editor handoff (Ctrl-O)** — toggleable plan/todo side panel; open the plan file in `$EDITOR`. Ref: keybinds + `plan_form.rs`. Effort: **M**. Depends: todo_write (A.5), plans (I.5).
- [Y] **Suspend (Ctrl-Z)**. Effort: **S**.

### F.4 Commands

- [Y] **Fuzzy command palette with dynamic sources** — nucleo fuzzy over builtin commands + custom commands (`.craft/commands`, `.claude/commands`) + MCP prompts + plugin commands; Tab completion; `!` bang suffix. Ref: `craft-ui/src/components/command.rs`. Effort: **M**. Depends: commands (J.5).
- [Y] **Full builtin slash-command set** — reference has ~35; we have 6 (two of which are no-ops). Notable additions:
  `/usage` (token usage + live provider quota), `/stats` (cost ledger), `/theme`, `/mcp`, `/login`, `/cd`, `/btw` (side-channel question against history, no tools, floating modal), `/yolo`, `/auto-review`, `/thinking [level]`, `/fast` (Anthropic fast mode), `/reload` (config),  `/recipe`, `/distill` (discover workflows → propose skills), `/checkpoint`. Ref: `command.rs:30-229` (`BUILTIN_COMMANDS`), `app/btw.rs`. Effort: **S each**, mostly dependent on their subsystems.

### F.5 Overlays & pickers

- [Y] **Permission prompt overlay** — tool context display, generalized "always allow" scope negotiation (session/project/global), Deny-with-editable-reason, paste support, answers routed to the owning subagent. Ref: `components/permission_prompt.rs`. Effort: **M**. Depends: B.1.
- [Y] **Model picker with tier assignment** — full model list plus per-role tiers (strong/medium/weak/compaction) via `1-4`/`!@#$`. Ref: `components/model_picker.rs`. Effort: **M**. Depends: H.4.
- [Y] **Usage & stats modals** — tokens + priced session billing + live quota fetch; per-model/per-session cost views. Ref: usage/stats modals. Effort: **M**. Depends: C.20.
- [Y] **Theme picker (30 themes, live switch)** — themes as TOML compiled in (ayu, catppuccin×4, dracula, everforest, gruvbox, kanagawa, nord, rose-pine, solarized, tokyonight, …), ~100 semantic style slots, global `Arc<Theme>` with generation counter so syntax highlighting re-styles too. Ref: `craft-ui/src/themes/*.toml`, `theme.rs`. Effort: **M**.
- [Y] **Other modals** — thinking picker (off/adaptive/effort/budget), login picker, MCP picker, recipe picker, plan form, flow goal form. Effort: **S each** behind their subsystems.

### F.6 Rendering

- [Y] **Markdown rendering engine** — block model (paragraphs, headings, lists with depth/markers, code, tables), width-aware re-wrap with **stateful resumption for streaming**, semantic `StyleToken`s, box-drawing + compact tables, shared truncation notices. Ref: `craft-markdown/src/{lib.rs, render.rs}` (1.4k). Effort: **L**.
- [Y] **Syntax highlighting** — syntect + two-face syntax set, path/lang lookup, theme-linked colors with generation counter, warmup + readiness, small thread pool. Ref: `craft-highlight/src/lib.rs`, `pool.rs`. Effort: **M**.
- [Y] **Diff rendering** — hunk computation + syntect-highlighted add/remove lines, collapsible diff tool cards. Ref: `craft-ui/src/code_view.rs:214`, `tool_display.rs`. Effort: **M**. (We render diff lines plainly today.)
- [Y] **Tool display specialization** — per-tool renderers (diff/file/grep/edit views) with truncation notices and expand toggles; findings/priority rendering. Ref: `craft-ui/src/components/tool_display.rs` (2.4k). Effort: **M**.
- [Y] **Scrollback engine** — segment cache with addressable rows, auto-scroll pinning, resize-stable restore, full keyboard scroll set (line/half/page/top/bottom). Ref: `components/messages/`, `messages/scroll.rs`. Effort: **M**.
- [Y] **OSC-8 hyperlinks** — clickable links injected into rendered cells post-hoc. Ref: `craft-ui/src/hyperlink.rs`. Effort: **S**.
- [Y] **Inline images** — kitty graphics protocol with unicode-halfblock fallback; image attach by path and clipboard image paste. Ref: `craft-ui/src/{image.rs, image_render.rs, app/image_paste.rs}`. Effort: **L**.

### F.7 Chrome & feedback

- [Y] **Splash animation** — animated startup banner with deterministic RNG traces. Ref: `craft-ui/src/splash.rs`. Effort: **S**.
- [Y] **Notifications** — bell notifications ranked by urgency (permission > completion), only when terminal unfocused, for TurnComplete/PermissionRequested/AuthRequired/QuestionAsked/PlanReady. Ref: `event_loop.rs:262-330`. Effort: **S**.
- [Y] **Status bar** — mode label, cwd, async git branch polling, timed flash toasts. Effort: **S** (we show branch already; deltas are mode/cwd/toasts).
- [Y] **Thinking/streaming indicators** — `ThinkingDelta` rendering + prompt-progress display. Ref: `components/streaming_content.rs`. Effort: **S**. Depends: C.1.

## G. CLI, headless modes & protocols

Our CLI has zero flags and one subcommand (`acp`). The reference surface:

- [Y] **G.1 Core flags** — `-p/--print`, `--image <PATH>` (repeatable, vision content), `-m/--model provider/model-id`, `--verbose` (full turn-by-turn in print mode), `-c/--continue` (resume latest session in cwd), `-s/--session <ID>` (resume specific, alias `--resume`), `--output-format text|stream-json`, `--mode build|plan|flow`, `--input-format text|stream-json`, `--max-turns`, `--system-prompt`, `--append-system-prompt`, `--yolo` (alias `--dangerously-skip-permissions`), `--auto-review`/`-A`, `--exit-on-done`, `--allowedTools`/`--disallowedTools` (comma lists), `--no-commands`, `--session-id`, `--fork-session`, `--permission-mode`, `--include-partial-messages`. Ref: `src/cli.rs:44-221`. Effort: **M** (flag plumbing) — each flag's semantics depends on its subsystem.
- [ ] **G.2 Subcommands** — `auth login/status <provider>`, `models`, `completions <shell>`, `stats [--sessions]`, `mcp auth`, `update [-y] [--no-color]`, `rollback`, `acp [--yolo] [--auto-review] [--cwd]`, `term` (shell integration), `doctor [--export]`, `prompt [system|research|general] [--plan] [--tools] [--names]`. Ref: `src/cli.rs:223-317`, `src/cmd/subcmd/`. Effort: **S each** behind their subsystems.
- [Y] **G.3 Print mode (headless)** — non-interactive run with `text` or `stream-json` output (Claude Code–compatible event stream), piped-stdin prompt, images, verbose transcript. Known reference bug worth not porting: agent errors don't set a nonzero exit code (only dispatch failures exit 1). Ref: `src/print.rs`. Effort: **M**. Depends: C.14-style headless API.
- [Y] **G.5 ACP server (full)** — ours covers initialize/session/config-options/prompt/cancel. The reference adds: permission requests & elicitation to the client, MCP server passthrough (`mcp.rs`), custom-command advertisement (`commands.rs`), full session-update translation (46k `translate.rs`), load-session/resume, mode configuration. Ref: `craft-acp/src/{server.rs (80k), translate.rs, elicitation.rs, mcp.rs, commands.rs}`. Effort: **L** (incremental on our base).
- [Y] **G.6 Setup & first-run flow** — interactive first-run provider/auth setup. Ref: `src/setup.rs`. Effort: **M**. Depends: H.5.
- [Y] **G.7 Self-update & rollback** — `craft update` fetches+execs `install.sh` from the main branch (⚠️ no checksum/tag pinning — if ported, pin a release digest), `craft rollback`, background update check. Ref: `src/update.rs`, `craft-storage/src/version.rs`. Effort: **M**.
- [Y] **G.8 Shell completions** — clap_complete generation. Effort: **S**.
- [Y] **G.9 Terminal shell integration (`craft term`)** — transparent command logging + `@craft` alias hook for the user's shell. Ref: `src/cmd/subcmd/term.rs`. Effort: **M**.

## H. Providers & auth

We use Rig with 26 provider kinds + TOML config. The reference has a fully custom
stack — different shape, not strictly a superset, but these capabilities are absent
in ours:

- [Y] **H.2 models.dev catalog** — Opencode provider fetches `models.dev/api.json` with 24h disk cache for provider/model discovery. Ref: `craft-providers/src/providers/opencode.rs`, `model_registry.rs`. Effort: **M**.
- [Y] **H.3 Model registry & tiers** — 3-layer tier assignment (user overrides persisted > manifest > auto), weak/medium/strong/compaction tiers used for routing and `/models`. Ref: `craft-providers/src/model_registry.rs`. We should auto assing models for advisors, subagents and flow mode as well to weak/medium/strong Effort: **M**.
- [Y] **H.6 Usage & cost accounting** — per-call `TokenUsage{input, output, cache_creation, cache_read}`, per-model accumulation, priced turns incl. cache tokens and schedules, session settle/recompute. Ref: `craft-providers/src/{model.rs, pricing.rs}`. Effort: **M**. Prerequisite for D.4, C.20, `/usage`.
- [Y] **H.11 Timeout/retry policy** — shared connect/low-speed/stream timeouts + retry policy per provider. Ref: `craft-providers/src/provider.rs`. Effort: **S** (Rig may cover; verify).

## I. Storage & sessions

We persist nothing (in-memory sessions only).

- [Y] **I.1 Path resolution** — XDG dirs with legacy `~/.craft/` adoption, project→global config search, data/state/logs/cache split. Ref: `craft-storage/src/paths.rs`. Effort: **S**.
- [Y] **I.2 Session persistence (JSONL)** — append-only per-session file: header (id, cwd, model), meta + message records; epoch/revision counters distinguishing appends from rewrites; archived hardlink copies (keep 3) when history shrinks; `cwd_latest.json` resume index; legacy `.json` migration. Payload includes tool-output map, subagent transcripts, per-model usage, and meta (mode, session rules, context size, input draft, queued messages, thinking, goal, flow state). Ref: `craft-storage/src/sessions.rs` (3.1k). Effort: **L**. Prerequisite for resume (F.3), rewind, `/sessions`.
- [Y] **I.3 IDs** — `CraftId`: UUIDv7 base58, time-sortable, legacy-v4 parseable. Ref: `craft-storage/src/id.rs`. Effort: **S**.
- [Y] **I.5 Plans storage** — slug-named markdown plan files (`<adjective>-<noun>.md`). Ref: `craft-storage/src/plans.rs`. Effort: **S**.
- [Y] **I.6 Cost ledger** — append-only `cost.jsonl` per run for `/stats`. Ref: `craft-storage/src/stats.rs`. Effort: **S**. Depends: H.6.
- [Y] **I.7 Misc state** — input history (100), last/recent models, theme name, rotating logs (200MB × 10, flock), atomic tempfile+rename IO with 0600 secret variants. Ref: `craft-storage/src/{input_history.rs, model.rs, theme.rs, log.rs, lib.rs}`. Effort: **S**.

## J. Extensibility (plugins, skills, commands, packages)

- [Y] **J.4 Skills** — compile-time bundled SKILL.md set + file discovery (`.craft/.agents/.claude/.opencode` `skills/` dirs, project > global > builtin shadowing), surfaced via the `skill` tool and `skill://` reads. Ref: `craft-agent/src/{builtin_skills.rs, discovery.rs}`, `plugins/skill/`. Effort: **M** without the Lua part.
- [Y] **J.5 Custom commands & recipes** — `.craft/commands` + `.claude/commands` slash-command files (`$ARGUMENTS`, frontmatter); YAML/JSON **recipes** (parametrized prompts rendered with minijinja, typed coercion) browsable via `/recipe` and runnable headless via `craft run <recipe>`; `template.rs` env/date/cwd variable substitution. Ref: `craft-agent/src/{command.rs, recipe.rs, template.rs}`, `src/cmd/subcmd/recipe.rs`. Effort: **M**.
- [Y] **J.6 Flow workstreams (persistence)** — typed append-only `log.jsonl` + `projections.json` per workstream, `workstream.json` state, goal-approval gate, `craft flow run/prune` CLI; Ref: `craft-storage/src/flow.rs`, `craft-agent/src/{typed_log.rs, flow_loop.rs}`. Effort: **XL** (with C.17).

---

## Summary counts by area

| Section | Theme | Gap items |
| --- | --- | --- |
| A | Tools | 34 |
| B | Tool infrastructure & permissions | 12 |
| C | Agent loop & orchestration | 20 |
| D | Context management & compaction | 9 |
| E | Reliability & guardrails | 10 |
| F | TUI & terminal experience | 35 |
| G | CLI, headless & protocols | 9 |
| H | Providers & auth | 11 |
| I | Storage & sessions | 8 |
| J | Extensibility | 8 |
| K | Support crates | 4 |

160 checkbox items total (the F.4 slash-command entry bundles ~29 commands into one checkbox).

Suggested triage order if you want maximum value first (purely by leverage-per-effort,
not a decision): tool-output pre-compression (B.5) + read lifecycle (D.2) + dedup
cache (B.6) for token savings; snapshots/undo (E.1) + permission engine (B.1) +
bash (A.1) for capability; session persistence (I.2) + resume for durability; then
subagents, semantic layer, and Lua/plugins last since they are the deepest subsystems.
