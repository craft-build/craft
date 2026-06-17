Return a structural outline of a file or directory.

For a file: returns a nested symbol tree with signatures, line ranges, and export status.
For a directory: returns per-file symbol trees with compact entries.
With `files=true` on a directory: returns a flat table of files with language, symbol count, and byte size.

- Supported languages: Rust, TypeScript/JavaScript, Python, Go, Java, C, C++, Ruby, Lua, Bash, Kotlin, Swift, C#, Elixir, Scala, PHP, HTML, Gleam, Dart, Starlark/Bazel, Nix, Zig, Markdown.
- Unsupported files are reported as skipped.
- Output is capped at 30KB with narrowing hints on truncation.
- Prefer this over `read` for getting an overview of a file's structure.
