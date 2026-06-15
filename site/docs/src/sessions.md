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

## What Is Saved

- The full message history and token usage
- Tool outputs and sub-agent messages
- Session mode (build/plan), plan, goal, thinking, and fast-mode flags
- Permission rules you set during the session
- Queued messages

Loading a session restores all of the above, so permissions you granted last time are remembered.

## Headless Sessions

Sessions work the same in `--print` mode. The JSON output from `craft -p --output-format json` includes a `session_id` you can pass back to `--session` to continue a headless run. See [Headless Mode](./headless.md).
