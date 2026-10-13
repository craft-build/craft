# TUI Porting Spec: batch wave 1 (app-wide type swap)

The TUI is migrating from ratatui to `superlighttui` (`slt`). This wave swaps
the app's *data types* to the new foundation modules while the legacy
ratatui paint layer keeps running through interim compat shims. Rendering
moves to slt in a later wave; **do not rewrite rendering in this wave.**

## New foundation modules (already landed)

| Old import | New import |
|---|---|
| `use ratatui::text::{Line, Span}` | `use crate::tui::text::{Line, Span}` |
| `use ratatui::style::Style` / `Color` / `{Modifier, Style}` | `use crate::tui::style::{Style, Color, Modifier}` (same names, slt-backed) |
| `use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, ...}` | `use slt::event::{...same names...}` |

These types keep the same constructor/method shapes as the ratatui ones:

* `Span::raw(..)`, `Span::styled(.., style)`, `Line::from(vec![..])`,
  `Line::from(String)`, `Line::default()`, `line.width()`, `line.spans`.
* `Style::default()`, `.fg(c)`, `.bg(c)`, plus `StyleExt`: `.add_modifier(m)`,
  `.remove_modifier(m)`, `.patch(other)`. **Anywhere `add_modifier` / `patch` /
  `remove_modifier` is used, add `StyleExt` to the `crate::tui::style` import**.
* `Color::Rgb(u8,u8,u8)`, `Color::Indexed(u8)`, named colors — unchanged.

## Renames to apply

1. **Modifier constants** (import name stays `Modifier`):
   * `Modifier::UNDERLINED` → `Modifier::UNDERLINE`
   * `Modifier::CROSSED_OUT` → `Modifier::STRIKETHROUGH`
   * `Modifier::SLOW_BLINK`/`RAPID_BLINK` → `Modifier::BLINK`
   * `Modifier::empty()` → `Modifier::NONE`; `.insert()` / `.contains()` exist.
2. **Key events**: `slt::event::KeyEvent { code, modifiers, kind }` — same
   fields/names as crossterm, **no `state` field**. `KeyEventKind::{Press, Repeat, Release}` unchanged.
   * Detection helpers: `key.is_char('x')`, `key.is_code(KeyCode::Esc)`,
     `key.is_ctrl_char('c')` exist on slt KeyEvent (kind==Press, modifiers checked).
   * Construction in tests: `slt::Event::key(KeyCode::Char('x'))` then patch
     fields (`if let slt::Event::Key(k) = &mut ev { k.modifiers = ... }`), or use
     `slt::test_utils::EventBuilder` (`.key(...)`, `.key_code(...)`, `.key_mod(...)`, `.build()`).
3. **KeyModifiers**: `slt::event::KeyModifiers` — same bit layout as crossterm;
   `.contains(x)` works, raw field is `.0` (`u8`) if you need `intersects`
   (use `(a.0 & b.0) != 0`), and `KeyModifiers::NONE`.
4. **Mouse events**: `slt::event::MouseEvent`:
   * `mouse.column` → `mouse.x` (u32), `mouse.row` → `mouse.y` (u32) —
     cast `as u16` when feeding existing u16 math/hit-testing.
   * `MouseEventKind` → `MouseKind` (same variants: Down/Up/Drag take
     `MouseButton::{Left,Right,Middle}`, plus Moved/ScrollUp/ScrollDown/ScrollLeft/ScrollRight).
5. **Event enum**: `slt::Event::{Key, Mouse, Resize(u32,u32), Paste(String), FocusGained, FocusLost}`.
   The legacy input pump hands you slt events already (translated in `tui/mod.rs`).

## Geometry

**Only files that store rects across frames** use `crate::tui::geom::Rect`
(u16, ratatui-shaped: `Rect::new(x, y, w, h)`, `.contains(row, col)`,
`.right()`, `.bottom()`, `.area()`, `shrink()`, plus `rect_contains(r, row, col)`
re-exported from `crate::tui::selection` if that's where you import it from).
Paint-time rects stay `ratatui::layout::Rect` this wave. Conversions at the
hand-off point: `geom::Rect::new(r.x, r.y, r.width, r.height)`.

## Paint sites (Paragraph/Block) — interim conversion

Ratatui still paints this wave. Where app `Line`/`Span` values reach a
ratatui widget, wrap with the compat shims (`use crate::tui::compat;`):

* `Paragraph::new(lines_vec)` → `Paragraph::new(compat::rat_text(&lines_vec))`
* `Paragraph::new(line)` / `.title(line)` → `compat::rat_line(&line)`
* `Paragraph::new(String)` / `&str` — unchanged (ratatui-native).
* ratatui `Line` used *only* as a Block title: build a `ratatui::text::Line`
  directly instead of the app type (e.g. plan_form's bordered title).

Do **not** change: `Paragraph`, `Block`, `Borders`, `Clear`, `Wrap`,
`Layout`, printing into rect math, buffer surgery (`cell.fg/bg`, `REVERSED`
via ratatui Modifier write-ins, hyperlinks) — those are all still ratatui
and get their own wave. Where buffer surgery writes ratatui
`Modifier::REVERSED`, import it *as* `ratatui::style::Modifier` explicitly to
avoid clashing with the app Modifier alias.

## Theme/colors

`crate::tui::ui::theme` fields and the `fallback` consts switch to app
`Color`. Note its parser maps name strings ("bold", "underline", ...) to
constants — apply the renames there too. `Mode::color()` (app/mode.rs)
returns app Color.

## Verification

Iterate with your **private target dir** so parallel agents don't block you:

```
cargo check -p craft --lib --target-dir target/w1-<yourname>
```

Fix only errors/warnings in **your assigned files**. Other agents own theirs;
if a foreign file errors, note it in your report and move on. When your files
are clean, run the unit tests for your modules:

```
cargo test -p craft --lib --target-dir target/w1-<yourname> tui::<your modules>
```

Tests inside your files must pass (adapt their type constructions per the
renames above). Do not edit files outside your list. The slt source is at
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/superlighttui-0.25.0/`
(consult `src/event.rs`, `src/style.rs`, `src/style/color.rs` when unsure of
an API; `docs/` has more).

# Wave 2+ design notes (paint pivot — do not implement in wave 1)

Locked decisions from planning, kept here so later waves stay consistent:

* **Run loop**: `slt::run_async_with::<AgentEvent>(RunConfig{mouse:true,
  handle_ctrl_c:false, tick_rate≈80ms, ...}, frame)` + a forward task pumping
  the provider channel into `run.sender()`. The frame closure owns `App`/`View`
  and must be `Send + 'static` (own your data; no `Rc`, no borrows).
* **Transcript** (RawDraw painter, per planning): a plain `ui.container().col`
  whose children are emitted **per visible segment in document order**:
  text segments become fixed-height `.draw()` regions painting app
  `Line`s via `Buffer::set_string` (styles are already slt `Style`);
  image segments become slt image widgets (`ui.kitty_image` /
  `ui.iterm_image` / `ui.image(HalfBlockImage)`) so payload encoding and
  capability detection stay in slt. The existing scrollback engine
  (`bottom/clamp/doc_row`, `SegmentCache`) is untouched and decides heights
  and which rows are visible. Overlays that need cell surgery (search
  highlight fg↔bg swap, selection REVERSED, mouse regions) go in trailing
  `.draw()` regions declared last so they see the finished cells.
* **frame_text / regions**: painters run deferred (after the frame closure
  returns), so they can't borrow `App`. Hand data back through an
  `Arc<Mutex<..>>` held in `View` — one frame of staleness is fine (the
  current code reads the previous frame's snapshot too).
* **Hyperlinks**: replace the OSC-8 cell-surgery with slt's first-class
  `Cell.hyperlink` (set while painting spans); the tmux/mux gating stays.
* **Composer/queries**: adopt slt input widgets per the planning decision
  (real carets). The legacy crossterm pump and `tui/compat.rs` are deleted
  when the loop swaps.
* **Editor handoff**: the `$EDITOR` spawn is replaced with an in-TUI
  `TextareaState` modal (user decision); Ctrl-Z suspend is slt built-in
  (`RunConfig::handle_suspend`, default on).

# Wave 2 contract: `ui::frame` (the paint pivot)

`agent/src/tui/ui/frame.rs` (new) is the single slt entry point:

```rust
pub fn frame(ui: &mut slt::Context, app: &mut App, view: &mut View);
```

`View` is a new light struct in `frame.rs` for slt-side persistent state
(`ScrollState`-compatible offsets stay in `App` — the doc engine remains
the source of truth). The root layout is:

```
root col [ bg=BgApp ]
├── header row  (mode pill + cwd/branch + model + status)   [w2-chrome]
├── content row (grow)
│   ├── transcript col (grow)                               [w2-transcript]
│   └── sidebar (optional, fixed width)                     [w2-chrome]
├── composer block (border-top, TextareaState-backed)       [w2-chrome]
└── overlays: pending approval bar [w2-chrome], then open modals
   (palette / model menu / help / mcp / usage / search / file picker /
    permission / question / plan / editor modal)            [w2-overlay]
```

Panel signatures (each panel owns its file(s); frame.rs calls these):

* `ui::composer_slt::render(ui, app)` — composer box + status row.
* `ui::sidebar_slt::render(ui, app)` — right sidebar.
* `ui::messages_slt::render(ui, app)` — transcript col (see design notes).
* `ui::overlays_slt::render(ui, app)` — modal stack incl. prompts/forms.
* bang-mode re-styling + `frame_text`/regions handoff: trailing `.draw()`
  regions declared at the end of `frame()` in frame.rs itself.

Colors come from `ui::theme::current()` exactly as today; do NOT invent
palette values.

**Event flow**: frame.rs only paints. Input stays in `tui/mod.rs`, which
each frame hands to `app.handle_key/handle_mouse/insert_paste` via
`ui.events()` (translate nothing — they are already slt types). The paint
files therefore take `&App` / `&mut App` but never read input.
