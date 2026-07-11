# Wiki

The wiki is a local knowledge base for your project. It lives in a `.wiki/` folder at the project root, so it travels with the repo. Commit it, share it, and edit it by hand.

Use it to hold decisions, glossaries, design notes, and digested copies of reference documents. The agent can read and extend it through tools, and you can browse it from the command line.

## Layout

```text
.wiki/
├── pages/              # hand-authored or tool-appended markdown pages
│   └── <slug>.md
├── ingested-sources/   # structured notes from ingested files
│   └── <slug>.md
├── log.md              # append-only dated log of ingest events
└── index.md            # generated: links every page and source by title
```

Everything is plain markdown. It diffs cleanly and stays readable in any text editor.

- **Pages** are free-form notes. Create them by hand or let the agent append to them.
- **Ingested sources** are notes made from a local file. Each one has a header with the source path, an ingest timestamp, a short LLM summary, and an excerpt.

Slugs are lowercase, digits, and dashes only, like `design-decisions` or `api-glossary`.

## Ingest a file

Turn a local file into a wiki source note with an LLM summary:

```bash
craft wiki ingest ./docs/architecture.md
```

Craft reads the file, asks the model for a two-sentence summary, and writes a structured note under `.wiki/ingested-sources/`. It also appends a dated line to `.wiki/log.md` and regenerates `.wiki/index.md`.

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

Print a page or source note by its slug:

```bash
craft wiki show design-decisions
```

## From the TUI

You can also work with the wiki without leaving the interactive session. Type `/wiki` in the command palette:

- `/wiki ingest <file>` ingests a file with an LLM summary, just like the CLI command. It runs in the background and flashes the result when done.
- `/wiki list` flashes the current pages and sources.
- `/wiki show <slug>` flashes the body of a page or source note.

The TUI commands use the same `.wiki/` folder as the CLI, so anything you do in one is visible in the other.

## Agent tools

The agent has two tools for working with the wiki. Both use the `.wiki/` folder in the current project.

- **`wiki_read`**: read a page or source note by slug.
- **`wiki_append`**: append markdown to a page, creating it if it does not exist, then refresh the index.

This lets the agent capture durable knowledge during a session, like a decision it helped you reach or a glossary entry for a term you clarified. Because the wiki is plain files, anything the agent writes is easy to review, edit, or revert in a normal commit.

## How it differs from memory

Craft also has a `memory` plugin for project knowledge. They serve different jobs:

- **Wiki** (`.wiki/`) is in-project, committable, and meant to be read by humans. It is organized by slug and title.
- **Memory** lives in your state directory, is curated and bulk-loaded into the prompt, and has semantic search.

Reach for the wiki when the knowledge should live alongside the code and be shared with your team.
