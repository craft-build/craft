# 06 — Native async tools on `ToolImpl`; rig removed

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt),
  including the [async rules](README.md#async-native), and the
  [harness design rules](../../animist/harness-design.md). Start with the
  [test inventory](../../animist/harness-design.md#test-integrity).
- When implementation is finished, **run the review agent**
  as described in [Finish](#finish).

## Goal

Port every built-in tool to `animist::agent::tool::ToolImpl`, and every MCP
tool to `ToolExecutorImpl`.

- Tools are async-native.
- They express conflicts as `ToolScheduling` claims instead of a global lock.
- They produce animist `ToolOutput`/`ToolContents`.

Delete 05's rig bridge and the `rig-core` dependency.

## Read first

- animist 0.7.0: `src/agent/{tool.rs, testing.rs}`, `docs/agents.md` →
  *Typed tools*, *Direct tool tests* and *Bounded scheduling*. Claims cover the whole authorize/execute/result
  pipeline, resource keys are opaque (craft owns normalization), and a
  conflicting call is a barrier.
- Craft:
  - `agent/src/tools/mod.rs`: `impl_tool!`, `Workspace::run` (today every
    filesystem tool runs under `spawn_blocking` holding one `std::sync::Mutex`,
    which is why tools are effectively serial), the error helpers and
    `BUILTIN_TOOL_NAMES`
  - `agent/src/tools/{bash.rs, read.rs, write.rs, edit.rs, multiedit.rs,
    apply_patch.rs, grep.rs, glob.rs, list.rs, move_file.rs, delete.rs,
    view_image.rs, question.rs, task.rs, batch.rs, list_tools.rs, webfetch.rs,
    websearch.rs, retrieve.rs, skill.rs, sessions.rs, todo_write.rs}`
  - `agent/src/tools/tests/` (the `invoke(..)` helper has about 141 call sites)
  - `agent/src/run/dedup.rs` (`extract_write_paths`, `normalize_write_path_with_root`)
  - `agent/src/mcp/{mod.rs, manager.rs, session/rmcp.rs}`
  - `docs/hardening/13-full-file-write-concurrency.md`

## Work

1. **Tool definitions.** Replace `impl_tool!` with plain `ToolImpl` impls.
   Use a macro only if it removes real repetition that a helper function
   can't, and keep it under ~30 lines. Each tool holds what it uses directly
   (workspace root, path-lock table, config slice). No dispatcher object
   sits between the registry and the tool. In each impl:
   - `Args` are the existing typed structs (serde decode is animist's, and
     argument errors become model-visible error results).
   - `Output: IntoToolOutput`. Keep the readable-text contract for the model
     and ACP; JSON renders to text; images become
     `ToolContent::Image { source: ImageSource::Base64 }`.
   - `Error` is a snafu `tools::Error` replacing `ToolExecutionError` +
     `ToolErrorKind`. The model-visible text must not change: animist renders
     `ToolError::Execution` as `"Tool execution failed: {source}"`. Check
     whether that prefix changes what the model sees. If it does, return
     `Ok(ToolOutput { is_error: true, .. })` for expected failures, keep `Err`
     for real execution faults, and pin the strings.
   - `definition()` uses the schemars schema, keeping the bare-object fallback.
2. **Scheduling claims** (`ToolImpl::scheduling`):
   - Read-only path tools (`read`, `grep`/`glob`/`list` on a root) claim
     `reads` of their normalized path or root.
   - Writers (`write`, `edit*`, `multiedit`, `apply_patch`, `delete`,
     `move_file`) claim `writes` of every normalized path, using
     `extract_write_paths` + `normalize_write_path_with_root`.
   - Directory walks must conflict with writes beneath them. Pick a key scheme
     (ancestors in the claim, or a conservative workspace-wide `reads` key that
     every write also claims) and document it.
   - `question`, `batch` and `bash*` stay `Exclusive`, because shell
     commands can touch anything; `batch` must be, as `dispatch_all`
     requires it. `task` and `review` claim empty resources: the child
     conversation claims its own resources, and cross-agent file ordering
     is `PathLocks`', so sibling subagents share a wave.
   - Network-only tools (`webfetch`, `websearch`) claim empty resources:
     independent.
   - MCP tools stay `Exclusive` unless the server marks them read-only, and
     then only if you can justify it.
   - This replaces `is_never_parallel` and the write-path barrier. When in
     doubt, use `Exclusive`.
3. **Async native.**
   - Remove `Workspace::run`'s `spawn_blocking` + `std::sync::Mutex`. Tool
     bodies use `tokio::fs` and `tokio::process`.
   - Keep `spawn_blocking` only for sync-only, CPU-bound work (the `ignore`
     walker in grep/glob, `similar` diffs on large files, tree-sitter, image
     decode), each with a one-line reason comment.
   - Cross-agent safety: claims only order calls within one agent. Subagents
     and concurrent sessions in one process share the workspace, so add an
     async per-path lock table (`tokio::sync`, keyed by the same normalized
     keys, writers exclusive). Writing tools take it for the duration of the
     write. Lock acquisition races the run's cancellation.
   - Hardening/13's lost-update protection must still hold.
4. **Context use.**
   - `task` and anything else that used `current_call_id` reads
     `context.call.id`. Delete the task-local.
   - `bash`/`bash_watch` stream accumulated output with
     `context.progress(full_so_far)`.
   - Long-running tools race `context.run.cancellation`.
   - `batch` and `task` are ported in 10. For now, keep their existing
     behaviour through the same `ToolImpl` shape: `batch` calls
     `context.dispatch_all` with all its entries.
5. **MCP.** The MCP adapter implements `ToolExecutorImpl` (runtime names):
   - its definition comes from the server listing;
   - cancellation comes from `context.run.cancellation`;
   - outputs and images map to `ToolContents`;
   - builtin-name collision rejection is kept.
6. **Registration.** One function builds the `animist::agent::tool::ToolRegistry`
   from config, registering each tool directly: no decorator (snapshots are
   05's `SnapshotLayer`). `Conversation` calls it when it builds or
   rebuilds the agent.
   - `Workspace` either becomes a plain value (root, path locks, env) shared
     by the tools, or disappears. It no longer runs or dispatches anything.
   - Delete the rig bridge, the `ToolDispatch` remnants and
     `SharedDispatch`. `batch` gets its children from
     `ToolContext::dispatch_all`, not from a back-reference.
   - Keep `AskQuestions` and `SpawnSubagent` only as surface seams. Each has
     several real implementations (TUI, ACP, headless, tests); name them in
     the completion note, or inline the trait.
7. **Test harness** (README decision 1). Build on animist's
   `agent::testing::ToolHarness` (dev-dependency feature `testing`, added in
   01). No provider, model or one-shot agent is involved.
   - Re-implement the existing `invoke(..)` helper as
     `harness.call(tool, args)`, so the ~141 call sites stay unchanged.
     `call` returns structured `ToolError`s (including `Arguments` and
     `Panicked`) rather than model-visible text. Where a test asserts the
     model-visible string, render it the way the agent does and pin that.
   - Progress and child events come from `harness.events()`; cancellation from
     `harness.cancellation()`.
   - Root calls bypass layers, timeouts and counters. Tests that need policy
     (approval, guardrails) run through the agent with `ScriptedProvider`
     instead (07).
   - Children dispatched by `task`/`batch` use the harness registry and the
     optional `layers`.
   - Pure logic (path validation, fuzzy replace, patch parsing) keeps direct
     unit tests with no harness.
8. **Remove rig.** Delete `rig-core` (including the `test-utils`
   dev-dependency) from both `Cargo.toml`s.

## Tests

- All existing `tools/tests/*` pass through the harness.
- Claim tests per tool family. animist's `conflicts` is still `pub(crate)` in
  0.7.0, so mirror its rule in a test helper and cite the source. Then cover end to end through
  the agent, with `max_concurrent_tools = 8`:
  - two `read`s overlap (use barrier-instrumented fake I/O or timing-free
    ordering assertions);
  - `write` + `read` of the same path serialize;
  - `bash` is a barrier;
  - a directory `list` conflicts with a write beneath it.
- Cross-agent: two agents writing the same file serialize through the path
  lock, and hardening/13's tests pass.
- Cancellation during a long `bash` and during a path-lock wait.
- MCP image and text results map in order.
- Model-visible error strings are unchanged.

## Acceptance

- `cargo tree -p craft | grep -i rig` is empty, and `grep -rn 'rig_core'
  agent/src` is empty.
- No `std::sync::Mutex` is held around tool I/O, and `spawn_blocking` remains
  only at commented CPU or sync-only sites.
- Model-emitted parallel calls really run in parallel when their claims allow.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note:
   - the claim table per tool;
   - the key scheme;
   - the error-text decision;
   - the remaining `spawn_blocking` sites;
   - the test-inventory diff;
   - the indirection rows closed;
   - which tool-side traits survived, and why.

## Completion note

**2026-10-31 · commits `4911907b..HEAD` (the cutover plus the review-fix
and note amendments) · `cargo test -p craft`: 1891 passed, 3 failed (the
three pre-existing recipe render fixed points), 29 ignored · reviewer
verdict: approve_with_nits → clean after three iterations (all findings
addressed) · clippy at the 01 baseline (zero new warnings),
`cargo fmt --check` clean.**

### Claim table per tool

| Tool | Claim |
| --- | --- |
| `read`, `view_image` | `reads: [norm(path)]` (resource URLs claim the normalized URI string) |
| `grep`, `glob`, `list` | `reads: [norm(start dir)]` |
| `write`, `edit`, `edit_lines`, `insert_lines`, `multiedit` | `writes: [norm(path) + ancestor dirs up to and including the root]` |
| `apply_patch` | writes of every parsed `patch_paths` target + ancestors |
| `delete` | same shape as write; **exclusive** when any target is a directory (or unreadable — fail closed), because file-level claims cannot cover a recursive removal |
| `move` (`move_file`) | exclusive (import rewrite touches any file); workspace-wide lock |
| `bash`, `bash_status`, `bash_watch`, `bash_kill` | exclusive |
| `question` | exclusive (parks on the host seam) |
| `task`, `review` | independent (empty resources; the child conversation claims its own, cross-agent ordering is `PathLocks`') |
| `batch` | exclusive (required by `dispatch_all`) |
| `todo_write` | `writes: ["todos"]` |
| `retrieve`, `skill`, `sessions`, `list_tools` | independent (empty resources) |
| `webfetch`, `websearch` | independent (network-only) |
| argosy knowledge tools | `reads: [norm(root)]` — they overlap reads and each other (the service serializes its own mutations internally) and exclude writers |
| MCP tools | independent when the server asserts `read_only_hint: true`; exclusive otherwise |

### Key scheme

Keys are `normalize_write_path_with_root(root, p)` — the same normalization
the dedup cache uses, so claims, the cross-agent path locks, and dedup
invalidation can never disagree (claims and locks share
`claims::write_keys`). Readers claim their exact path or start directory;
writers claim each target **plus every ancestor directory up to and
including the workspace root**. Consequences (all pinned by
`claim_scheme_conflicts_match_the_documented_cases`): two reads overlap;
same-path write/read and write/write serialize; a directory walk conflicts
with any write beneath it; and — deliberately — **every write shares the
root ancestor, so writes serialize workspace-wide** (the prompt's
alternative "conservative workspace-wide key" collapses to this same
shape). Targets outside the root (the plan file) claim the target alone.

Cross-agent safety: `tools/locks.rs` — a per-workspace
`tokio::sync` lock table (shared root RwLock + per-key RwLocks, acquired
root-first then sorted keys; writers hold exclusive key guards under a
shared root guard; `move` and tree deletes hold the root exclusively).
Acquisition races the run's cancellation. Pinned by
`cross_agent_same_file_writes_serialize_through_the_path_lock`,
`tree_delete_excludes_a_concurrent_directory_walk`, and
`cancellation_during_a_path_lock_wait_returns_promptly`.

### Error-text decision

Every model-correctable failure (validation, denial, not-found, io, seam
errors) returns `Ok(ToolOutput { is_error: true, content: <exact legacy
text> })` — model-visible strings are byte-identical to the rig
dispatcher's; `Error::Io` keeps the io error as its snafu source with
`display("{source}")`. `Err` is reserved for machinery faults
(`Blocking`, `LockCancelled`), which animist renders with its
`Tool execution failed: ` prefix. Three intentional, pinned text changes:
argument-decode failures now read animist's `Invalid tool arguments: …`
(was `invalid arguments: …`), unknown tools read animist's
`Unknown tool: {name}` (was craft's longer hint — the hint survives on
`list_tools`' unknown-detail path), batch dispatch failures read
`batch dispatch failed: …` (pinned by
`dispatch_failure_is_reported_with_our_wording`), and the fail-closed
delete re-check reads
`{path} became a directory before the delete; retry` (pinned by
`target_becoming_a_directory_before_the_delete_fails_closed`).
Machinery-fault wording is animist-composed and left unpinned except the
batch string.

### Remaining `spawn_blocking` sites (each with a reason comment)

grep (ignore walker + regex scan, one bounded unit) · glob (ignore
walker) · move_file (walker + import rewrites) · view_image (image
decode/downscale/re-encode) · edit (CPU-bound fuzzy match) ·
`Workspace::note_snapshot` (sync snapshot store) · `stage` (tempfile +
fsync) · read's `argosy://` (sync argosy service) · read's `skill://`
(sync discovery walk) — plus the pre-existing non-tool sites
(auto-review decide, runtime env, knowledge, mcp manager, snapshot, TUI
cards/resume/usage). `similar`-diff rendering stays sync: it runs in the
sync output-conversion path (`IntoToolResult`), as it did under rig.

### Test-inventory diff

`1922 → 1922` listed tests (1890 pass; 29 ignored = 28 prior + the
restored-for-07 `children_feed_guardrail_counters`; the three failures
are the pre-existing recipe fixed points). 19 removed / 19 added:

- `run::dispatch::plan_mode_tests` (6) — plan gating lives in
  `PolicyLayer`; covered by `harness/policy.rs`'s gate tests and
  `run/tests/cli_policy`.
- `run::dispatch::result_content_tests` (2) — rig content mapping died
  with rig; pretty-JSON-to-text stays pinned by `task`'s
  `json_result_is_pretty_printed`.
- `after_execute_transform_rewrites_results`,
  `dedup_replays_through_after_hook` → `post_exec_layer_transforms_results`
  (layer `post_tool_exec`, exercised through a batch child since root
  harness calls bypass layers); the replay half is
  `dedup_replays_identical_read_calls` (the `[cached]` marker was a
  dispatch-path artifact that 05's agent path never had).
- `never_parallel_tool_waits_for_earlier_calls` → `exclusive_tool_waits_for_earlier_calls`.
- `unknown_tool_becomes_recoverable_error_result` → folded into
  `unknown_tool_surfaces_error_and_run_continues` (pins animist's wording).
- `children_go_through_the_approval_gate` → `approval_gate_reaches_batch_children`.
- `children_go_through_the_after_hook` → `post_exec_layer_transforms_results`.
- `children_feed_guardrail_counters` → restored `#[ignore = "restored in 07"]`
  (the counters move into the GuardrailLayer's post-hook in 07).
- `registered_in_the_workspace_dispatch_table` → `registered_in_the_workspace_registry`.
- `concurrent_edits_share_one_workspace_lock` → `concurrent_edits_share_one_path_lock`.
- `plan_mode_sandbox_cell…` / `build_mode_and_general_subagents_share_the_session_cell`
  — the mode-frozen sandbox cells were unreachable since 05 (every surface
  registered Build); the surviving freeze is pinned by
  `read_only_workers_get_a_frozen_read_only_cell`.
- Added beyond the replacements: the claims suite (table, mirrored
  `conflicts` rule citing animist's `pub(crate)` source, scheme cases,
  MCP hint shapes), the end-to-end scheduling tests the prompt requires
  (overlapping reads, same-path serialization, exclusive barrier, dir
  walk vs write), the cross-agent and cancellation tests, the tree-delete
  exclusion test, and `independent_calls_run_concurrently` restored from
  its 06 ignore (barrier probes through the conversation).

### Indirection rows closed

`PortableTool`/`DynamicTool`/`impl_tool!`/`dynamic()`/`tool_name()`/
`animist_definitions()` (rig adapters) · `Workspace::run` (global
`std::sync::Mutex` under `spawn_blocking`) · `harness/rig_bridge.rs` ·
`run::dispatch::{ToolDispatch, SharedDispatch, AfterExecute,
DispatchOutcome, CURRENT_CALL_ID, call_parts}` · `Workspace::{before,
with_before, mcp_cancel, with_cancel, register, register_with_mode,
register_subagent, register_reviewer, tool_definitions,
turn_sandbox_cell}` · `batch`'s `SharedDispatch` back-reference ·
`dedup::normalize_write_path`'s process-global `OnceLock` root (the
root is always explicit now).

### Surviving tool-side traits, and why

- `AskQuestions` (question.rs): four real implementations — TUI
  `QuestionAsker` (parks on the shared park + question form), ACP
  `ElicitationAsker` (client elicitation), headless `DismissAsk`
  (default), and the test `Scripted`.
- `SpawnSubagent` (task.rs): `SubagentLauncher` (TUI/ACP/headless) and
  `NoSubagents` (default).
- `BeforeExecute`/`Decision` (now in `run`): the approval seam with the
  TUI `ApprovalGate`, ACP `AcpPermissionGate`, headless `HeadlessGate`,
  and the auto-review compound gate; consumed by `PolicyLayer`.
- `IntoToolResult` (tools/error.rs): renders each tool's typed output
  into its model-visible `ToolOutput` (animist's `IntoToolOutput` is
  async-futured and Result-returning; the tools' rendering is sync and
  infallible).
