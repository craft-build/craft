# Scan: Project Documentation

Investigate the project and capture what you learn as durable project knowledge, so future sessions start oriented instead of re-reading the whole tree. This is a documentation pass — do not change any project files.

## Steps

1. See what already exists: call the `memory` tool with `view` (no path) and note any `docs/` entries. Also check the wiki with `wiki_read` on `*` if the project has one. An existing document is updated in place, never duplicated.
2. Investigate the project with the `read`, `grep`, and `glob` tools; start from the README and package manifests (`Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`, ...), then use `outline` and `zoom` for the module layout and `inspect` for a quick health picture. Prioritize: README and docs, package manifests, entry points and module layout, build/CI config (`Makefile`, `.github/workflows`, `justfile`, ...), and tests.
3. Write the core set with the `memory` tool (`write` command), prefixing each entry with `docs/`:
   - `docs/summary` — what the project is, what it does, and for whom; the one-page orientation.
   - `docs/architecture` — the major components, how they connect, where the code lives (real paths), and the flow of a typical request or run.
   - `docs/tech` — languages, frameworks, runtime, and key dependencies with versions taken from the manifests, not from memory.
   - `docs/development` — how to build, test, lint, and run: the actual commands, taken from CI, Makefile, or docs.
4. Add further entries only when the project clearly warrants them — e.g. `docs/decisions/<slug>` (one dated entry per major architectural decision), `docs/glossary` (domain terms), `docs/conventions` (patterns the codebase consistently follows). Skip anything that would be padding.
5. If step 1 found an entry the project has outgrown, delete it with the `memory` tool (`delete` command).
6. Report a one-paragraph summary of what you wrote, updated, skipped, and deleted.

## Rules

- Ground every claim in something you actually read: real paths, real commands, versions from the manifests. If you are not sure, say so or leave it out.
- Write for a newcomer session that knows nothing about this project: lead with what matters, keep each entry tight, prefer tables of facts over prose.
- Distill, don't copy: point at canonical files (README, docs) for the long version instead of transcribing them.
- Re-runs are updates: reuse the same names so `memory write` overwrites, never create near-duplicates under new names.
- Record no secrets or credentials.
- Do not modify the project itself — this pass writes only to the memory store.
