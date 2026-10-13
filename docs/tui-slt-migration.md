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

# Wave 2b (remaining)

* Adopt slt input widgets ([`TextareaState`] for the composer + the in-TUI
  editor modal, [`TextInputState`] for palette/search/picker query rows
  and question-form fields) for hardware-caret fidelity, per the planning
  decisions. Fixtures are re-captured with the caret-rendering delta
  applied deliberately and reviewed. The wave-2a spoofed block caret
  (`apply_caret` in composer.rs and the query-row carets) is what 2b
  replaces.
* The `$EDITOR`/Ctrl-O handoff currently runs through the in-TUI
  `EditorForm` modal (plain buffer editing, Ctrl-S save / Esc cancel);
  2b decides whether `TextareaState` keeps or replaces it. Ctrl-Z
  suspend/resume is slt built-in (`RunConfig::handle_suspend`).
