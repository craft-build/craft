# Argosy

The `argosy` knowledge crate is linked **in-process** — no subprocess, no MCP transport. Its tools register as first-class native tools with the same typed-JSON interface as the builtins.

Argosy keeps its own state keyed by project root: writes from craft are visible to the standalone `argosy mcp` server, and vice versa. Embeddings run locally (ONNX, via the argosy crate) — nothing leaves your machine for indexing.

## Search and ask

- `search` — hybrid semantic search over the project's knowledge (documents, memory, skills, rules): exact-term BM25 and vector similarity fused by reciprocal rank fusion. Exact identifiers and rule IDs rank alongside paraphrases.
- `ask` — retrieval augmented by an optional Jev-compatible decision endpoint: hybrid-retrieves candidates, then asks the endpoint which (if any) answers the question, returning the selection with a calibrated confidence.

Retrieval is passage-level: hits carry the matched passage, a heading breadcrumb, and line numbers, so the agent often answers without a further read.

## Documents, memory, rules

Reads and writes for every concept kind: documents (curated project knowledge), memory (session learnings), and styleguide rules (conventions the project wants enforced). Writes reconcile the index immediately.

## Skills

Project-stored skills join the `skill` tool's discovery with shadowing precedence (see [Skills & Recipes](./skills-recipes.md)).

## Review toolset

Grounded like `argosy mcp`: `start_review`, `review_diff`, `report_finding`, `review_findings`. `start_review` runs the deterministic styleguide pattern pre-pass over the diff's added lines; `report_finding` validates rule URIs. See [Review](./review.md).

## Memory extraction

Post-turn memory extraction (on by default; `agent.memory_extraction`) writes durable facts from a turn into the project's local argosy, so future sessions find them via `search`. `/memory` opens the workflow in the TUI.
