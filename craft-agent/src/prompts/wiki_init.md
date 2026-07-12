# Wiki init: project onboarding

Research the current project and produce a structured starter wiki (`.wiki/`) that captures the tech stack, architecture, design decisions, glossary, and a short project overview. The wiki is plain markdown with OKF frontmatter, committed to the repo and shared with the team.

## Step 1: Detect existing wiki state

Call `wiki_read` on each of the five target slugs below, or read `.wiki/index.md`, to see what already exists.

- If a page exists, treat this run as an **update**: you must preserve its existing content. Read the page first, then append new findings under a `## Updated YYYY-MM-DD` heading using `wiki_append`. Never delete or rewrite existing user content.
- If a page does not exist, create it from scratch with `wiki_append`.

## Step 2: Fan out parallel research

Spawn exactly four parallel research subagents via the `task` tool (`subagent_type: "research"`, `model_tier: "weak"`). Launch all four in one `batch` call so they run concurrently. Each agent returns its findings as prose; it does not write files. Give each agent one of these focused briefs:

1. **Stack and structure** - languages, frameworks, runtimes, key dependencies, package/crate layout, entry points. Read dependency manifests (`Cargo.toml`, `package.json`, `go.mod`, etc.) and the top-level directory layout.
2. **Architecture** - main modules and crates, data flow, cross-cutting concerns, integration boundaries between subsystems. Read `AGENTS.md`, top-level source dirs, and module-level docs.
3. **Conventions and decisions** - coding style, error handling, testing patterns, logging, notable design decisions. Pull from `AGENTS.md`, `justfile`, `CONTRIBUTING.md`, linter configs, and inline comments.
4. **Glossary** - domain terms, project-specific jargon, acronyms. Scan docs, READMEs, and source identifiers.

Merge and deduplicate their findings before writing. If a subagent returns nothing useful for a page, write that page from your own observations rather than skipping it.

## Step 3: Write five wiki pages

Use `wiki_append` for each page. The `page` argument is the slug; `body` is the markdown body starting with an H1 title. Pass the `kind`, `description`, and `tags` arguments so the page's OKF frontmatter carries useful metadata for search and the index listing. Each call creates OKF frontmatter and bumps the timestamp automatically.

| Slug | OKF `type` (`kind`) | Contents |
|---|---|---|
| `tech-stack` | `Reference` | Languages, runtimes, key deps, package layout, entry points |
| `architecture` | `Reference` | Module map, data flow, subsystem boundaries |
| `design-decisions` | `Decision` | Conventions, patterns, rationale |
| `glossary` | `Note` | Term to definition list |
| `project-context` | `Note` | One-paragraph overview linking the other four pages |

For every page, set `description` to one concise sentence summarizing the page (it appears in the index listing and powers search), and set `tags` to a short list of relevant keywords. For pages that already exist, `kind` is ignored and the existing type is preserved, but `description` and `tags` fill in a missing field if the page does not yet have one.

Example call shape: `wiki_append({page: "architecture", body: "# Architecture\n\n...", kind: "Reference", description: "Module map and data flow", tags: ["architecture", "subsystems"]})`.

Write `project-context` last, after the other four exist, so it can link to them accurately.

## Step 4: Report

End with a short summary of which pages you created or updated. Do not dump page contents; the wiki files are the artifact.

## Rules

- Use the project's current working directory as the project root. Do not explore outside it.
- Keep each page focused and skimmable. Prefer lists and short paragraphs over walls of text.
- Cite specific file paths (e.g. `src/main.rs:12`) so claims are verifiable.
- Never invent facts. If you cannot confirm something, leave it out.
- Every `wiki_append` call already refreshes the index, so do not call any separate index-rebuild step.
