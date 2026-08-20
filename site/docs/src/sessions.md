# Sessions

Craft saves every session automatically, so you can pick up exactly where you left off: full conversation, tool outputs, sub-agent chats, permissions, and mode.

## Storage

Sessions are stored as append-only JSONL files:

```
~/.local/state/craft/sessions/<id>.jsonl
```

Each file starts with a header record (version, id, cwd, title), followed by message, tool-output, and metadata records. The `~/.craft/` directory is checked as a legacy fallback. Run `craft migrate xdg` to move legacy data into the XDG layout.

## Resuming

| Flag | Effect |
|------|--------|
| `-c`, `--continue` | Resume the most recent session in the current directory |
| `-s`, `--session` *(alias `--resume`)* `<id>` | Resume a specific session by ID |
| `--fork-session` | Resume a session under a new ID, leaving the original untouched |

Inside the TUI, `/sessions` opens a picker that lists sessions for the current directory with relative timestamps. Pick one to switch, or delete one with Ctrl+D.

## Checkpoints

Run `/checkpoint` to write a clean checkpoint of the current session. This makes the next resume faster and safer, since compaction and intermediate state are flushed to disk.

## Archived Logs

Compaction replaces the older turns in the session's on-disk log with the summary. The dropped turns are not lost: before the rewrite, craft parks the previous log at `sessions/archive/<session-id>/<n>.jsonl` in the [state directory](./configuration.md#directory-layout). It keeps the newest three per session, and at most 32 MB of them. The names count up, so the highest number is the newest.

An archive is a complete session file, so `jq` or an editor reads it as it is. To open one in craft you have to put it back in place of the live log, which drops the session's current state, so move that out of the way first:

```sh
cd ~/.local/state/craft/sessions
mv <session-id>.jsonl <session-id>.jsonl.bak
cp archive/<session-id>/<n>.jsonl <session-id>.jsonl
craft -s <session-id>
```

`CRAFT_DISABLE_AUTOCOMPACT=1` turns off the automatic compaction. A manual `/compact` still compacts.

## What Is Saved

- The full message history and token usage
- Tool outputs and sub-agent messages
- Session mode (build/plan), plan, goal, thinking, and fast-mode flags
- Permission rules you set during the session
- Queued messages

Loading a session restores all of the above, so permissions you granted last time are remembered.

## Headless Sessions

Sessions work the same in `--print` mode. The JSON output from `craft -p --output-format json` includes a `session_id` you can pass back to `--session` to continue a headless run. See [Headless Mode](./headless.md).
