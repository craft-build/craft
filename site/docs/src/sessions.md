# Sessions

Sessions are crash-safe append-only JSONL files with an index, archive, and usage ledger (`crafty stats`, `/stats`).

## Format

Messages persist as animist **entry records** (log format 4): `entry`, `insert`, `revise`, `deactivate`, `replace`, mapping one to one onto the history change feed — plus `sub_entry` subagent histories and the trailing meta record.

One loader reads every older shape (pre-animist message records, `.json` files) and folds their raw `tool_outputs` copies into entry sources; saving rewrites the current shape. A corrupt or truncated trailing record is skipped with a warning; a record whose index is out of range for the replayed list **fails the load** with an actionable error.

> **One-way upgrade:** older `crafty` builds cannot read the new saves — upgrade before rolling back.

## Resume

`/sessions` (TUI), `-c` / `--continue` (latest), `-s/--session <id>` (specific), and ACP `session/load` all replay the same shared transcript builder:

- inactive (compacted-away) entries are hidden;
- compaction summaries render as cards;
- markers render by tag — altered tool results show their altered text with the verbatim original one click away (see [Context Management](./context.md)).

`--fork-session` copies a `--session`/`--continue` source under a new id without touching the source. `--session-id` sets the persistence id for a **new** session; it never resumes.

## Mode persistence

The TUI persists its mode (build/plan) in the session file on every user-initiated change (Tab toggle, implementing a plan). A resumed session restores its stored mode — the stored mode wins over `--mode`/`--permission-mode plan`, which seed fresh sessions and legacy sessions with no stored mode. The stored model is restored too (unless `-m` overrides it).

Print-mode resume (`-c`/`-s`) restores history and the stored model, but not the stored plan/build mode or thinking config — those are TUI/ACP surfaces.

## Stats

`crafty stats` reports the usage ledger; `--sessions` breaks it down per session. In the TUI, `/stats` and `/usage` show the same data live.
