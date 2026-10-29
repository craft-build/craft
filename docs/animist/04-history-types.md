# 04 — History types become animist history

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt).
- When implementation is finished, **run the review agent**
  (`.craft/agents/reviewer.md`) as described in [Finish](#finish).

## Goal

`crate::history` stops defining its own message model. The whole crate (run
loop, TUI, ACP, headless, compaction, sessions) speaks
`animist::history::{Message, ContentBlock, ToolContents, ToolContent,
ImageSource, Role, MessageKind, StoredMessage}`. Old session files still load.
`edge.rs` is deleted. The run loop is still craft's (`Vec<Message>`); the
`History` handle comes in 05/11.

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

1. **The module.** `crate::history` re-exports the animist types and adds an
   extension module (`history::ext`, an extension trait) for what craft relies
   on:
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
     - While the loop still uses `Vec<Message>`, keep the tag in a side map,
       or keep `Vec<StoredMessage>` in the loop if that is simpler. Prefer
       `Vec<StoredMessage>` if the churn is comparable, because 05 needs
       stored messages anyway.
3. **The legacy converter.** Move the old craft enums into
   `storage/sessions/legacy_messages.rs` as serde-only types, with a
   `legacy::Message → Vec<StoredMessage>` converter (03's `edge.rs` mapping, in
   the history direction). Recognize old marker texts and tag them.
   Old-session timestamps come from the session's `created_at`/`updated_at`;
   document the approximation.
4. **The session format.** Bump the session `version`:
   - New message records serialize animist `StoredMessage`.
   - The loader accepts both versions, plus the existing `.json` legacy path:
     try the new shape, otherwise decode as a legacy record and convert.
   - Saving a loaded legacy session rewrites it in the new shape, the way
     `.json` → `.jsonl` works today.
   - `subagent_messages` and `tool_outputs` (`Arc<T>`, where `T` was
     `history::ToolResult`) move to the animist shapes. Tool outputs store the
     animist `ToolResult` block itself, which now carries the name.
5. **Delete `edge.rs`.** The provider call takes animist messages directly. The
   request-shaping rules from 03 (user coalescing, image relocation for Chat
   Completions) move into `run/view.rs` as request-view functions. 05 lifts them
   into the `RequestLayer`.
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

## Acceptance

- `agent/src/edge.rs` and the old `history` enums are gone, apart from the
  serde-only legacy module.
- Every pre-existing session fixture and test passes, with the format change
  documented in user docs: older crafty builds cannot read new saves.
- No rig message types remain anywhere.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note: how captions and `Usage` were decided, the new
   format version, and the downgrade caveat.

## Completion note

_(fill in when done)_
