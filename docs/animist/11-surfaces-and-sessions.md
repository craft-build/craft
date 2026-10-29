# 11 — Surfaces own the conversation; sessions persist `HistoryEntry`s

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt).
- When implementation is finished, **run the review agent**
  (`.craft/agents/reviewer.md`) as described in [Finish](#finish).

## Goal

The TUI, ACP, headless and print surfaces each hold a long-lived
`harness::Conversation` per session instead of a `Vec<Message>`. Sessions
persist animist `HistoryEntry`s incrementally by replaying animist's history
change feed. The agent already squashes tool-result revisions (README
decision 7), so craft does no pruning. Delete the `run::run` facade. After
this prompt the migration branch can merge.

## Read first

- Craft:
  - `agent/src/tui/provider/live/{turn.rs, resume.rs, bootstrap.rs,
    commands.rs, usage_recorder.rs}`
  - `agent/src/acp/{mod.rs, turn.rs}`
  - `agent/src/headless.rs`, `agent/src/print.rs`, `agent/src/cli.rs`
    (`run_headless_query`), `agent/src/runtime.rs`
  - `agent/src/storage/sessions/{mod.rs, log.rs, archive.rs, legacy.rs}`
    (epoch, rewrites, O(delta) cursors)
  - `agent/src/snapshot.rs` (`/undo` commit points)
  - `docs/hardening/09-continue-and-fork-semantics.md`,
    `docs/hardening/14-honest-and-complete-undo.md`,
    `docs/hardening/28-session-model-index-fallback.md`
- animist 0.7.0: `docs/history.md` → *Entries and views*, *Agent tool-result
  revision retention*, *Atomicity and concurrency* (the change feed), and
  persistence via `snapshot().entries()` / `History::from_entries`.

## Work

1. **Conversation lifetime.**
   - Each surface session creates one `Conversation`. It is built from the
     resumed `History::from_entries(..)` or from empty.
   - Runs go through `harness::driver::run_turn`.
   - Model switch, mode switch and tool-set change (MCP connect or disconnect)
     rebuild the agent on the same `History` between runs (README decision 6).
   - Teardown calls `Conversation::shutdown().await` on every exit path: TUI
     quit, ACP session close, headless end.
   - Keep `runtime::run_policy` as the single source of params (hardening/01).
2. **Session record format.** Bump the version again. New records map one to
   one onto `HistoryChange`:
   - `entry { entry }` ← `Appended` (source plus any revisions present);
   - `insert { index, entry }` ← `Inserted`;
   - `revise { index, message }` ← `Revised`;
   - `deactivate { index }` ← `Deactivated`;
   - `replace { index, entry }` ← `EntryReplaced` (the agent's tool-result
     squash: replace the persisted entry, don't append a revision).

   Indices are as animist defines them: each refers to the entry list at that
   individual event, so replaying records in order reproduces the snapshot. A
   committed compaction plan therefore persists as a handful of small
   records, with no full rewrite.
   - The header, meta, subagent and usage records stay as they are, and
     subagent histories become entry lists.
   - The `tool_outputs` record type is gone: 09 moved raw outputs into entry
     sources. The loader still accepts old `tool_outputs` records. Fold them
     into the matching tool-result entry as its source if that entry has no
     richer original; otherwise drop them.
3. **Incremental sync** (`harness::persist`). One persister per
   `Conversation`:
   - On load or a full write, persist `snapshot().entries()` and keep
     `snapshot.generation()` as the cursor. `History::from_entries` starts a
     fresh feed, so take the cursor from the restored history, never from an
     earlier one (`ForeignVersion`).
   - At each checkpoint (after each run, and at today's mid-run tool-result
     checkpoints that `content_revision` drives), call
     `history.changes_since(&cursor)`, append its `changes()` as records, and
     only then advance the cursor to the response's `generation()`. Agent
     commits, alterations and compaction plans all arrive through the same
     feed, so nothing reports edits separately and there is no shape diffing.
   - `HistoryError::ChangesTooOld` (more than 1024 events since the cursor):
     fall back to a full rewrite with an epoch bump, using the existing
     mechanism, then resume from the new snapshot's cursor.
   - Concurrency: `changes_since` and `snapshot` take a short read lock and
     return owned data. Never hold a history lock across file I/O.
4. **Tool-result revisions.** Don't prune. Before `StepFinished` the agent
   squashes each tool-result entry to its placeholder source plus one final
   revision, and emits `EntryReplaced`. A mid-run checkpoint may already have
   written progress revisions; the later `replace` record supersedes them.
   - A run dropped before squashing leaves the source plus the progress
     revisions committed so far, with explicit placeholders for unfinished
     calls. Persist that as is.
   - Alteration and compaction revisions are never squashed and keep every
     revision.
   - Test a run of 10 parallel tool calls with large outputs: after reload the
     result entry holds source + 1 revision, and the session file's live
     records hold each output once (the superseded progress records are
     dropped at the next compaction rewrite).
5. **Loading.** One loader reads every previous shape:
   - legacy `.json`;
   - v-old JSONL with craft-legacy messages;
   - 04's `StoredMessage` records (each becomes a source-only active entry);
   - the new entry records.

   Saving always writes the new shape. Resume replays `history.current()` into
   the TUI scrollback, with inactive entries hidden and summaries rendered as
   compaction cards.
6. **Continue, fork and undo.**
   - Fork copies the snapshot's entries (hardening/09).
   - `/undo` keeps its snapshot-manager commit points: a commit after every
     non-failed run, as today.
   - Decide whether `/undo` should also restore deactivated history entries;
     it is now possible. Default: no behaviour change. Note it as a follow-up.
7. **Surface specifics.**
   - **TUI**: usage recorder from `RunReport::stats` (via `Event::Done`); the
     superseded-attempt rendering from 08; markers styled by tag.
   - **ACP**: `session/load` replays `current()`; cancel maps to
     `Cancellation`; permission and elicitation flows are unchanged (07).
   - **Headless/print**: JSON output shapes stay stable for SDK consumers
     (hardening/18). If a field must change, version it.
8. **Delete the facade.** Remove `run::run`, `run_with_images`, `RunOutcome`
   plumbing that nothing uses, and the `Vec<Message>` history parameters from
   every caller. Move the remaining `run/` pure-logic modules under
   `harness/`, or leave them in `run/`. Choose one layout and make it coherent.

## Tests

- Session round trips for every historical format (04's fixtures plus new
  entry-format fixtures), with compaction (inactive entries plus summary) and
  revisions.
- Incremental save writes only the delta. Assert the appended byte count stays
  small after a one-message run on a long session.
- Feed replay: for runs with appends, a compaction (inserts plus
  deactivations), an alteration pass, and a tool step with mid-run
  checkpoints, replaying the records equals `snapshot().entries()`.
- `ChangesTooOld` falls back to a full rewrite and resumes incrementally.
- Squash: the 10-parallel-call run reloads with one revision per result entry,
  and reload equals the in-memory `current()`.
- Crash safety: a corrupt trailing `revise` record is skipped with a warning,
  and the entry keeps its previous revision.
- TUI resume, ACP `session/load` and headless `--continue` against fixtures.
- Model switch mid-session keeps history and works with the new provider.
- `Conversation::shutdown` runs on every exit path. Use a counting layer.

## Acceptance

- `grep -rn 'run::run\b\|run_with_images' agent/src` is empty.
- Every surface holds a `Conversation`.
- `cargo test -p craft` is green, and the scripted end-to-end smoke tests for
  TUI (model-level), ACP and headless pass.
- **Manual QA** (no paid calls unless you approve them): a local llama.cpp or
  oMLX server through `openai-compatible`/`llama-cpp`, one TUI session with
  tool calls, cancel, resume and compaction.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note: the format version, the session-size
   measurements, the undo follow-up, and the manual QA results.

## Completion note

_(fill in when done)_
