# 06 — Native async tools on `ToolImpl`; rig removed

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt),
  including the [async rules](README.md#async-native).
- When implementation is finished, **run the review agent**
  (`.craft/agents/reviewer.md`) as described in [Finish](#finish).

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

1. **Tool definitions.** Replace `impl_tool!` with a macro, or plain impls, that
   implements `ToolImpl` for each tool:
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
   - `question`, `batch`, `task` and `bash*` stay `Exclusive`, because shell
     commands can touch anything.
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
6. **Registration.** `Workspace::register` fills an `animist::agent::tool::ToolRegistry`,
   each tool wrapped in 05's `HarnessExecutor`. Delete the rig bridge,
   `ToolDispatch` remnants and `SharedDispatch`, if 10 doesn't need it.
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
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note: the claim table per tool, the key scheme, the
   error-text decision, and the remaining `spawn_blocking` sites.

## Completion note

_(fill in when done)_
