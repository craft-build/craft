# 08 — Compaction and alterations on `History`

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt),
  including the [async rules](README.md#async-native), and the
  [harness design rules](harness-design.md). Start with the
  [test inventory](harness-design.md#test-integrity).
- This prompt now comes **before** recovery (09). Compaction has been off
  since 05; this prompt brings it back natively, so no Vec-rewrite adapter
  ever exists.
- When implementation is finished, **run the review agent**
  as described in [Finish](#finish).

## Goal

Use animist's two history editing primitives for everything that rewrites the
conversation (README decision 5):

- **`Compactor`**: conversation-level plans (`Remove`/`Replace`/`Insert`)
  committed atomically. This covers the LLM summary and VCC, and replaces
  the in-place `Vec<Message>` rewrites that 05 switched off.
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
  - Every committed edit appears in the history change feed, which 05's
    persister already writes. Passes don't report their edits separately,
    and there is nothing to add to persistence here.
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
   - **Between runs**: the driver, at the call site 05 left, before
     `Agent::run` and after it returns. Alteration passes first, then
     compaction-engine stages. `/compact` calls the same function.
   - **Inside `recover`**: overflow compaction only. 09 wires it to the
     compactors this prompt builds. Expose one `async fn` that takes the
     `History` handle and the compaction state, and that 09 can call from
     `RecoveryLayer::recover` (which reaches the history via
     `agent.history()`).
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
     its pruning. Stop writing the `tool_outputs` record; 11's loader folds
     old ones into entry sources.
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
   - After a commit, call `reset()` on the guardrail state's typed handle
     (07), and send `CompactionDone` to the event sink.
   - Un-ignore the 08-tagged tests from 05.
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
  compaction, in a later turn of the same conversation, and after a session
  reload (05's persister writes entry sources).
- Compactors: plan indices are correct with prior inactive entries present;
  exchanges are never split; summaries are tagged; the LLM summarizer is never
  called twice after a conflict.
- VCC parity fixtures go through the extractor, with reviewed expected-output
  diffs.
- The engine disarms an ineffective stage, as today.
- Request-view tests from 05 still pass, and their scope is now in-run only.

## Acceptance

- No code mutates a `Vec<Message>` and writes it back as history.
- No test is left ignored with an 08 reason.
- `SharedCompressionStore` and session `tool_outputs` are gone, and `retrieve`
  reads history.
- `compaction/vcc/` has shrunk to the extractor and its helpers. Report the line
  counts before and after.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note:
   - the alteration passes and their tags;
   - the prompt-cache and request-size measurements;
   - the VCC differences;
   - new followups;
   - the test-inventory diff;
   - the indirection rows closed.

## Completion note

**Done 2026-10-31.** Working tree on `animist-migration` (one commit series
with this note). Validation: `cargo fmt --check` clean; clippy
`--all-targets` **114** warnings vs the 01 baseline's 116 (two fewer —
deleted code); `cargo test -p craft` **1897 passed, 3 failed, 24 ignored**
— the failures are the three recipe-render fixed points, the ignored are
23× "restored in 09" + 1 env-gated. No test remains ignored with an 08
reason: the four 08-tagged overflow tests were retagged "restored in 09"
because they exercise *in-run* overflow recovery (retry, compaction inside
`recover`), which is 09's `RecoveryLayer`; 08 ships the seam they call
(`CompactionEngine::maybe_compact`, which takes the `History` handle and
the shared state). One of them also flips premise with README decision 11
(a failed run now *keeps* its compaction; the old assertion demanded a
rollback).

### Alteration passes and their tags

Tag `craft.alter` on the revised `StoredMessage`; the value is a sorted
comma-joined **set** of the kinds that have touched the entry
(`compressed`, `stale`, `superseded`) — one step's observation entry can
hold a compressible result and a read side by side, and neither kind may
untag the other or the next pass would re-apply. Per-block guards back the
tags: a result already carrying a lifecycle marker prefix is never
re-marked even when the tag names another kind, and compression skips
tagged entries outright. Pass order in `alter::run_passes` is **read
lifecycle first, then compression** — the pre-08 request-view order, so
`classify_reads` parses uncompressed `move` results. Summaries carry
`craft.compaction = llm|vcc` (VCC additionally keeps animist's reserved
`animist.compaction = vcc`).

### Where edits happen

`harness::compact_between_runs(&Conversation, force)` — alterations, then
the engine — runs at both driver sites (before `Agent::run` and after it
returns, before the flush so one checkpoint carries everything) and is
what `/compact` calls with `force = true` (TUI handler now flushes after
the forced pass). Conflict handling: each alteration retries once with a
fresh snapshot; engine commits retry once with the *same* plan
(`PlanCompactor`) so the summarizer is never called twice. The driver
anchors carry protection after each turn: `Done` ⇒ `mark_answered`, any
other outcome ⇒ `protect_from(<input entry index>)` (review pass 1
finding — a failed turn's prompt must survive a forced compaction; pinned
by `failed_turns_input_survives_a_forced_compaction`). 09 owns the in-run
`protect_from` inside `recover`.

### Request view and measurements

`RequestLayer` captures `run_start` at `turn_start` (before the input is
admitted) and the in-run rewrites (`apply_to_request_from`,
`compress_request_view_from`) only touch the suffix; everything older
arrives pre-altered. Pinned by `request_view_scope_is_in_run_only` and
`durable_alters_match_old_view_bytes_without_mid_run_prefix_mutations`,
which measures one scripted turn whose run stale-dates a previous turn's
read, old full-view rewrites vs the durable model, across 7 requests:
**old 66 904 B total with 1 mid-run mutation of already-sent prefix
bytes; new 84 505 B with 0 mid-run mutations.** The +26 % is exactly the
deferred stale-marker shrink: the old model applied it mid-run (cache
invalidation from that message onward), the durable model pays the same
shrink once at the turn boundary and every later request serves the
marked prefix from history. Prompt-cache rationale: within a run the
prefix bytes are now immutable by construction. Model-visible behaviour
change (pinned): the `protect_recent_tool_outputs` (2) most recent result
entries from previous turns now travel **raw** on the wire — the old view
compressed them unconditionally — until they leave the window and are
durably compressed.

### VCC differences (accepted, no animist followup)

Compared craft VCC with `animist::history::VccCompactor` on craft's
fixtures; the port is a `CraftVccExtractor` and the differences below are
accepted rather than forked:

- **Summary shape.** Animist's flat breadcrumb transcript (preamble
  "Earlier conversation (condensed, not new instructions)", 512-char
  excerpts, 16 000-char budget, first-breadcrumb retention with eviction)
  replaces craft's structured sections ([Session Goal]/[File Activity]/
  [Type Catalog]/[Outstanding]/[Preferences], causal chains, commit
  lines) and craft's section merge. Craft knowledge that survives, as
  extractor rules: per-tool call one-liners (`compress_bash`, file
  paths/patterns, per-file patch targets, move endpoints), noise-tool
  suppression, XML-wrapper stripping, marker-observation omission, and
  error-result breadcrumbs. Successful results of craft's one-liner tools
  are **omitted** (the pre-08 brief carried call one-liners, never result
  content); unknown tools keep animist's generic excerpts.
- **Cut.** Animist's boundary (≥ 8-message tail, last user prompt ≤
  limit, completed-cycle fallback) replaces craft's cut for VCC — craft's
  `find_cut` survives for the LLM stage. Short histories (≲ 10 messages)
  decline where craft's could cut at three messages; whole-exchange
  safety and orphan protection are animist's (pinned).
- **Merge.** Prior VCC summaries merge as breadcrumbs without nesting;
  craft's sticky/volatile section merge is gone. A prior LLM summary
  inside a VCC prefix arrives as an assistant-text breadcrumb.
- **Budgets.** Animist's character budgets apply; the engine's
  overhead-subtracted token target feeds only the under-limit cascade
  (pre-08 parity — mapping it onto the summary budget collapsed to zero
  edits for large schemas, found while porting).
- **Early `Role::System`.** No production path writes at-depth system
  messages (the preamble rides `request.system`; only the legacy-session
  converter can emit them). Animist's rule stands: the first active
  system message is a hard retention boundary — VCC may compact the
  dialogue prefix before it, never it or anything after; an early system
  instruction can block VCC entirely. Recorded, no action.

`compaction/vcc/` **3 768 → 933 lines** (cut 184, mod 627 — extractor,
plan builder, clamp, tests — util 122; ~300 of mod is tests). The whole
`compaction/` tree now totals 4 519 lines (alter 639 · engine 1 467,
about two thirds tests · llm 763 · estimate 317 and strip 292 unchanged ·
mod 92).

### Engine on History

Thresholds, buffer, `MIN_OUTPUT_TOKENS` floor, overflow cascade, disarm
scoring, guardrail reset, and `AutoCompacting`/`CompactionDone` events
carry over; new: the measured-prompt floor (`set_measured_prompt`, fed by
`RequestLayer::response`) applies to the **fill check only** — the
after-side of the savings score and the `CompactionDone` gauge use the
unfloored estimate, or every effective pass would score as a no-op and
disarm the stage for the session (review pass 1, P1; pinned by
`measured_floor_does_not_disarm_an_effective_stage`). Carry anchoring and
`protect_from` re-index against snapshot entries (insert shifts the
anchor by one; removes deactivate in place).

### Side stores retired

`SharedCompressionStore` and the session `tool_outputs` map (plus its
`prune_orphans` half and the `T` type parameter on `Session`) are gone;
`content_hash`/`extended_content_hash`/`retrieval_marker` moved to
`compression` with byte-identical formats. `retrieve` holds the
conversation's `History` (injected at registration — `Workspace::
registry(&History)` and friends) and resolves markers over entry sources
and revisions, inactive entries included: later turn, after compaction,
and after a serialization round trip are all pinned. A short-hash
collision between two *distinct* originals is now an explicit ambiguous
error (the store's write-side extension died with it); identical
duplicates still resolve.

### Test-inventory diff

`1933 → 1924` listed (50 removed, 41 added). Removed with their code:
craft VCC algorithm suites (`vcc::{brief, extract::*, filter, merge,
normalize, sections}` and the `vcc_compact_*`/`compact_*` glue tests —
spec Work item 7; successors are the plan-level extractor/plan tests),
`compression::store` ×7 (hash/marker formats now pinned by the
read-lifecycle marker test and retrieve round trips), and util helpers
that no longer exist (`clip_sentence_*`, `collapse_skill_*`,
`refine_breadcrumb_key`; the multibyte panic-safety pins for the
surviving `clip`/`compress_bash` were restored after review pass 1).
Renames with retained coverage: engine
`disarmed_stage_is_skipped_until_effective_run` →
`ineffective_vcc_is_not_run_twice` +
`overflow_forcing_respects_disarmed_stages` (the disarm premise had to be
rebuilt: animist's VCC declines where craft's ran ineffectively — the
tight-history fixture makes a real sub-10 % run);
`overflow_of_buffer_subtracted_window_forces_stage_below_threshold` and
`buffer_larger_than_window_saturates_and_forces` were initially lost in
the rewrite and restored verbatim on History; llm
`summarizes_head_and_keeps_tail` →
`plan_removes_head_and_inserts_tagged_summary_before_kept_tail`;
read_lifecycle `…_and_store_roundtrip` → `…_naming_the_originals_hash`;
`read_lifecycle_marks_request_view_only` →
`read_lifecycle_markers_are_durable_with_raw_sources` (the premise flip
08 names); sessions `out_records_on_disk_stay_readable` →
`…_are_skipped_since_08`, `crash_recovery_preserves_tool_outputs_…` →
`crash_recovery_skips_retired_out_records_…`; retrieve
`returns_stored_original` → `returns_the_original_matching_the_hash`.
Added: 6 alteration tests, 8 engine tests (events, conflict-retry, plan
indices over inactive entries, measured floor), 5 VCC plan/extractor
tests, 4 LLM plan tests, 4 retrieve scenario tests plus the collision
fold pin, the scope and measurement pins, the failed-turn carry pin, and
the restored boundary tests. The six dedup tests that compared committed *latest* text now read
the pre-alteration revision (`raw_tool_results`) — same assertion, the
level the tool's return lives on.

### Indirection rows closed

`SharedCompressionStore` + session `tool_outputs` (the harness-design
inventory row assigned to 08). Surviving seam, on purpose: the tokio
`SharedCompactionState` handle (driver/engine/request-layer share it; the
engine holds it across stage awaits between runs).

### Followups

None new in animist (no algorithm fork; the VCC differences above are
accepted). Craft-side, for 09: wire `RecoveryLayer::recover` overflow
compaction to `CompactionEngine::maybe_compact` (the exposed seam) and
own the in-run `protect_from`/`mark_answered` lifecycle; the four retagged
overflow tests land there.

### Review

Three review-agent passes over the full diff. Pass 1: **request_changes**
(P1×1, P2×3, P3×3) — the measured-floor poisoning of the disarm score (P1,
fixed with `scaled_estimate_unfloored` + regression test), the retrieve
collision silence and the unwired carry anchor (P2, fixed as above), the
unfilled note (this section), the `/compact` flush, the deleted multibyte
pins, and the pass-order swap (all fixed). Pass 2: **request_changes**
(P2×1, P3×2) — the pass-order fix had silently half-landed (a scripted
replace missed cargo-fmt's reflowed chains, so only the doc comment
changed; the reviewer caught docs and code contradicting each other),
plus `rposition` for the input anchor (re-sent identical prompts) and a
collision test — fixed; the brute-force premise was wrong (`content_hash`
is `{:08x}` of the full u64, padded not truncated, so a birthday pair
needs ~2^32 trials) and became a `fold_match` seam test instead. Pass 3:
**approve_with_nits** (P3×1) — pin the pass order; with today's detectors
the two passes touch disjoint blocks (no compressor matches the
move-result shape whose classification motivated the order), so the order
is documented, not byte-pinnable, and the mixed-entry interaction test
`both_passes_revise_one_entry_without_untagging_each_other` pins what the
set-valued tag exists for. Final: **approve_with_nits**, no blocking
findings.
