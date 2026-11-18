# Argosy

Argosy is Craft's project knowledge layer. It stores documents, memory, skills, and styleguide rules so the agent can find them across sessions. It is built in: you do not need to start a separate process or configure an MCP server.

Knowledge is associated with the project root. Craft and the standalone `argosy mcp` server can read each other's writes for the same project. Indexing and embeddings run locally; this does not mean model requests or optional decision-endpoint requests stay on your machine.

## Search and ask

- `search` combines exact-term and semantic search over project knowledge, so it can find both exact identifiers and paraphrased concepts.
- `ask` retrieves candidate passages and can use an optional Jev-compatible decision endpoint to select which, if any, answers the question. The result includes a confidence score.

Retrieval is passage-level: hits carry the matched passage, a heading breadcrumb, and line numbers, so the agent often answers without a further read.

## Documents, memory, rules

The agent can read and write documents (curated project knowledge), memory (facts learned during work), and styleguide rules (project conventions). Writes update the index immediately.

## Skills

Project-stored skills join the `skill` tool's discovery with shadowing precedence (see [Skills & Recipes](./skills-recipes.md)).

## Review toolset

The review tools are `start_review`, `review_diff`, `report_finding`, and `review_findings`. `start_review` first checks added lines against styleguide patterns; `report_finding` validates references to styleguide rules. See [Review](./review.md) for the workflow.

## Memory extraction

After a successful run, Craft extracts durable facts into local project memory so later sessions can find them with `search`. This is enabled by default. `/memory` opens the workflow in the TUI.

To disable automatic extraction, set `memory_extraction = false` inside the `agent` block in your [configuration](./configuration.md#memory-and-display).
