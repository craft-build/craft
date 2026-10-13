# TUI migration: ratatui → `superlighttui` (`slt`)

**Status: wave 2a is DONE.** The ratatui/crossterm/ratatui-image paint path,
the legacy run loop, `tui/compat.rs` (except `slt_key`), and `tui/hyperlink.rs`
are deleted; `run()` drives `slt::run_async_with` and `ui::frame::frame`
paints. The golden-fixture suite (`tui::parity_slt`) holds the 8 scenes
byte-identical. What remains is wave 2b (bottom of this doc).

## Historical: wave 1 (app-wide type swap) — complete

The app's data types moved to the slt foundation (`crate::tui::text`,
`crate::tui::style`, `slt::event`, `crate::tui::geom` for stored rects).
The interim compat shims (`rat_style`/`rat_line`/`rat_text`,
`slt_event`/`slt_mouse_button`/`slt_modifiers`) and the OSC-8 cell surgery
(`hyperlink::apply_to_cell`) are gone; `compat::slt_key` remains because
slt's `KeyEvent` is `#[non_exhaustive]` and the keybinding normalizer and
many tests construct events through it. The URI helper (`file_uri`) and
the mux gate (`is_muxed`) moved into `tui/ui/messages/mod.rs` next to the
painter that uses them.

## Locked design decisions (kept so 2b stays consistent)

* **Run loop**: `slt::run_async_with::<AgentEvent>(RunConfig{mouse:true,
  handle_ctrl_c:false, tick_rate≈80ms, ...}, frame)` + a forward task pumping
  the provider channel into `run.sender()`. The frame closure owns `App`/`View`
  and must be `Send + 'static` (own your data; no `Rc`, no borrows).
* **Transcript** (RawDraw painter, per planning): a plain `ui.container().col`
  whose children are emitted **per visible segment in document order**:
  text segments become fixed-height `.draw()` regions painting app
  `Line`s via `Buffer::set_string` (styles are already slt `Style`);
  image segments become slt image widgets (`ui.kitty_image` /
  `ui.iterm_image` / halfblock fallback) so payload encoding and
  capability detection stay in slt. The existing scrollback engine
  (`bottom/clamp/doc_row`, `SegmentCache`) is untouched and decides heights
  and which rows are visible. Overlays that need cell surgery (search
  highlight fg↔bg swap, selection REVERSED, mouse regions) go in trailing
  `.draw()` regions declared last so they see the finished cells.
* **frame_text / regions**: painters run deferred (after the frame closure
  returns), so they can't borrow `App`. Hand data back through an
  `Arc<Mutex<..>>` — one frame of staleness is fine (the current code reads
  the previous frame's snapshot too).
* **Hyperlinks**: slt's first-class `Cell.hyperlink` set while painting
  spans, with the OSC-8 bytes in the symbol (what the fixtures capture
  byte-for-byte); the tmux/mux gating stays.
* **Images**: `ui/image.rs` keeps the decode gates (8 MP pixel cap, 20 MB
  byte cap, `MAX_IMAGE_ROWS`) and probes the font cell size itself
  (`CSI 16 t` → `CSI 6 ; h ; w t`, cached, (8, 16) default under
  tests/tmux/pipes) before slt takes over the terminal; protocol
  detection and encoding live in slt's image widgets.

# Wave 2 contract: `ui::frame` (the paint pivot) — implemented

`agent/src/tui/ui/frame.rs` is the single slt entry point:

```rust
pub fn frame(ui: &mut slt::Context, app: &mut App, view: &mut View);
```

`View` is a light struct in `frame.rs` for slt-side persistent state
(`ScrollState`-compatible offsets stay in `App` — the doc engine remains
the source of truth). Root layout: transcript column + optional sidebar,
forms stacked above the composer block, overlays back to front, the
in-TUI editor modal on top, then the trailing selection/frame_text pass.

Panel signatures (each panel owns its file(s); frame.rs calls these):

* `ui::composer::render_slt(ui, app, avail)` — composer box + status row.
* `ui::sidebar::render_slt(ui, app)` — right sidebar.
* `ui::messages::render_slt(ui, app)` — transcript col (see design notes).
* `ui::overlays::render_slt(ui, app)` — modal stack incl. prompts/forms.

Colors come from `ui::theme::current()` exactly as today; do NOT invent
palette values.

**Event flow**: frame.rs only paints. Input stays in `tui/mod.rs`, which
each frame hands to `app.handle_key/handle_mouse/insert_paste` via
`ui.events()` (translate nothing — they are already slt types). The paint
files therefore take `&App` / `&mut App` but never read input.

**Fixtures**: `tui::parity.rs` defines the scenes;
`tui::parity_slt.rs` renders them through `ui::frame::frame` on
`slt::TestBackend` (two passes — the layout handoff settles on frame 2)
and compares against `agent/fixtures/golden/`. Re-capture with
`UPDATE_FIXTURES=1`.

# Wave 2b — DONE (composer + editor on input widgets; query rows on the ▎ idiom)

Adopted where typing actually lives, in three steps (all fixture-gated):

1. **Composer → `slt::TextareaState` (render mirror).** `View.composer`
   is synced each frame from `App.composer`: text via the legacy
   `wrap_rows` output (slt's own wrap is hard-cluster; ours is
   word-preferring — feeding pre-wrapped lines with `wrap_width = None`
   keeps row breaks byte-identical), cursor mapped onto those rows. The
   app keeps owning every edit (history gating on Up/Down, word ops,
   submit-on-Enter): the run-loop closure consumes each key/paste it
   dispatches (`ui.events().enumerate()` + `ui.consume_event(i)` —
   nothing consumes before the closure, so indices align; mouse stays
   unconsumed for click-to-focus), so the focused widget renders + carries
   the real caret (`▎` glyph + `buf.set_cursor_pos` hardware cell) but
   never edits. Accent bar, craft-theme restyle, bang syntax colors, and
   the placeholder paint in a trailing `grow(1)` overlay from the
   fixed-width wrapper's prev-frame rect (the outer box container's
   `Response::rect` predicts unreliably; the fixed-width wrapper's is
   exact — same one-frame handoff cadence as the transcript regions).
2. **Editor modal → `TextareaState` (widget-owned editing).** A plain
   textarea needs no custom semantics, so the widget owns editing
   natively: grapheme-safe caret, word jumps (Ctrl/Alt+arrows), undo/redo
   (Ctrl-Z/Y), kill-line (Ctrl-K). The closure intercepts Esc (cancel)
   and Ctrl-S (save), keeps readline Ctrl-A/E by moving the state cursor,
   leaves exactly the widget's own edit-key set unconsumed
   (`editor_key_edits_widget` mirrors the widget's consumption set), and
   swallows everything else. Pastes while the modal is open now land in
   the editor (2a sent them to the composer under the modal — fixed).
   Known cosmetic: while any slt modal is active, non-modal overlay draws
   collapse to 0×0, so the composer overlays pause under the editor modal
   (dimmed background only; no fixture scene opens the modal).
3. **Query rows → the `▎` idiom.** Palette query, question-form free
   text, permission-prompt guide input: the REVERSED-cell spoof became
   the `▎` glyph for one consistent caret idiom. These stay painted rows
   (not `TextInputState`) because the app must own every keystroke
   (list filtering, tab order) and the hardware-caret API is widget-only;
   forcing it through widgets needs `focus_by_name` choreography across
   the five sheet surfaces — optional follow-up, not worth the risk while
   the surfaces are transient single-line filters.

Fixture deltas, all deliberate caret changes: `composer.txt` (+`▎` at the
caret), `palette.txt` (+`▎` on the query row).

## Wave 2b debugging harnesses (kept for future regressions)

* `agent/examples/mock_tui` — the scripted mock provider end-to-end;
  `MOCK_TUI_STREAM=1` swaps in a GLM-shaped stream (ReasoningDelta then
  markdown-heavy AssistantDelta). Verified: streaming paints within
  ~250 ms under tmux.
* `CRAFT_TUI_TRACE=<path>` — per-frame render-loop trace (message/ui
  event counts, status, reveal state, painted visible-prefix length +
  paint area); zero-cost when unset.

## Remaining manual QA (pre-merge)

* Resize + suspend/resume (Ctrl-Z), CJK paste into composer/editor,
  mouse-copy, images on kitty/iTerm/tmux, hyperlink clicks, focus bells.
* Editor modal round trip: Alt-O from a non-empty composer, Ctrl-S back —
  verified under a py pty against `mock_tui` (typing, save, cancel).
* A `plan`-form fixture scene (title-cell nuance: slt pins titles 2 cells
  in vs ratatui's 1) — candidate for the next capture.

## Notable slt behaviors found during 2b (bit us once each)

* `ui.events()` is blocked while a modal was active the previous frame
  (the closure runs outside the modal scope). A global input router must
  use `ui.raw_events()` — otherwise every key dies while the editor modal
  is open.
* Container `Response::rect` predictions: a fixed-width wrapper predicts
  reliably frame-over-frame; the composer box container (margin + fixed
  w/h) came back 1-row-tall. Derive overlay geometry from fixed-width
  wrappers only.
* While any slt modal is active, non-modal overlay draws collapse to
  0×0 (deferred draws skip zero rects) — the composer's overlay pass
  pauses under the editor modal. Cosmetic only (dimmed background).
