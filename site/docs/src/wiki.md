# Wiki

The wiki is a local knowledge base for your project. It lives in a `.wiki/` folder at the project root, so it travels with the repo. Commit it, share it, and edit it by hand.

Use it to hold decisions, glossaries, design notes, and digested copies of reference documents. The agent can read and extend it through tools, and you can browse it from the command line.

The wiki conforms to the [Open Knowledge Format (OKF) v0.1](https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md), an open spec for representing knowledge as a directory of markdown files with YAML frontmatter. A Craft wiki is an OKF bundle, so any OKF-compatible tool can read it without translation.

## Layout

```text
.wiki/
├── index.md             # generated OKF directory listing (bundle root)
├── log.md               # OKF update log, date-grouped, newest first
├── pages/               # hand-authored or tool-appended concept documents
│   └── <slug>.md
└── ingested-sources/    # concept documents derived from a source file
    └── <slug>.md
```

Everything is plain markdown with YAML frontmatter. It diffs cleanly and stays readable in any text editor.

- **Pages** are free-form concept documents. Create them by hand or let the agent append to them. Each carries an OKF `type` field. Craft defaults new pages to `type: Note`, but the `wiki_append` tool accepts an optional `kind` so the agent can mark a page as a `Reference`, a `Decision`, or any other OKF type.
- **Ingested sources** are concept documents made from a local file. They carry `type: Reference`, a `description` with a short LLM summary, a `resource` pointing at the source path, and a `timestamp`. The body holds an excerpt and the full verbatim source.

Slugs are lowercase, digits, and dashes only, like `design-decisions` or `api-glossary`.

## Initialize the wiki

Bootstrap a starter wiki for a new project, or refresh one that already exists, with a single command:

```bash
craft wiki init
```

Craft spawns an agent that researches the codebase, fans out parallel research subagents, and writes five starter pages:

- `tech-stack`: languages, runtimes, key dependencies, layout, entry points
- `architecture`: module map, data flow, subsystem boundaries
- `design-decisions`: coding conventions, patterns, rationale
- `glossary`: domain terms and jargon
- `project-context`: a short overview linking the other four

Each page gets OKF frontmatter, the index regenerates after every write, and a dated entry lands in `log.md`.

Pick a different model for the run with `-m`:

```bash
craft wiki init -m anthropic/claude-sonnet-4-6
```

`craft wiki init` is idempotent. If the wiki already has pages, the agent reads each target page first and appends new findings under a dated heading instead of overwriting your hand-written content. Run it again whenever the project changes shape and you want the wiki to catch up.

## Frontmatter

Every concept document starts with a YAML block delimited by `---`. OKF requires a single field, `type`, and Craft fills in the rest when it generates a document:

```yaml
---
type: Reference
title: Orders architecture
description: Two-sentence LLM summary of the source file.
resource: ./docs/architecture.md
tags: [backend, data]
timestamp: 2026-05-28T14:30:00Z
---
```

You can add any extra keys. Craft preserves them across writes, and they do not break anything, per the OKF permissive consumption model.

## Ingest a file

Turn a local file into a wiki concept document with an LLM summary:

```bash
craft wiki ingest ./docs/architecture.md
```

Craft reads the file, asks the model for a two-sentence summary, and writes an OKF concept document under `.wiki/ingested-sources/` with the full source body preserved. It also appends a dated entry to `.wiki/log.md` and regenerates `.wiki/index.md`.

Pick a different model for the summary with `-m`:

```bash
craft wiki ingest ./docs/architecture.md -m anthropic/claude-sonnet-4-6
```

## List entries

Print every page and source with its title:

```bash
craft wiki list
```

## Show an entry

Print a page or source concept document by its slug:

```bash
craft wiki show design-decisions
```

## From the TUI

You can also work with the wiki without leaving the interactive session. Type `/wiki` in the command palette:

- `/wiki ingest <file>` ingests a file with an LLM summary, just like the CLI command. It runs in the background and flashes the result when done.
- `/wiki init` starts an agent turn that researches the project and writes the starter pages, streaming progress as it goes.
- `/wiki list` flashes the current pages and sources.
- `/wiki show <slug>` flashes the body of a page or source document.

The TUI commands use the same `.wiki/` folder as the CLI, so anything you do in one is visible in the other.

## Agent tools

The agent has two tools for working with the wiki. Both use the `.wiki/` folder in the current project.

- **`wiki_read`**: read a page or source document by slug. Returns the markdown body.
- **`wiki_append`**: append markdown to a page, creating it if it does not exist (with OKF frontmatter), then refresh the index. Optional `kind`, `description`, and `tags` arguments populate the page's OKF frontmatter: `kind` sets the `type` for a new page (default `Note`), and `description`/`tags` fill in a missing field on an existing page without overwriting what is already there.

This lets the agent capture durable knowledge during a session, like a decision it helped you reach or a glossary entry for a term you clarified. Because the wiki is plain files, anything the agent writes is easy to review, edit, or revert in a normal commit.

## How it differs from memory

Craft also has a `memory` plugin for project knowledge. They serve different jobs:

- **Wiki** (`.wiki/`) is in-project, committable, OKF-conformant, and meant to be read by humans. It is organized by slug and title.
- **Memory** lives in your state directory, is curated and bulk-loaded into the prompt, and has semantic search.

Reach for the wiki when the knowledge should live alongside the code and be shared with your team.
