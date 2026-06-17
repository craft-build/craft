Create and restore file-system checkpoints, undo file edits, and view backup history.

- `checkpoint` with a `name`: snapshot all project files in memory. Use before risky edits.
- `restore` with a `name`: roll back all files to the checkpoint state. Reverts everything.
- `list`: show all saved checkpoints and backup summary.
- `undo` with a `path`: restore a file to its state before the last edit. Each call pops one backup.
- `history` with a `path`: show the backup stack for a file (most recent last).
- Checkpoints are in-memory only (lost on process exit).
- Auto-backups are created before every write, edit, multiedit, or apply_patch mutation.
- Prefer `undo` for single-file mistakes, `checkpoint`/`restore` for broad changes.
