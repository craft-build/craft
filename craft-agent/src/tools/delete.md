Delete files or directories. Text file contents are auto-backed up (use `safety undo` to recover).

- Set `recursive=true` for non-empty directories.
- Non-existent paths are skipped and reported.
- Special files (symlinks, devices) are skipped.
- Directories are removed with all contents (when recursive=true).
