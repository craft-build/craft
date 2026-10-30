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

_(fill in when done)_
