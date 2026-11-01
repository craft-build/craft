# 11 — Sessions: one loader, resume, fork, undo, and the module layout

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt)
  and the [harness design rules](harness-design.md). Start with the
  [test inventory](harness-design.md#test-integrity).
- When implementation is finished, **run the review agent**
  (`.craft/agents/reviewer.md`) as described in [Finish](#finish).

## Goal

05 already gave every surface a long-lived `Conversation` and a change-feed
persister. This prompt finishes the session story:

- one loader for every historical shape, including folding old `tool_outputs`
  records into entry sources;
- resume, continue, fork and `/undo` over `HistoryEntry`s;
- crash safety;
- a coherent module layout once the migration's churn is done.

After this prompt the migration branch can merge.

## Read first

- Craft:
  - `agent/src/storage/sessions/` (the loader, the persister from 05, epoch
    rewrites)
  - `agent/src/tui/provider/live/resume.rs`, `agent/src/acp/` (`session/load`),
    `agent/src/headless.rs` (`--continue`)
  - `agent/src/snapshot.rs` (`/undo` commit points)
  - `docs/hardening/09-continue-and-fork-semantics.md`,
    `docs/hardening/14-honest-and-complete-undo.md`,
    `docs/hardening/18-*` (headless JSON),
    `docs/hardening/28-session-model-index-fallback.md`
- animist 0.7.0: `docs/history.md` → *Entries and views*, *Agent tool-result
  revision retention*; `History::from_entries`.

## Work

1. **One loader.** It reads every previous shape:
   - legacy `.json`;
   - pre-04 JSONL with craft-legacy messages;
   - 04's entry records, plus any change records written since 05.

   Old `tool_outputs` records are folded into the matching tool-result
   entry's source if that entry has no richer original; otherwise they're
   dropped. Saving always writes the current shape. Subagent histories load
   through the same path.
2. **Crash safety.** A corrupt or truncated trailing record is skipped with a
   warning, and the entry keeps its previous state. A record whose index is
   out of range for the replayed list fails the load with an actionable
   error. Don't skip it silently.
3. **Resume rendering.** TUI resume, ACP `session/load` and headless
   `--continue` replay `history.current()`:
   - inactive entries are hidden;
   - compaction summaries (`craft.compaction` tag) render as compaction
     cards;
   - markers render by `craft.marker` tag;
   - altered results (`craft.alter`) show their altered text, with "show
     full output" reading `verbatim()`.

   One function builds the replay list, and every surface uses it.
4. **Continue, fork and undo.**
   - Fork copies the snapshot's entries (hardening/09).
   - `/undo` keeps its snapshot-manager commit points: a commit after every
     non-failed run, as today.
   - Decide whether `/undo` should also restore deactivated history entries;
     it is now possible. Default: no behaviour change. Note it as a follow-up.
5. **Module layout.** Move the `run/` pure-logic modules that layers use
   (doom, guardrails, dedup, recency, read_lifecycle, view, retry
   classification) next to the layer that owns them under `harness/`, or
   leave all of them in `run/`. Pick one layout, make it coherent, and
   delete `run/` if it's empty. A module that only re-exports is deleted.
6. **Leftover indirection.** Go through the
   [inventory](harness-design.md#no-indirection-without-a-job). Close every
   row that is still open, or record why it stays.

## Tests

- Session round trips for every historical format (04's fixtures plus
  entry-format fixtures with compaction, alterations and revisions).
- Old `tool_outputs` records fold into sources, and `retrieve` returns them
  after reload.
- Squash: a run of 10 parallel tool calls with large outputs reloads with
  one revision per result entry, and reload equals the in-memory
  `current()`.
- Crash safety: a corrupt trailing `revise` is skipped; an out-of-range
  index fails loudly.
- TUI resume, ACP `session/load` and headless `--continue` against
  fixtures, all through the shared replay function.
- Fork copies entries, including inactive ones, and the fork's persister
  starts from a fresh cursor.

## Acceptance

- One loader module handles every session shape.
- Every surface uses the same resume replay function.
- `cargo test -p craft` is green, and the scripted end-to-end smoke tests for
  TUI (model level), ACP and headless pass.
- **Manual QA** (no paid calls unless you approve them): a local llama.cpp or
  oMLX server through `openai-compatible`/`llama-cpp`, one TUI session with
  tool calls, cancel, resume and compaction.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note:
   - the session-size measurements;
   - the undo follow-up;
   - the manual QA results;
   - the final module layout;
   - the test-inventory diff;
   - the inventory rows closed or kept, with reasons.

## Completion note

**Done 2026-11-01.** Working tree on `animist-migration` (one commit series
with this note). Validation: `cargo fmt -p craft --check` clean; clippy
`--all-targets` **115** warnings, the same set as the pre-11 tree (zero in
any file this prompt touched); `cargo test -p craft` **2013 passed, 3
failed, 1 ignored** — the failures are the three recipe-render fixed
points, the ignored one is the argosy env gate (the 01 baseline).

### Loader (one loader, every shape)

- Old `out` records (jsonl v3, legacy `ToolResult` shape) and the legacy
  `.json` `tool_outputs` map fold into the matching tool-result entry's
  **source** when the entry holds no richer copy (richness = text chars +
  image blocks, the raw copy wins ties only strictly); otherwise they are
  dropped, as are copies whose call left the history. The fold runs against
  the main list and every subagent history (the map was keyed by call id
  across both). *Pins:* `out_records_fold_into_entry_sources`,
  `legacy_json_session_loads_converted` (extended),
  `folded_raw_outputs_serve_retrieve_after_reload` (through `ToolHarness`:
  the compression marker's hash resolves to the folded raw text after
  reload). `records.rs`' two "until 11" comments are gone.
- **Behaviour change:** a record whose index is out of range for the
  replayed list now **fails the load** with
  `SessionError::RecordOutOfRange { path, line, index, len }` (an
  actionable message naming the archive/restore options) instead of being
  skipped with a warning. Unparseable records keep the skip-with-warning
  policy wherever they sit. *Pins:* `an_out_of_range_index_fails_the_load`,
  `a_corrupt_trailing_revise_is_skipped` (the entry keeps its previous
  state), records.rs `out_of_range_records_are_rejected` (comment re-pinned
  to the new contract).

### Replay (one function, every surface)

`harness::replay::transcript(&[HistoryEntry]) -> Vec<ReplayItem>` builds
the replay list from `current()` semantics: inactive entries hidden,
markers by `craft.marker`, compaction summaries by `craft.compaction`,
altered results showing their altered text with the verbatim original one
field away. *Pins:* the five `harness::replay::tests`.

- **TUI resume** maps it onto an extended `LoadedMessage`
  (`Tool`/`Compaction`/`Marker` variants): tool results rebuild as tool
  cards (the verbatim original behind the card's new "click to show full
  output" affordance, `Message::Tool.full`), summaries render as
  compaction cards (`Message::Compaction`), markers as notice lines. The
  old hand-rolled `transcript()` (which showed inactive entries and
  dropped markers/summaries/results) is deleted. *Pins:*
  `session_load_rebuilds_replayed_tool_markers_and_compaction`,
  `loaded_messages_replay_the_current_view`.
- **ACP `session/load`** now replays the transcript to the client as
  session updates **before** the response (the protocol's documented
  resume contract): user/agent text as chunks, results as completed tool
  calls with content; markers and summaries have no live ACP translation
  either and replay as nothing. *Pin:*
  `acp::tests::replay_updates_carry_the_shared_transcript`.
- **Headless `--continue`** keeps today's stdout contract (nothing prints
  prior history): its resume goes through the same loader/persister
  restore, so the replay rules hold by construction — pinned at the
  storage level by the round-trip and squash tests below rather than by a
  display test.

### Continue, fork, undo

- Fork copies the snapshot's entries whole — inactive entries and
  revisions included (hardening/09) — and the fork's persister restores
  from a fresh feed cursor whose first checkpoint writes nothing. *Pin:*
  `cli::tests::fork_copies_inactive_entries_and_restarts_the_persister`.
- `/undo` is unchanged: the snapshot layer still commits after every
  non-failed run (`SnapshotLayer::turn_end`). **Follow-up:** `/undo` could
  now also restore deactivated history entries (they survive in sources);
  deliberately not changed — today's contract is file-contents-only
  (hardening/14), and mixing history rollback into it is a product
  decision.
- hardening/28 closed: `latest_model_in`/`latest_thinking_in` treat an
  indexed-but-unreadable newest header as stale and fall back to the scan
  instead of suppressing the restore. *Pin:*
  `latest_model_and_thinking_treat_a_malformed_indexed_header_as_stale`.

### Squash reload and session-size measurements

`parallel_tool_calls_squash_and_reload_to_the_same_current_view`: ten
parallel `read`s of ~19 KB files through a persisted conversation leave
one placeholder-backed result entry with ten result blocks and **exactly
one revision** (animist F04's squash is what the change feed writes), and
the reload's `current()` equals the in-memory one. Session sizes: that
fixture's log is **474 KB** for ~194 KB of raw output (source placeholder
+ final revision + JSON escaping); the golden entry-format fixtures are
485–1059 B each (`fixtures/*.jsonl`, 6096 B total). Folding `out` copies
into sources never grows a file — the raw bytes move into the entry source
once and the duplicate copy (or the compressed in-message text) drops out
of the rewrite.

### Module layout (final)

`run/` keeps only the seams surfaces and the harness share — `advisor`
(driver-driven), `approval` (the policy layer's responder seam),
`cancel` (bang-mode and subagent tokens), `mode` (the build/plan taxonomy
surfaces carry), plus `BoxFuture`. The pure-logic modules moved next to
the layers that own them, under `harness/`: `doom.rs`, `guardrails.rs`,
`dedup.rs` (guardrail family; tools reach them for write-path claims),
`read_lifecycle.rs`, `view.rs` (request family), `retry.rs` (recovery
family). `clamped_max_tokens`/`MIN_OUTPUT_TOKENS` moved to
`harness::request`; `SharedCompactionState` moved to `compaction`. No
module only re-exports.

### Indirection rows closed or kept

- **Closed:** `RecencySource` — **deleted** with `run/recency.rs`, the
  builder `.recency()` and the request-layer field. It had **zero**
  production implementations at the 01 baseline ("`None` on every surface
  today", its own doc said) and none since; the rule keeps a trait only
  with two real implementations. Its 7 tests died with the seam.
- **Closed:** `headless::SessionStore`/`StoredSession` re-export shims —
  every consumer now imports `storage::sessions` directly.
- **Closed:** dead code the move exposed in `dedup.rs`
  (`cached_result` + `CACHED_PREFIX` — the pre-05 `[cached]` replay
  prefix concept — plus the test-only `new`/`len`/`normalize_write_path`
  now `#[cfg(test)]`) and `view::assert_replay_valid` (`#[cfg(test)]`,
  its only callers are session tests).
- **Kept:** `CancelMap`/`CancelToken` in `run/cancel` —
  `Command::CancelSubagent` still groups per-subagent tokens through the
  map (TUI), and bang-mode shell runs use the watch tokens.
- **Kept:** `BoxFuture` in `run` — the approval seam's return type,
  implemented by three surfaces.
- Confirmed already closed: one `ResolvedModel` (`model_selection`),
  `SharedDispatch`/`SharedCompressionStore`/`tool_outputs` gone (grep
  clean), `block_in_place|block_on|new_current_thread` zero hits in
  `agent/src` (stricter than the audit's "main.rs only" — `main.rs` is
  plain `#[tokio::main]`).

### Test-inventory diff

2009 → **2017** listed. Real removals (9, every one mapped):
`run::recency::tests::*` (5) + the two recency-tail harness/run_tests
tests — deleted with the dead seam;
`run::dedup::tests::cached_result_marks_and_rekeys` — deleted with the
dead helper; `tui::provider::live::tests::transcript_skips_tool_blocks` →
`loaded_messages_replay_the_current_view` (the new replay carries tool
results instead of skipping them; system entries now drop instead of
masquerading as assistant text). Everything else in the diff is the
module-move rename (`run::{doom,guardrails,dedup,read_lifecycle,view,retry}`
→ `harness::…`). Real additions (17): the loader suite (the fold pin —
which also pins the equal-richness tie — retrieve-after-reload,
out-of-range, corrupt-trailing-revise, the entry-format round trip, and
`apply_changes`' unapplied count), the replay suite (5, including the
call-name fallback), the TUI pair, the ACP replay pin, the fork pin, the
squash pin, and the H.28 fallback pin.

### Review

Review agent over the full diff: **approve_with_nits** (P2×1, P3×4). All
five fixed and pinned: the P2 was a write/read asymmetry 11 itself created
— `apply_changes` silently skipped records the loader would now refuse, so
the persister recovers with a full rewrite and warns instead of appending
a poison record (`apply_changes_reports_records_it_could_not_apply`); the
P3s were the fold's per-entry clone cost (now locate-first, clone-once),
a missing equal-richness tie pin, replayed tool cards losing their tool
kind/name and error text (names now resolve through the matching call —
the `ext::tool_name` convention — and the TUI derives the card kind with
`tool_head`), and a duplicated assert a module move exposed in
`dedup.rs`. A verification pass over the fixes: **approve** — all five
confirmed. Its three residual P3s: the fold's first-list-wins on a
cross-agent call-id collision (doc sentence added; the pre-08 keying could
not disambiguate one anyway), the persister's recovery branch having no
direct test (justified skip: triggering it needs the live history's own
feed to emit an out-of-range index, which `History`'s public API cannot be
made to do — the pin covers the contract the branch reads and
`full_rewrite` is exercised by the stale-cursor test), and the note's
counts, refreshed above.

**Pre-existing flake (not 11's):** `subagent::tests::worktree_children_*`
(10's isolation pins) fail intermittently under parallelism with a git
`worktree add` `index.lock` race; measured at 5/12 failures on a pristine
checkout of HEAD (292149fb) in a separate git worktree — worse than under
this diff. Left for 12 to serialize or otherwise fix; "green" above means
green modulo that known flake and the three recipe fixed points.

### Manual QA

**Not performed.** The local oMLX server on 127.0.0.1:8000 is up (6
models) but requires an API key this run does not have; no other keyless
local server (llama.cpp/ollama) is running. Everything scriptable ran
offline: the full suite includes the scripted surface suites (TUI at the
model level, ACP, headless). One interactive TUI session with tool calls,
cancel, resume and compaction against a local model remains to be run
before merge — provide an oMLX key or a keyless llama.cpp/ollama endpoint
and it is a five-minute check.
