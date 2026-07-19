---
type: Decision
description: Design decisions for porting maki commit 4867eed (concurrent sessions). Phase 4 blockers.
tags:
- sessions
- port
- maki
- event-loop
- design
timestamp: 2026-07-18T13:13:58Z
---
# Concurrent sessions: port of maki commit 4867eed

Source: maki commit `4867eed138007668fa7550a31998a076204a3569` ("ui: run sessions
concurrently, one /sessions picker for all of them") by Tony Solomonik.

## Status

Phases 1-3 are merged in tree. Phase 4 (UI core rewrite) is blocked on three
design decisions that have no maki equivalent and need an owner.

## Done (Phases 1-3)

### Phase 1: Foundation
- `Cargo.toml`: added `select` feature to flume 0.12.
- `craft-agent/src/permissions.rs`: added `PermissionManager::fork()` for per-session rules.
- `craft-config/src/lib.rs`: added `"sessions"` to `DEFAULT_BUILTINS`.
- `craft-lua/src/api/util/command.rs`: added `SessionRequest`, `SessionReply`, `UiAction::Session`.
- `craft-ui/src/animation.rs`: added `animation_elapsed_ms()` static epoch helper.
- `craft-ui/src/event_loop.rs`: stub `UiAction::Session` arm (returns "unimplemented") — to be wired in Phase 4.

### Phase 2: Storage layer (`craft-storage/src/sessions.rs`, `craft-ui/src/storage_writer.rs`)
- `SessionLog::append` persists meta-only changes via a `saved_meta: Vec<u8>` cursor and `meta_record()` helper. Rollback on failed write (`set_len` to last record boundary).
- `SessionLog::open` truncates torn tails at the last `\n` before parsing.
- `load_jsonl(data: &[u8], display_path: &str)` signature (was `path: &Path`).
- `cursor_from` seeds all cursors directly (removed `sync_cursors`).
- Scan cache: `scan_cache.json` keyed by filename, validated by `(size, mtime_ms)`. New structs `ScanCacheEntry`, `ScannedHeader`, helpers `load_scan_cache`, `file_signature`. `scan_jsonl_header`/`scan_legacy_header` return `ScannedHeader` (cwd filter moved into `scan_headers`).
- `write_session_file` extracted; `save_to` calls it directly + `update_cwd_index`.
- `update_cwd_index` no-op guard when index already points at session.
- `is_session_file` excludes `scan_cache` stem.
- `StorageWriter`: per-session-id `HashMap<CraftId, Box<AppSession>>` pending map (was single `latest` slot). Deletes run on writer thread via `Op::Delete { id, done }`. Cost-writer thread preserved.

### Phase 3: Lua API surface (`craft-lua/src/api/session.rs`, `craft-lua/src/api/ui/win.rs`, `craft-lua/src/api/mod.rs`, `craft-lua/src/lib.rs`)
- New `craft.session.{list,live,current,new,focus,delete,set_title}` round-tripping through `UiAction::Session`.
- `SessionStatusChanged` autocmd name reserved (fired from Phase 4 loop).
- `win:recv(timeout_ms?)` adds optional timeout returning `{type="timeout"}`. WinHandle fields are `Cell<bool>` so methods take `&self` and a parked `recv` doesn't block other win calls from other coroutines. Registered as `add_async_function` (not method) so the userdata borrow isn't held across the await.
- Win event/command channels changed `bounded(8)` to `unbounded` so a full channel can't drop a `Close` and leave a zombie modal.
- Adapted to tokio (maki used smol): `tokio::time::sleep` + `tokio::select!` for the timeout race; `#[tokio::test]` for async tests.

## Blocked: Phase 4 design decisions

The maki rewrite moves `EventLoop` from owning one `App` + `AgentHandles` to a
`Vec<SessionRuntime>` selected over a `flume::Selector` across input / ui_action
/ per-session agent / per-session shell / warn channels, with terminal input on
a dedicated reader thread. Craft's event_loop.rs (1,013 lines) has diverged
past maki's baseline (673 to 1,077 lines) with features maki doesn't have, and
three of them are structural blockers for a faithful port.

### Blocker 1: `embed_rx` is a single unclonable receiver

`EventLoopParams.embed_rx: Option<flume::Receiver<craft_agent::EmbedRequest>>`
(`craft-ui/src/event_loop.rs:92`) is consumed by a single `AgentHandles::spawn`
(`craft-ui/src/agent/mod.rs:76`, `:263`). Under `Vec<SessionRuntime>`, every
runtime wants its own agent, but `flume::Receiver` is not `Clone`.

**Options:**
- a) Switch to `tokio::sync::broadcast` so every runtime subscribes. Requires
  changing the agent-side consumer and the `EmbedRequest` flow.
- b) Keep `embed_rx` on `SpawnCtx` as a single shared `Arc<Mutex<Receiver>>`
  and have agents pull from it. Contention risk; semantically odd.
- c) Promote embed to a per-runtime channel created in `spawn_runtime`. Means
  embed producers need to target a specific session, likely the right
  answer if embeds are always user-driven from the focused session.

**Recommended:** (c) embed feels session-scoped, but confirm with the embed
feature owner.

### Blocker 2: `ShutdownResult` ownership model

`ShutdownResult { session_id, exit_code, handles, storage_writer }`
(`craft-ui/src/event_loop.rs:42`) owns exactly one `AgentHandles` and one
`StorageWriter`. Maki's `shutdown` returns `(Option<MakiId>, i32)` and walks
every runtime. Under multi-session, "the handles" is ambiguous.

**Options:**
- a) Change `ShutdownResult` to `Vec<AgentHandles>` (one per session) +
  single `storage_writer` (already per-session internally).
- b) Drop `handles` from `ShutdownResult`; shutdown drains all runtimes
  internally and returns only `(session_id, exit_code)`.
- c) Shutdown becomes per-runtime and the outer caller loops.

**Recommended:** (b) `ShutdownResult` shouldn't carry handles; shutdown
should consume them. Check `craft-desktop`/`craft-acp` callers first
(`craft-ui/src/lib.rs:37-40` is the type alias).

### Blocker 3: Global `watch_handle` + `context_window_overrides` key off focused cwd

- `watch_handle` (`craft-ui/src/event_loop.rs:116`) is one watcher bound to
  `self.app.state.session.cwd`. Multi-session means N cwds; either spawn one
  watcher per runtime or re-bind on focus change.
- `context_window_overrides: ArcSwap<...>` is global; the focused session's
  model drives `apply_active_context_window_override`. Model changes already
  propagate to all sessions in maki's `drain_channels`, but the override
  application needs to be focus-aware.

**Options:**
- a) Per-runtime `watch_handle` in `SessionRuntime`. Cleanest, but watch mode
  becomes session-scoped (probably what users want anyway).
- b) Re-bind the global watch_handle in `set_focus`.
- c) Drop watch_handle into `SpawnCtx` as `Arc<Mutex<Option<WatchHandle>>>`
  and re-arm on focus.

**Recommended:** (a) per-runtime. Same story for `context_window_overrides`:
make it per-runtime and have the Lua/config mutation target the focused
runtime.

## Other Phase 4 work (mechanical, after blockers resolve)

- Port `craft-ui/src/input.rs` (`InputReader`, `PauseGuard`) — zero design work.
- Add `App::awaiting_input()` (maki has it, craft doesn't) for `SessionStatus::of`.
- Introduce `SessionRuntime`, `SpawnCtx`, `Wake`; rewrite main loop with `flume::Selector`.
- Implement `handle_session_request` (List/Live/Current/New/Focus/Delete/SetTitle) — wire the existing `UiAction::Session` stub at `craft-ui/src/event_loop.rs:538-544`.
- Delete `craft-ui/src/components/session_picker.rs` and purge references in `app/{mod,session,view}.rs`, `components/{keybindings,mod}.rs`.
- Slim `craft-ui/src/components/list_picker.rs` (guts move to Lua).
- Port `plugins/lib/craft/list_picker.lua` enrichment (word filter, match-range highlighting, ranking).
- Author `plugins/sessions/init.lua` (~588 LoC) using `craft.session.*` + `win:recv(timeout)` + `SessionStatusChanged` autocmd. Register as bundled plugin in `craft-lua/src/loader.rs`.
- Loader priority queue (`prio_tx` for `RunCommand`/`RunKeybindCallback`; restores use bulk `tx`). **Deferrable** — UX-only, not correctness.
- Re-add `pub(crate) const SPINNER_STYLE_PREFIX: &str = "spinner:";` to `craft-ui/src/components/tool_display.rs` when the spinner-style consumer lands.

## References

- Maki diff: `git show 4867eed138007668fa7550a31998a076204a3569`
- Maki new event_loop: `git show 4867eed138007668fa7550a31998a076204a3569:maki-ui/src/event_loop.rs`
- Attribution: Tony Solomonik `<tony.solomonik@gmail.com>`; upstream URL `https://github.com/tontinton/maki`.
