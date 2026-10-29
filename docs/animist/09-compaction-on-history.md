# 09 — Compaction and alterations on `History`

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt),
  including the [async rules](README.md#async-native).
- When implementation is finished, **run the review agent**
  (`.craft/agents/reviewer.md`) as described in [Finish](#finish).

## Goal

Use animist's two history editing primitives for everything that rewrites the
conversation (README decision 5):

- **`Compactor`**: conversation-level plans (`Remove`/`Replace`/`Insert`)
  committed atomically. This covers the LLM summary and VCC, and replaces
  in-place `Vec<Message>` rewrites and 08's rewrite adapter.
- **`Alteration`**: per-message revisions of the latest active content, with
  the original and earlier revisions kept. This covers durable versions of
  today's per-step request-view rewrites: tool-output pre-compression,
  stale/superseded read markers, and stripping old tool results.

Originals then live in entry sources, so the `retrieve` tool and the TUI's
"show full output" read them from history. That retires
`SharedCompressionStore` and the session's raw `tool_outputs` map.

## Read first

- animist 0.7.0: `docs/history.md` (*Agent tool-result revision retention*,
  *Alteration*, *Compaction*, *VCC*, *Harness-specific extraction*,
  *Atomicity and concurrency*) and `src/history/{mod.rs, vcc.rs}`. Key facts:
  - `alter` visits each active entry's latest `StoredMessage`. Returning
    `Some` appends a revision; tags are editable via `tags_mut()`.
  - Both `alter` and `compact` snapshot, await without the lock, and commit
    only if the generation is unchanged, otherwise `Conflict`.
  - Neither drops revisions: `Remove` deactivates, and `Replace` appends.
    Only the agent squashes, and only its own in-flight tool-result entry;
    alteration and compaction revisions are never squashed.
  - Every committed edit appears in the history change feed, which 11
    persists. Passes don't report their edits separately.
  - VCC keeps every `Role::System` message and everything after the first one
    verbatim, so an early system instruction can block VCC entirely.
  - Any history edit during an `Agent::run` makes the agent's next commit fail
    with `HistoryChanged`, except inside `recover`, where the snapshot is
    re-taken.
- Craft:
  - `agent/src/compaction/{mod.rs, engine.rs, llm.rs, strip.rs, estimate.rs, vcc/}`
  - `agent/src/compression/`, `agent/src/compression/store.rs`
  - `agent/src/run/{read_lifecycle.rs, view.rs}`
  - `agent/src/tools/retrieve.rs`
  - the session `tool_outputs`
  - `agent/src/run/{guardrails.rs, doom.rs}` (reset on compaction)

## Work

1. **Where edits happen:**
   - **Between runs** (the driver, before `Agent::run` and after it returns):
     alteration passes, then compaction-engine stages.
   - **Inside `recover`** (08): overflow compaction only.
   - **Never** concurrently with a run. The driver owns the sequencing, and
     `Conflict` is handled explicitly: between runs, retry once with a fresh
     snapshot; never re-run LLM work implicitly.
2. **Alteration passes** (`compaction/alter.rs` or similar). Each is an
   `Alteration` impl:
   - **Output compression.** The `compression/` compressors applied to
     tool-result text once a result leaves the "recent" window. They produce
     a revision with compressed `ToolContents` plus a tag
     `craft.alter = compressed`. The original stays in the entry.
   - **Read lifecycle.** `classify_reads` over `history.current()` decides
     which `read` results are Stale or Superseded. The pass revises those
     results into today's marker text, tagged `craft.alter = stale|superseded`.
     Because `Alteration::call` sees one message at a time, compute the
     classification first (from a snapshot), then run an alteration that
     looks up per-entry decisions. Do the same for any pass that needs global
     context.
   - **Strip** (if `strip.rs` is a separate stage): collapse old tool results
     into stubs, keeping `tool_use_id` and `is_error`.
   - Passes must keep every `ToolResult` block (same `tool_use_id`) and its
     `ToolUse` pairing, and preserve thinking signatures. Assert
     `validate_replay` after each pass in tests.
   - **Re-alteration**: a pass sees the *latest* revision. Use tags so a
     compressed result isn't recompressed, and so a stale marker isn't
     re-marked.
3. **Narrow the request view.** `RequestLayer` (05) keeps only what must change
   *within* a run: the recency tail, plus read-lifecycle and compression for
   results produced during the current run. Everything older arrives
   pre-altered from history.
   - Measure the effect on provider prompt caching. Durable alterations change
     a prefix once, while the old view could change it every step.
   - Record request sizes before and after for a long scripted session.
4. **Retire the side stores.**
   - `retrieve` resolves its markers to the entry's `verbatim()` (or an earlier
     revision) by `tool_use_id`, reading from the conversation's `History`
     handle. Inject the handle into the tool at registration.
   - Delete `SharedCompressionStore`, plus the session `tool_outputs` map and
     its pruning (11 drops its record type).
   - Keep the retrieval marker text format, or change it everywhere at once.
5. **The plan contract** (compactors):
   - Edits index the snapshot's `entries()`, inactive entries included.
   - Remove whole tool exchanges or none.
   - Tag summaries `craft.compaction = llm|vcc`.
6. **The LLM stage** (`compaction/llm.rs`). `Remove` the head entries and
   `Insert` the summary before the first kept entry.
   - Keep the targeted-topics prompt and the overflow retry ladder. The ladder
     works on the summarizer's *request copy*.
   - The summarizer reads `current()`, so it sees altered (compressed)
     content. Check that summaries don't get worse; if they do, read
     `unaltered()` for the summarized range.
7. **VCC.**
   - Compare craft VCC with `animist::history::VccCompactor` on craft's VCC
     fixtures.
   - Port the harness knowledge (file/symbol extraction, sections,
     tool-specific breadcrumbs) into a craft `VccExtractor`, then use
     `VccCompactor::with_extractor`. The extractor sees the latest stored
     message, so use tags to recognize altered stubs, and read originals from
     the snapshot if a breadcrumb needs them.
   - Differences that animist's cut and merge rules can't express are either
     documented as accepted, or become a new followup prompt in animist's
     `docs/followups/`. Don't fork animist's algorithm.
   - Check whether any craft history (legacy sessions from 04) holds early
     `Role::System` messages. If so, record how VCC behaves on them.
   - Delete the craft VCC code the extractor no longer needs.
8. **The engine.** `CompactionEngine` keeps its threshold and effectiveness
   scoring over `estimate.rs` of `history.current()`. It runs from the driver
   between runs, which moves pre-turn compaction out of the surfaces so they
   behave identically, and from `recover`.
   - After a commit, reset guardrails and doom state, and emit
     `CompactionDone`.
   - `CompactionState`'s carry-anchor and `protect_from` re-index against
     snapshot entries.
9. **Estimation.** `estimate.rs` works over animist `Message`s plus `RequestOverhead`,
   with the measured input from `RunReport::stats` as the floor.

## Tests

- Alterations:
  - each pass keeps replay valid for the Anthropic, Responses and Chat
    Completions tool sets;
  - originals are recoverable via `snapshot()`;
  - tags prevent double application;
  - an alteration racing a concurrent `add` gives `Conflict` with nothing
    committed.
- `retrieve` returns the original output after compression and after
  compaction, including after a session reload in 11.
- Compactors: plan indices are correct with prior inactive entries present;
  exchanges are never split; summaries are tagged; the LLM summarizer is never
  called twice after a conflict.
- VCC parity fixtures go through the extractor, with reviewed expected-output
  diffs.
- The engine disarms an ineffective stage, as today.
- Request-view tests from 05 still pass, and their scope is now in-run only.

## Acceptance

- No code mutates a `Vec<Message>` and writes it back as history. 08's rewrite
  adapter is deleted.
- `SharedCompressionStore` and session `tool_outputs` are gone, and `retrieve`
  reads history.
- `compaction/vcc/` has shrunk to the extractor and its helpers. Report the line
  counts before and after.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note: the alteration passes and their tags, the
   prompt-cache and request-size measurements, the VCC differences, and any
   new followups.

## Completion note

_(fill in when done)_
