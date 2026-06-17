Move/rename a file or directory and update import references across the project.

- Source file contents are auto-backed up before the move (use `safety undo` to recover).
- After moving, scans the project for import statements referencing the old path and updates them.
- Import update is text-based (string replacement of module path). Works for Rust `use` paths, TypeScript `import` paths, and similar.
- Parent directories of the destination are created if needed.
