# Sessions

Craft saves conversation history so you can resume work or branch from an earlier session.

## Resume

In the TUI, use `/sessions` to find saved work. From the command line:

```sh
craft --continue
craft --session <ID>
```

`--continue` resumes the latest session; `--session` resumes the specified ID. ACP clients can resume with `session/load`.

## Fork a session

Add `--fork-session` to work from a copy without changing the source session:

```sh
craft --continue --fork-session
```

`--session-id <ID>` names a **new** session; it never resumes an existing one.

## Saved format and compatibility

Messages persist as animist **entry records** (log format 4): `entry`, `insert`, `revise`, `deactivate`, `replace`, mapping one to one onto the history change feed — plus `sub_entry` subagent histories and the trailing meta record.

One loader reads every older shape (pre-animist message records, `.json` files) and folds their raw `tool_outputs` copies into entry sources; saving rewrites the current shape. A corrupt or truncated trailing record is skipped with a warning; a record whose index is out of range for the replayed list **fails the load** with an actionable error.

**Save compatibility:** older Craft builds cannot read the current save format. Keep a backup of existing session files before opening them with a newer build if you may need to return to the older build.

## What the transcript restores

The TUI, command-line resume, and ACP use the same transcript loader:

- inactive (compacted-away) entries are hidden;
- compaction summaries render as cards;
- markers render by tag — altered tool results show their altered text with the verbatim original one click away (see [Context Management](./context.md)).

## Mode persistence

The TUI persists its mode (build/plan) in the session file on every user-initiated change (Tab toggle, implementing a plan). A resumed session restores its stored mode — the stored mode wins over `--mode`/`--permission-mode plan`, which seed fresh sessions and legacy sessions with no stored mode. The stored model is restored too (unless `-m` overrides it).

Print-mode resume (`-c`/`-s`) restores history and the stored model, but not the stored plan/build mode or thinking config — those are TUI/ACP surfaces.

## Stats

`craft stats` reports the usage ledger; `--sessions` breaks it down per session. In the TUI, `/stats` and `/usage` show the same data live.
