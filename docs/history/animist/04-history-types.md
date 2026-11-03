# 04 — History types become animist history

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt)
  and the [harness design rules](../../animist/harness-design.md). The design rules win
  where they disagree with the README. Start with the
  [test inventory](../../animist/harness-design.md#test-integrity).
- Read the 02/03 completion notes first. If the merged 02/03 cutover already
  removed `edge.rs` or changed `crate::history`, skip the steps below that it
  already covered, and say so in the completion note.
- When implementation is finished, **run the review agent**
  as described in [Finish](#finish).

## Goal

`crate::history` stops defining its own message model. The whole crate (run
loop, TUI, ACP, headless, compaction, sessions) speaks
`animist::history::{Message, ContentBlock, ToolContents, ToolContent,
ImageSource, Role, MessageKind, StoredMessage}`. Old session files still load.
`edge.rs` is deleted. The run loop is still craft's until 05, running over
`Vec<StoredMessage>`. The `History` handle comes in 05.

This prompt also makes the **only** session format bump of the migration: the
final entry-record vocabulary is defined here, and 05's change-feed persister
writes into it.

## Read first

- Craft:
  - `agent/src/history.rs`, `agent/src/edge.rs` (03's mapping becomes the
    legacy converter)
  - `agent/src/storage/sessions/{mod.rs, log.rs, legacy.rs, tests.rs}`
  - `agent/src/headless.rs` (`StoredSession`)
  - `agent/src/run/overflow.rs` (markers), `agent/src/run/nudge.rs`,
    `agent/src/run/doom.rs` (grace prompt)
  - `agent/src/tui/provider/live/resume.rs`, `agent/src/tui/ui/messages/`
  - `agent/src/acp/` (session load)
  - `docs/hardening/27-synthetic-grace-message-cleanup.md`
- animist 0.7.0: `src/history/{message.rs, stored.rs}`, `docs/history.md`
  (*At-depth system instructions*, *Tool-result names*, *Tool-result content
  compatibility*).

## Work

1. **The module.** `crate::history` re-exports the animist types. Add helpers
   only for what animist lacks: check `Message`, `ContentBlock` and
   `StoredMessage` first, and don't wrap anything animist already provides.
   Prefer free functions in `history::ext` over an extension trait unless
   method syntax removes real noise at call sites. Expected helpers:
   - `text()`, plus constructors `user_text`, `assistant_text`, `observation`;
   - `tool_calls()` and `tool_results()` iterators;
   - `tool_name(&ContentBlock, &[Message])`: the result's stored `name`, else
     the matching `ToolUse`'s name by `tool_use_id` (legacy entries load with
     `name: None`), else `"unknown"`;
   - image helpers that map craft `ImageMedia` + base64 onto
     `ImageSource::Base64`;
   - `Usage`, either kept as craft's ledger type or replaced with
     `animist::provider::TokenUsage` if every field maps. Decide based on
     `usage.rs` and the session `token_usage` shape.
2. **Semantic gaps.** Resolve each one explicitly:
   - **Tool name on results.** animist results carry `name: Option<String>`,
     and the agent fills it for every result it writes. Callers that read the
     name (dedup, guardrails, read lifecycle, TUI cards, compaction
     extractors, ACP) use `ext::tool_name`, which falls back to the matching
     `ToolUse` only for legacy entries. The legacy converter sets `name` from
     the old record, so converted sessions are self-describing too. Test an
     orphaned legacy result: name unknown → `"unknown"`.
   - **`ToolResultContent::Json`.** Render to pretty text when converting.
     Tools that produce JSON render it themselves (06).
   - **Image captions** (`ImageBlock::caption`). Move them to
     `Message::display_text` or drop them. Check what the TUI shows and keep
     that.
   - **`System` messages.** Map to `Role::System` text-only messages
     (03's mapping). The legacy converter keeps their position and
     authority.
   - **Markers.** `CANCEL_MARKER`, `END_MARKER`, `[Run failed: …]`, the doom
     grace prompt and the nudge prompt become `Message { role: User, kind:
     Observation }`, stored with a `craft.marker = cancel|end|failed|grace|nudge|continuation`
     tag. Check marker identity by tag, not by text. That fixes the
     text-equality fragility hardening/27 describes.
     - Tags live on `StoredMessage`, not `Message`. The loop, the surfaces
       and the session store therefore move to `Vec<StoredMessage>` in this
       prompt. **No side map**: a parallel tag store goes out of sync on
       every compaction and is thrown away in 05 anyway.
     - Provider calls take `current`-style `Vec<Message>`, mapped at the
       call site.
3. **The legacy converter.** Move the old craft enums into
   `storage/sessions/legacy_messages.rs` as serde-only types, with a
   `legacy::Message → Vec<StoredMessage>` converter (03's `edge.rs` mapping, in
   the history direction). Recognize old marker texts and tag them.
   Old-session timestamps come from the session's `created_at`/`updated_at`;
   document the approximation.
4. **The session format: the one bump.** Bump the session `version` and
   define the final record vocabulary, mapping one to one onto animist's
   `HistoryChange` (05 writes all of them; this prompt writes only `entry`):
   - `entry { entry: HistoryEntry }` ← `Appended`;
   - `insert { index, entry }` ← `Inserted`;
   - `revise { index, message: StoredMessage }` ← `Revised`;
   - `deactivate { index }` ← `Deactivated`;
   - `replace { index, entry }` ← `EntryReplaced`.

   Indices refer to the entry list at the time of each record, as animist
   defines them, so replaying records in order reproduces the snapshot. Unit
   test the replay function for all five record kinds now, using
   hand-built `HistoryEntry`s, even though only `entry` is written yet.
   - Each saved message is an `entry` record holding a source-only
     `HistoryEntry` (`HistoryEntry` is serde in 0.7.0).
   - The loader accepts the new records, the current JSONL records and the
     `.json` legacy path. Try the new shape first; otherwise decode as legacy
     and convert. Put it all in one loader module.
   - Saving a loaded legacy session rewrites it in the new shape, the way
     `.json` → `.jsonl` works today.
   - `subagent_messages` become entry lists too.
   - `tool_outputs` stays readable but is no longer written; its raw copies
     move into entry sources in 08. Until then, keep writing it in its
     animist shape (`ContentBlock::ToolResult`, which now carries the name).
     Note that this record goes away in 08.
5. **Delete `edge.rs`.** The provider call takes animist messages directly. The
   request-shaping rules from 03 (user coalescing, image relocation for Chat
   Completions) become pure `fn(Vec<Message>) -> Vec<Message>` functions in
   `run/view.rs`, with no trait and no state. 05 calls them from
   `RequestLayer::prepare_request`. Delete user coalescing if no animist
   adapter needs it: check the adapters, and record the answer.
6. **Callers.** Update every crate user (about 60 files reference `history::`).
   - TUI rendering: thinking blocks, tool cards, images, markers styled by tag.
   - ACP `session/load` replay, headless JSON output, print mode.
   - `compaction/estimate.rs` token estimation over `ContentBlock`.
   - `read_lifecycle`, `recency`, `nudge`, `dedup`.

## Tests

- Golden legacy fixtures: write session files in the *current* format before
  changing code (one per shape: text, reasoning with signature, Bedrock-nested
  signature, parallel tool results, image tool result, JSON tool result,
  markers, subagent histories). Commit them, and assert that each loads into
  the expected animist messages and tags.
- Round trip: load legacy → save → load equals the first load.
- `validate_replay` accepts every converted fixture, for the tools it
  references.
- Marker detection by tag: a user message whose text equals `CANCEL_MARKER`
  is not treated as a marker.
- Record replay: each of the five record kinds, applied to a hand-built
  entry list, gives the expected `HistoryEntry`s, including indices after
  an earlier `insert`.

## Acceptance

- `agent/src/edge.rs` and the old `history` enums are gone, apart from the
  serde-only legacy module.
- Every pre-existing session fixture and test passes, with the format change
  documented in user docs: older crafty builds cannot read new saves.
- No rig message types remain anywhere.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note: how captions and `Usage` were decided, the new
   format version, the downgrade caveat, the test-inventory diff, and the
   [indirection rows](../../animist/harness-design.md#no-indirection-without-a-job) closed.

## Completion note

**2026-10-30, branch `animist-migration`**, commits `7b137693` (golden
fixtures, written with pre-cutover code) and this one (base `a73e3db3`).

`crate::history` is now animist history: the module re-exports
`Message/ContentBlock/ToolContent(s)/ImageSource/Role/MessageKind/
StoredMessage/HistoryEntry`, craft-side helpers live in `history::ext`
(free functions: text extraction, constructors, `tool_calls`/`tool_results`
iterators, `tool_name` with the call-fallback for legacy results, image
helpers, `stored`/`entries`/`stored_of_entries`), and marker identity lives
in `history::marker` (`craft.marker` tags on `StoredMessage`). `edge.rs` is
deleted; its request-shaping rules are pure functions in `run/view.rs`
(`shape_messages`, `relocate_tool_result_images`, `coalesce_user_messages`,
`assert_replay_valid`) applied after the request rewrites, exactly where the
old edge sat. The loop and every surface speak `Vec<StoredMessage>` and map
to `Vec<Message>` at the provider call.

- **Captions**: dropped. `view_image` already ships its human-visible
  summary as a leading Text block (the caption the TUI showed), composer
  attachment captions were never rendered and never model-visible, and
  `dispatch`'s hardcoded `"[image]"` placeholder survives as
  `ext::tool_content_text`'s image rendering. The Chat-Completions
  relocation placeholder no longer embeds the caption (it names the image
  generically) — the only model-visible string that changed.
- **Usage**: `history::Usage` is deleted and `crate::usage::TokenUsage`
  re-exports `animist::provider::TokenUsage` (every field maps; helpers
  became free functions in `usage.rs`). The two identical
  `StoredTokenUsage` types collapsed into `usage::StoredTokenUsage`. Cache
  counters now flow into pricing for the first time (the old seam zeroed
  them); folded display semantics (`input_tokens` = input + cache_read,
  stream-json `total` = input + cache_read + output) are preserved.
- **Format**: one bump, `LOG_FORMAT_VERSION` 3 → 4 (the `.json` path keeps
  `SESSION_VERSION` 1). Messages persist as animist entry records —
  `entry/insert/revise/deactivate/replace`, one to one with animist's
  `HistoryChange` (only `entry` and `sub_entry` are written today; the
  replay function covers all five kinds and is unit-tested) — and
  `subagent_messages` are entry lists. One loader (`records.rs`) accepts v4
  records, v3 `msg`/`sub_msg`/`out` records (converted by
  `legacy_messages.rs`, markers recognized by text and tagged,
  timestamps approximated to the session's `created_at`) and `.json` files
  (current shape first, then the legacy shape converted); a loaded legacy
  session rewrites in the new shape on save, and `tool_outputs` stays
  readable but is never written. **Downgrade caveat**: older `crafty` builds
  cannot read new saves (documented in `agent/README.md`).
- **Coalescing**: kept. animist's Anthropic/Bedrock adapters map messages
  one to one with no merging, and both protocols require a step's tool
  results in the following single user message, so
  `coalesce_user_messages` remains a request-view rule (with a bug the
  review caught: only user-role messages may merge — the first cut folded
  assistant messages in and broke replay).
- **hardening/27**: `strip_trailing_markers` drops any trailing run of
  grace/nudge observations by tag (repeated suffixes included), never
  touches untagged text or assistant replies.

**Validation**: `cargo fmt -p craft --check` clean; clippy 93 lib / 118
all-targets against the 02 note's 93/119 baseline; `cargo test -p craft`
**2001 passed, 3 failed** — the three pre-existing recipe-render failures
recorded in 01 — and 1 pre-existing ignore (`tools/tests/read.rs`). No new
`#[ignore]`.

**Behaviour changes** (each pinned):

1. Session format v4 (above); older builds cannot read new saves.
2. stream-json `user` events serialize content in animist's block shape
   (was the craft enum shape); both were already non-Claude-Code shapes.
3. Cache-token counters reach pricing and usage ledgers (previously
   zeroed at the seam); folded display identities unchanged.
4. Marker identity is tag-based; a user typing a marker's text verbatim is
   no longer treated as one (TUI cancel-close, grace stripping).
5. TUI usage label and ACP `UsageUpdate` keep their exact folded values.

**Removed tests → replacements** (9): `edge::tests` (6) →
`run::view::tests::{adjacent_user_messages_coalesce,
empty_assistant_turns_drop, tool_result_images_relocate_only_when_asked,
image_bearing_user_messages_stay_separate, request_view_passes_replay_validation}`
plus `stream`/`turns` request assertions; `history::tests` (2) →
`history::ext::tests` + marker/serialization coverage in the session
round-trips; `usage::rig_usage_converts_with_zero_cache_counters` died with
`history::Usage` (cache counters now pass through natively). **Added**: 33
(ext/marker units, marker-by-tag, five-record replay, eight golden-fixture
loads + round trip + replay validation, legacy `.json` load, legacy-format
archive pin, out-record readability, rig-Json pretty-print).

**Review**: three parallel reviewer passes (core/sessions/surfaces) found
3 P1 + 3 P2 + a P3 batch — all fixed: out-of-range `insert` records could
panic the loader; the archive prefix cutover silently disabled archiving
for legacy-format logs; legacy `.json` sessions failed to deserialize (the
loader now converts them); `LegacyRecord::Meta` had a nested-key shape real
files never had; stream-json `total_tokens` drifted from the folded
identity; the rig-Json pretty-print coverage was tautological (dispatch
test added); plus fail-closed gates, request-order restoration
(lifecycle/compress before protocol shaping, context-size estimate on the
rewritten view), estimate clone removal, and comment cleanups. A second
verification pass confirmed **13/13 fixes correct**, and its one residual
P2 (context_size estimated from the raw view) and two doc nits are fixed.
Two findings are recorded in the argosy review session (`c51376c4…`);
new-file findings (records.rs, legacy_messages.rs, history/, view.rs are
untracked in the snapshot) live here instead.

**Indirection rows closed**: none — the inventory's rows all land in 05+.
`edge.rs` was not an inventory row; it is simply gone.

