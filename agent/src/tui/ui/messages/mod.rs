//! Message list rendering: user / assistant / tool blocks, plus scrolling.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use super::theme;
use crate::tui::app::{App, Message};
use crate::tui::compat;
use crate::tui::geom;
use crate::tui::hyperlink;
use crate::tui::style::Style;
use crate::tui::text::{Line, Span};
use crate::tui::ui::scrollback::{Layout, ScrollPos, Segment};

use std::collections::VecDeque;

mod cards;
mod text;
mod tools;
mod wrap;

pub(crate) use self::text::AssistantCache;
pub(crate) use self::wrap::wrap_rows;

use self::cards::tool_block;
use self::text::{notice_block, thinking_block, user_block, user_line};

const MARGIN: u16 = 2;

/// Doc-row deltas feed screen rows (hit regions, link rows, image
/// blits), which are `u16`. A huge document delta must clamp, not wrap:
/// a wrapped cast drops a far-offscreen card back onto the top of the
/// viewport and misaligns every click target under it.
fn clamp_row_u16(delta: u32) -> u16 {
    u16::try_from(delta).unwrap_or(u16::MAX)
}

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let inner = Rect {
        x: area.x + MARGIN,
        y: area.y + 1,
        width: area.width.saturating_sub(MARGIN * 2),
        height: area.height.saturating_sub(1),
    };
    if inner.width == 0 {
        return;
    }
    let g = crate::tui::geom::Rect::new(inner.x, inner.y, inner.width, inner.height);
    let build = build_frame(app, g);

    // App lines meet the legacy ratatui painter here; the compat shim
    // borrows them as ratatui text at the paint boundary. Image bands
    // blank-pad their reserved rows, exactly as the slice loop computed.
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner.height as usize);
    for band in &build.bands {
        match band {
            Band::Text(rows) => lines.extend(rows.iter().cloned()),
            Band::Image { rows, .. } => lines.extend((0..*rows).map(|_| Line::default())),
        }
    }
    let para = Paragraph::new(compat::rat_text(&lines));
    f.render_widget(para, inner);

    // Draw inline images over their reserved rows. Like the hyperlink
    // injection this happens after the Paragraph so the layout math is
    // untouched; skipped rows (scrolled out above or below) simply don't
    // draw.
    let mut off: u16 = 0;
    for band in &build.bands {
        match band {
            Band::Text(rows) => off += rows.len() as u16,
            Band::Image { state, rows } => {
                state.render(
                    Rect {
                        x: inner.x,
                        y: inner.y + off,
                        width: inner.width,
                        height: *rows,
                    },
                    f,
                );
                off += rows;
            }
        }
    }

    // The search modal's current match highlight (F.3): the segment's
    // visible cells are reversed, mirroring the reference's
    // `Cursor::render(..., highlight)`.
    if let Some((start, end)) = build.highlight {
        for row in start..end {
            for col in 0..inner.width {
                let cell = f
                    .buffer_mut()
                    .cell_mut((inner.x + col, inner.y + row))
                    .expect("bounded by inner");
                std::mem::swap(&mut cell.fg, &mut cell.bg);
            }
        }
    }

    // Rewrite the linked header cells in place: spans stayed plain text
    // during layout, so wrap math is unaffected. The mux gate and the
    // single-row/column bounds were applied when the links were resolved.
    for (row, col_start, col_end, uri) in &build.links {
        for col in *col_start..*col_end {
            let cell = f
                .buffer_mut()
                .cell_mut((inner.x + col, inner.y + row))
                .expect("col bounded by inner.width, vis by inner.height");
            hyperlink::apply_to_cell(cell, uri);
        }
    }

    // Publish the hit regions and the transcript area's width through the
    // frame handoff (event handlers read them one frame later).
    let mut p = app.view.paint.lock().expect("paint handoff");
    p.tool_regions = build
        .tool_spans
        .iter()
        .map(|&(idx, s, e)| {
            (
                idx,
                crate::tui::geom::Rect::new(g.x, g.y + s, g.width, e - s),
            )
        })
        .collect();
    p.notice_regions = build
        .notice_rows
        .iter()
        .map(|&(idx, r)| (idx, crate::tui::geom::Rect::new(g.x, g.y + r, g.width, 1)))
        .collect();
    p.width = area.width;
}

/// One horizontal band of the visible transcript produced by
/// [`build_frame`]: consecutive caption rows, or reserved inline-image rows.
enum Band {
    /// Caption rows in document order, sliced to the viewport.
    Text(Vec<Line<'static>>),
    /// Reserved inline-image rows, capacity-clamped like the text rows.
    Image {
        state: std::sync::Arc<crate::tui::ui::image::ImageRenderState>,
        rows: u16,
    },
}

/// The painter-neutral transcript frame: visible bands plus hit-region,
/// link, and highlight data, all addressed in inner-rect-relative rows so
/// the legacy ratatui painter and the slt painter share one math.
struct FrameBuild {
    bands: Vec<Band>,
    /// (message index, start..end rows) of collapsible tool cards.
    tool_spans: Vec<(usize, u16, u16)>,
    /// (message index, row) of "click to expand" notice rows.
    notice_rows: Vec<(usize, u16)>,
    /// (row, col_start, col_end, uri) of card link headers; empty under a
    /// muxed terminal or when every header is off-screen.
    links: Vec<(u16, u16, u16, String)>,
    /// Search-highlight rows (start..end), when the highlighted segment
    /// intersects the viewport.
    highlight: Option<(u16, u16)>,
}

/// Build the frame's visible transcript: refill the segment document, resolve
/// the viewport through the scrollback engine (follow pins to the bottom,
/// anything else is clamped back inside its segment), and slice the visible
/// rows into bands. This is the single source of truth for both painters —
/// the math is TRUTH, painting is per-backend.
fn build_frame(app: &mut App, inner: crate::tui::geom::Rect) -> FrameBuild {
    let t = theme::current();
    let width = inner.width as usize;

    // Build the frame's segment document: one segment per block, in
    // deterministic message order, so stored scroll positions survive the
    // refill and appended messages.
    app.view.segments.clear();
    app.view.assistant_cache.begin_frame();

    // Blank spacer carrying the user-message accent bar, so the block's
    // left border reads as one continuous line.
    let bar_blank = |width: usize| {
        user_line(
            vec![Span::styled("▎", Style::default().fg(t.accent))],
            width,
        )
    };

    if app.conversation.messages.is_empty() {
        let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
            "No messages yet.",
            Style::default().fg(t.text_tertiary),
        ))];
        lines.push(Line::default());
        #[cfg(test)]
        lines.push(Line::from(Span::styled(
            "Send a message — the mock provider will replay the scripted \"session refresh\" scenario.",
            Style::default().fg(t.text_disabled),
        )));
        #[cfg(not(test))]
        lines.push(Line::from(Span::styled(
            "Send a message to get started.",
            Style::default().fg(t.text_disabled),
        )));
        app.view.segments.push(Segment::with_lines(lines));
    }

    let mut msg_seg_start = Vec::with_capacity(app.conversation.messages.len());
    let mut tool_seg_ranges: Vec<(usize, usize, usize)> = Vec::new(); // (msg idx, start, end)
    let mut notice_seg_rows: Vec<(usize, usize, usize)> = Vec::new(); // (msg idx, seg, row)
    // (segment index, link) for card headers carrying an OSC-8 target;
    // resolved into viewport rows once the layout is known.
    let mut card_links: Vec<(usize, hyperlink::Hyperlink)> = Vec::new();
    // (segment, tool id, base64 payload) collected during the message walk;
    // resolved into render states right after it.
    let mut pending_images: Vec<(usize, String, String)> = Vec::new();
    for (idx, msg) in app.conversation.messages.iter().enumerate() {
        msg_seg_start.push(app.view.segments.len());
        let is_user = matches!(msg, Message::User(_));
        if is_user {
            app.view
                .segments
                .push(Segment::with_lines(vec![bar_blank(width)]));
        }
        match msg {
            Message::User(text) => app
                .view
                .segments
                .push(Segment::with_lines(user_block(text, width))),
            Message::Assistant(text) => {
                let text = app.conversation.visible_text(idx).unwrap_or(text);
                app.view.segments.push(Segment::with_shared_lines(
                    app.view.assistant_cache.render(idx, text, width),
                ));
            }
            Message::Thinking(text) => {
                let text = app.conversation.visible_text(idx).unwrap_or(text);
                app.view
                    .segments
                    .push(Segment::with_lines(thinking_block(text, width)));
            }
            Message::Notice { tone, text } => app
                .view
                .segments
                .push(Segment::with_lines(notice_block(*tone, text, width))),
            Message::Tool {
                id,
                kind,
                lines: body,
                diff,
                review,
                image,
            } => {
                let collapsed = app.conversation.collapsed.iter().any(|c| c == id);
                let body_expanded = app.conversation.expanded_bodies.iter().any(|c| c == id);
                let focused = app.conversation.focused == Some(idx);
                let hovered = app.view.hover_tool == Some(idx);
                let (lines, notice_row, link) = tool_block(
                    kind,
                    id,
                    body,
                    *diff,
                    focused,
                    collapsed,
                    hovered,
                    body_expanded,
                    width,
                );
                let seg = app.view.segments.len();
                if let Some(row) = notice_row {
                    notice_seg_rows.push((idx, seg, row));
                }
                if let Some(hl) = link {
                    card_links.push((seg, hl));
                }
                if kind.collapsible() {
                    tool_seg_ranges.push((
                        idx,
                        app.view.segments.len(),
                        app.view.segments.len() + 1,
                    ));
                }
                app.view.segments.push(Segment::with_lines(lines));
                // F.6 inline images: `view_image` results render below the
                // card body, in the same segment so scroll/height math
                // covers them. The message loop holds `&app.conversation`;
                // decoding is deferred to after it (the picker cache needs
                // `&mut app`).
                if let Some(data) = image {
                    pending_images.push((app.view.segments.len(), id.clone(), data.clone()));
                }
                // Auto-review rides just below the card, outside its chrome,
                // so the card itself stays focused on the tool's output.
                if let Some(review) = review {
                    app.view.segments.push(Segment::with_lines(notice_block(
                        review.tone,
                        &review.text,
                        width,
                    )));
                }
            }
        }
        if is_user {
            app.view
                .segments
                .push(Segment::with_lines(vec![bar_blank(width)]));
        }
        app.view
            .segments
            .push(Segment::with_lines(vec![Line::default()]));
    }
    app.view.assistant_cache.end_frame();

    for (seg, id, data) in pending_images {
        if let Some(state) = app.image_state(&id, &data, inner.width)
            && let Some(s) = app.view.segments.get_mut(seg)
        {
            s.set_image(Some(state));
        }
    }

    // Resolve the viewport through the scrollback engine: follow pins to
    // the bottom, anything else is clamped back inside its segment.
    let layout = Layout::new(&app.view.segments, inner.width);
    if app.view.follow {
        app.view.scroll = layout.bottom(inner.height);
    } else {
        app.view.scroll = layout.clamp(app.view.scroll);
    }
    let top_row = layout.doc_row(app.view.scroll);
    app.view.view_height = inner.height;
    app.view.view_width = inner.width;
    app.view.msg_starts = msg_seg_start
        .iter()
        .map(|&s| layout.doc_row(ScrollPos { seg: s, row: 0 }) as usize)
        .collect();

    // Slice the visible rows out of the segments starting at the scroll
    // position. Lines are pre-wrapped (one display row each), so slicing
    // rows is slicing lines; image rows blank-pad into their own bands so
    // following segments stay below the image.
    let mut bands: Vec<Band> = Vec::new();
    let mut taken: u16 = 0;
    let mut pos = app.view.scroll;
    while taken < inner.height && pos.seg < app.view.segments.len() {
        let seg = app
            .view
            .segments
            .get(pos.seg)
            .expect("bounds checked above");
        let caption = seg.lines().len().min(u16::MAX as usize) as u16;
        let img_rows = seg.image.as_ref().map_or(0, |img| img.rows);
        let capacity = inner.height - taken;
        // Caption rows are real lines; image rows have none, so they are
        // blank-padded to keep following segments below the image. A
        // position can point inside the image region, so clamp the slice
        // start to the caption lines (height() counts image rows too).
        let take_lines = caption.saturating_sub(pos.row).min(capacity);
        let start = (pos.row as usize).min(seg.lines().len());
        let rows: Vec<Line<'static>> = seg
            .lines()
            .range(start, take_lines as usize)
            .cloned()
            .collect();
        match bands.last_mut() {
            // Consecutive caption rows merge into one text band.
            Some(Band::Text(v)) => v.extend(rows),
            _ => bands.push(Band::Text(rows)),
        }
        taken += take_lines;
        let img_start = caption.max(pos.row);
        let img_end = (caption + img_rows).min(pos.row + capacity);
        let img_taken = img_end.saturating_sub(img_start);
        if img_taken > 0 {
            if let Some(state) = seg.image.clone() {
                bands.push(Band::Image {
                    state,
                    rows: img_taken,
                });
                taken += img_taken;
            }
        }
        pos.seg += 1;
        pos.row = 0;
    }

    // Visible screen rows of collapsible tool cards (for hover/click),
    // addressed in doc rows so they track the scroll position.
    let tool_spans = tool_seg_ranges
        .iter()
        .filter_map(|&(idx, s_start, s_end)| {
            let vis_start = clamp_row_u16(
                layout
                    .doc_row(ScrollPos {
                        seg: s_start,
                        row: 0,
                    })
                    .saturating_sub(top_row),
            );
            let vis_end = clamp_row_u16(
                layout
                    .doc_row(ScrollPos { seg: s_end, row: 0 })
                    .saturating_sub(top_row),
            )
            .min(inner.height);
            (vis_end > vis_start && vis_start < inner.height).then_some((idx, vis_start, vis_end))
        })
        .collect();

    // Visible rows of "click to expand" notice rows, addressed the same
    // way as card regions so they track the scroll position.
    let notice_rows = notice_seg_rows
        .iter()
        .filter_map(|&(idx, seg, row)| {
            let vis = clamp_row_u16(
                layout
                    .doc_row(ScrollPos {
                        seg,
                        row: row as u16,
                    })
                    .saturating_sub(top_row),
            );
            (vis < inner.height).then_some((idx, vis))
        })
        .collect();

    // Resolve card links to visible rows: the header sits one row under the
    // card's top-padding blank. Skipped under tmux, whose passthrough
    // mangles OSC-8. The single-row guard mirrors the reference: a header
    // is one pre-wrapped row, and column bounds keep any overflow from
    // writing outside the viewport.
    let mut links = Vec::new();
    if !hyperlink::is_muxed() {
        for (seg, hl) in card_links {
            let doc = layout.doc_row(ScrollPos { seg, row: 1 });
            // Skip rows scrolled out above (plain subtraction, not
            // saturating) or pushed out below the viewport.
            if doc < top_row {
                continue;
            }
            let vis = clamp_row_u16(doc - top_row);
            if vis >= inner.height {
                continue;
            }
            if hl.col_start >= inner.width || hl.col_end > inner.width {
                continue;
            }
            links.push((vis, hl.col_start, hl.col_end, hl.uri));
        }
    }

    let highlight = highlight_visible(app, &layout, top_row, inner);

    FrameBuild {
        bands,
        tool_spans,
        notice_rows,
        links,
        highlight,
    }
}

/// The search modal's current match highlight (F.3) as visible rows: the
/// highlighted segment's doc-row span intersected with the viewport.
fn highlight_visible(
    app: &App,
    layout: &Layout<'_>,
    top_row: u32,
    inner: crate::tui::geom::Rect,
) -> Option<(u16, u16)> {
    let seg = app.view.highlight_segment?;
    let s = app.view.segments.get(seg)?;
    let start = layout.doc_row(ScrollPos { seg, row: 0 });
    let end = start + u32::from(s.height(inner.width));
    let lo = start.max(top_row);
    let hi = end.min(top_row + u32::from(inner.height));
    (hi > lo).then(|| ((lo - top_row) as u16, (hi - top_row) as u16))
}

#[cfg(test)]
mod tests {
    use crate::tui::provider::ToolLine;
    use ratatui::style::Modifier;

    #[test]
    fn streaming_frames_share_completed_transcript_lines() {
        use crate::tui::app::App;
        use crate::tui::provider::AgentEvent;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::time::{Duration, Instant};

        let mut app = App::new();
        for _ in 0..100 {
            app.handle_event(AgentEvent::AssistantText(
                "Completed **Markdown** paragraph with `inline code`.".into(),
            ));
        }
        app.handle_event(AgentEvent::AssistantDelta("Streaming reply.".into()));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| crate::tui::ui::draw(f, &mut app))
            .unwrap();
        let cached = app.view.segments.get(0).unwrap().lines().clone();
        let now = Instant::now();
        for elapsed in [16, 32, 48] {
            app.tick_reveal(now + Duration::from_millis(elapsed));
            terminal
                .draw(|f| crate::tui::ui::draw(f, &mut app))
                .unwrap();
            assert!(cached.ptr_eq(app.view.segments.get(0).unwrap().lines()));
        }
    }

    /// Regions/links/images are placed at `(doc_row - top_row) as u16`;
    /// a document delta past 65535 must clamp to `u16::MAX` (offscreen
    /// below, correctly skipped) instead of wrapping onto the viewport.
    #[test]
    fn doc_row_delta_clamps_instead_of_wrapping() {
        use super::clamp_row_u16;
        assert_eq!(clamp_row_u16(0), 0);
        assert_eq!(clamp_row_u16(42), 42);
        assert_eq!(clamp_row_u16(u32::from(u16::MAX)), u16::MAX);
        assert_eq!(clamp_row_u16(u32::from(u16::MAX) + 5), u16::MAX);
        assert_eq!(clamp_row_u16(u32::MAX), u16::MAX);
    }

    /// F.6: a `view_image` result renders an inline image under the card
    /// body — the segment grows by the image's cell rows and halfblock
    /// glyphs land in the buffer below the header.
    #[test]
    fn view_image_card_renders_inline_image() {
        use crate::tui::app::App;
        use crate::tui::provider::{AgentEvent, ToolCallData, ToolKind};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let img = image::RgbaImage::from_pixel(16, 8, image::Rgba([255, 255, 255, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);

        let mut app = App::new();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t1".into(),
            kind: ToolKind::Bash {
                cmd: "view_image shot.png".into(),
            },
            lines: Vec::new(),
            awaiting_approval: false,
            image: Some(b64),
        }));
        let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
        terminal
            .draw(|f| super::render(f, &mut app, f.area()))
            .unwrap();

        let seg_idx = (0..app.view.segments.len())
            .find(|&i| app.view.segments.get(i).is_some_and(|s| s.image.is_some()))
            .expect("image segment built");
        let (caption, height) = {
            let seg = app.view.segments.get(seg_idx).unwrap();
            (seg.lines().len() as u16, seg.height(56))
        };
        assert!(height > caption, "image rows add to the segment height");

        // Halfblock fallback draws upper/lower-cell glyphs into the buffer
        // below the card header.
        let buf = terminal.backend().buffer();
        let symbols: Vec<&str> = buf.content.iter().map(|cell| cell.symbol()).collect();
        assert!(
            symbols.iter().any(|s| *s == "\u{2580}" || *s == "\u{2584}"),
            "halfblock glyphs rendered"
        );
    }

    /// Scrolling into a segment's image region must not slice the caption
    /// lines out of range: image rows are blank-padded, not real lines.
    #[test]
    fn scrolling_into_image_rows_does_not_panic() {
        use crate::tui::app::App;
        use crate::tui::provider::{AgentEvent, ToolCallData, ToolKind};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let img = image::RgbaImage::from_pixel(16, 8, image::Rgba([255, 255, 255, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);

        let mut app = App::new();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t1".into(),
            kind: ToolKind::Bash {
                cmd: "view_image shot.png".into(),
            },
            lines: Vec::new(),
            awaiting_approval: false,
            image: Some(b64),
        }));
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal
            .draw(|f| super::render(f, &mut app, f.area()))
            .unwrap();
        let seg_idx = (0..app.view.segments.len())
            .find(|&i| app.view.segments.get(i).is_some_and(|s| s.image.is_some()))
            .expect("image segment built");
        let seg = app.view.segments.get(seg_idx).unwrap();
        let caption = seg.lines().len() as u16;
        let height = seg.height(56);
        assert!(height > caption, "precondition: image rows exist");

        // Park the scroll one row past the caption, inside the image rows.
        app.view.follow = false;
        app.view.scroll = crate::tui::ui::scrollback::ScrollPos {
            seg: seg_idx,
            row: caption + 1,
        };
        terminal
            .draw(|f| super::render(f, &mut app, f.area()))
            .unwrap();
    }

    /// Auto-review renders as its own line *under* the tool card; the card
    /// keeps showing the tool's own header and output.
    #[test]
    fn auto_review_renders_under_the_card_not_inside_it() {
        use crate::tui::app::App;
        use crate::tui::provider::{AgentEvent, Tone, ToolCallData, ToolKind};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t1".into(),
            kind: ToolKind::Bash {
                cmd: "cargo test".into(),
            },
            lines: Vec::new(),
            awaiting_approval: false,
            image: None,
        }));
        app.handle_event(AgentEvent::AutoReview {
            id: "t1".into(),
            tone: Tone::Success,
            text: "auto-review allow: low — in-project edit".into(),
        });

        let mut terminal = Terminal::new(TestBackend::new(70, 16)).unwrap();
        terminal
            .draw(|f| super::render(f, &mut app, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let cmd_row = rows
            .iter()
            .position(|r| r.contains("cargo test"))
            .expect("tool header still shown");
        let review_row = rows
            .iter()
            .position(|r| r.contains("auto-review allow"))
            .expect("review line rendered");
        assert!(
            review_row > cmd_row,
            "review must sit under the card, not inside it: {rows:?}"
        );
    }

    #[test]
    fn assistant_markdown_read_highlight_and_painted_card_header() {
        let t = theme::current();

        use super::theme;
        use crate::tui::app::{App, Message};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::style::Color;

        let mut app = App::new();
        app.conversation.messages.push(Message::Assistant(
            "# Title\n\nsome **bold** and `code`\n\n```rust\nfn main() {}\n```".into(),
        ));
        let path = std::env::temp_dir().join("probe.rs");
        app.conversation.messages.push(Message::Tool {
            id: "t1".into(),
            kind: crate::tui::provider::ToolKind::Read {
                path: path.display().to_string(),
                summary: "2 lines".into(),
            },
            lines: vec![
                ToolLine::new(crate::tui::provider::LineKind::Context, "fn main() {"),
                ToolLine::new(crate::tui::provider::LineKind::Context, "}"),
            ],
            diff: None,
            review: None,
            image: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|f| super::render(f, &mut app, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();

        // Theme colors are app-typed now; convert at the buffer boundary.
        let accent = crate::tui::compat::rat_color(t.accent);
        let text_secondary = crate::tui::compat::rat_color(t.text_secondary);
        let text_tertiary = crate::tui::compat::rat_color(t.text_tertiary);

        // Agent text is markdown: heading rendered in accent bold.
        let hy = rows.iter().position(|r| r.contains("Title")).unwrap();
        let hx = rows[hy].find("Title").unwrap() as u16;
        let c = &buf[(hx, hy as u16)];
        assert_eq!(c.fg, accent, "heading fg");
        assert!(c.modifier.contains(Modifier::BOLD), "heading bold");

        // Read card body is syntax highlighted: some source cell carries a
        // color other than the plain secondary/tertiary card text.
        let cy = rows.iter().position(|r| r.contains("fn main() {")).unwrap();
        let hl_found = (0..buf.area.width).any(|x| {
            let c = &buf[(x, cy as u16)];
            matches!(c.symbol(), "f" | "m" | "(" | ")")
                && c.fg != Color::Reset
                && c.fg != text_secondary
                && c.fg != text_tertiary
        });
        assert!(
            hl_found,
            "read card code should be syntax colored; row={:?}",
            rows[cy]
        );

        // Card header: OSC-8 link present, full path text survives, and no
        // cell inside the message area falls back to terminal-default bg
        // (the pre-fix symptom: black sections after the linked cell).
        let ry = rows.iter().position(|r| r.contains("Read ")).unwrap();
        assert!(rows[ry].contains("\u{1b}]8;;file://"));
        assert!(rows[ry].contains("probe.rs"));
        for x in 2..78u16 {
            assert_ne!(
                buf[(x, ry as u16)].bg,
                Color::Reset,
                "unpainted cell at x={x}"
            );
        }
    }

    #[test]
    fn render_injects_osc8_into_visible_header_and_skips_scrolled_out() {
        use crate::tui::app::{App, Message};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new();
        let path = std::env::temp_dir().join("osc8_probe.rs");
        app.conversation.messages.push(Message::Tool {
            id: "t1".into(),
            kind: crate::tui::provider::ToolKind::Read {
                path: path.display().to_string(),
                summary: String::new(),
            },
            lines: vec![ToolLine::new(
                crate::tui::provider::LineKind::Context,
                "body",
            )],
            diff: None,
            review: None,
            image: None,
        });
        // Fill the document past the viewport so a scroll below the card
        // survives the layout clamp instead of snapping back to the top.
        for i in 0..15 {
            app.conversation
                .messages
                .push(Message::Assistant(format!("filler {i}")));
        }

        app.view.follow = false;
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|f| super::render(f, &mut app, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let row_text: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect();
        let header_row = row_text
            .iter()
            .find(|r| r.contains("Read "))
            .expect("header rendered");
        assert!(
            header_row.contains("\u{1b}]8;;"),
            "header row wrapped: {header_row:?}"
        );
        assert!(header_row.contains("file://"));
        assert!(header_row.contains("\u{1b}]8;;\u{1b}\\"));

        // Scroll the card fully above the viewport: no escapes anywhere.
        // A fresh terminal, so stale cells from the first draw can't leak
        // into the assertion (Paragraph only rewrites the rows it fills).
        app.view.follow = false;
        app.view.scroll = crate::tui::ui::scrollback::ScrollPos { seg: 1, row: 0 };
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|f| super::render(f, &mut app, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let any_osc8 = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .any(|(x, y)| buf[(x, y)].symbol().contains("\u{1b}]8;;"));
        assert!(!any_osc8, "scrolled-out link must not be injected");

        // A link whose column range runs past the viewport is skipped, but
        // its visible text must survive untouched (spans render first;
        // injection only adds escapes to cells it wraps).
        let mut narrow_app = App::new();
        let long = std::env::temp_dir().join("osc8_probe.rs");
        narrow_app.conversation.messages.push(Message::Tool {
            id: "t1".into(),
            kind: crate::tui::provider::ToolKind::Read {
                path: format!("{}?padpadpadpadpad", long.display()),
                summary: String::new(),
            },
            lines: vec![ToolLine::new(crate::tui::provider::LineKind::Context, "x")],
            diff: None,
            review: None,
            image: None,
        });
        narrow_app.view.follow = false;
        let mut narrow = Terminal::new(TestBackend::new(24, 12)).unwrap();
        narrow
            .draw(|f| super::render(f, &mut narrow_app, f.area()))
            .unwrap();
        let buf = narrow.backend().buffer();
        let screen: String = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert!(
            screen.contains("Read ") && screen.contains("/var/folders"),
            "off-screen link's visible text dropped: {screen:?}"
        );
        assert!(!screen.contains("\u{1b}]8;;"));
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Wave 2a: the slt transcript painter (segment flow per the docs contract)
// ─────────────────────────────────────────────────────────────────────────

/// Paint the transcript: the visible bands from [`build_frame`] become
/// per-segment children in document order — text runs as fixed-height
/// `.draw()` regions painting app `Line`s via `Buffer::set_string`, image
/// segments as slt image widgets — plus a trailing bookkeeping region that
/// publishes `msg_area` / hit regions / width into [`PaintData`] for the
/// next frame's event handlers and wrap math (one frame stale, like the
/// ratatui-era snapshot reads).
pub(crate) fn render_slt(ui: &mut slt::Context, app: &mut App) {
    // slt lays out after the frame closure returns, so the wrap width and
    // viewport height can only come from the previous frame's handoff; the
    // bookkeeping draw below republishes the true rect every frame (the
    // parity harness renders twice, so frame 2 is exact). 80 columns is the
    // first-frame fallback.
    let (area_w, area_h) = {
        let p = app.view.paint.lock().expect("paint handoff");
        (if p.width == 0 { 80 } else { p.width }, p.msg_area.height)
    };
    let inner = geom::Rect {
        x: 0,
        y: 0,
        width: area_w.saturating_sub(MARGIN * 2),
        height: area_h.saturating_sub(1),
    };
    let build = if inner.width == 0 {
        None
    } else {
        Some(build_frame(app, inner))
    };

    let paint = app.view.paint.clone();
    let area_only = paint.clone();
    // Region data crosses the col closure boundary through this slot so
    // the always-run overlay publish (declared after the col) can own it.
    let regions_out: std::sync::Arc<std::sync::Mutex<(Vec<(usize, u16, u16)>, Vec<(usize, u16)>)>> =
        std::sync::Arc::new(std::sync::Mutex::new((Vec::new(), Vec::new())));
    let regions_slot = std::sync::Arc::clone(&regions_out);
    let grow_resp = ui.container().grow(1).col(move |ui| {
        // The legacy painter's top margin row (its `inner.y = area.y + 1`).
        ui.container().h(1).draw(|_, _| {});

        let mut total_rows: u32 = 0;
        if let Some(build) = build {
            let FrameBuild {
                bands,
                tool_spans,
                notice_rows,
                links,
                ..
            } = build;
            let bg = Some(theme::current().bg_app);
            let cols = u32::from(inner.width);
            let mut links = VecDeque::from(links);
            let mut row_cursor: u16 = 0;
            for band in bands {
                match band {
                    Band::Text(rows) => {
                        let count = rows.len() as u16;
                        if count > 0 {
                            // Links land inside their band: rows are
                            // inner-relative and ascend with the walk.
                            let mut band_links = Vec::new();
                            while let Some(&(row, ..)) = links.front() {
                                if row >= row_cursor.wrapping_add(count) {
                                    break;
                                }
                                let (row, cs, ce, uri) = links.pop_front().expect("peeked");
                                band_links.push((row - row_cursor, cs, ce, uri));
                            }
                            ui.container()
                                .ml(u32::from(MARGIN))
                                .mr(u32::from(MARGIN))
                                .h(u32::from(count))
                                .draw(move |buf, rect| {
                                    paint_rows_slt(buf, rect, &rows, &band_links, bg);
                                });
                            total_rows += u32::from(count);
                            row_cursor += count;
                        }
                    }
                    Band::Image { state, rows } => {
                        if rows > 0 {
                            if let Some(px) = state.slt_pixels() {
                                // Margins align the widget with the text
                                // column; slt owns protocol detection and
                                // falls back to halfblocks itself.
                                ui.container()
                                    .ml(u32::from(MARGIN))
                                    .mr(u32::from(MARGIN))
                                    .h(u32::from(rows))
                                    .row(|ui| {
                                        if iterm_terminal() {
                                            let file = px.file.clone();
                                            ui.iterm_image(&file, cols, u32::from(rows));
                                        } else {
                                            let rgba = px.rgba.clone();
                                            ui.kitty_image(
                                                &rgba,
                                                px.pixel_width,
                                                px.pixel_height,
                                                cols,
                                                u32::from(rows),
                                            );
                                        }
                                    });
                            } else {
                                // No pixel payload: still reserve the rows so
                                // the flow below the image stays aligned.
                                ui.container().h(u32::from(rows)).draw(|_, _| {});
                            }
                            total_rows += u32::from(rows);
                            row_cursor += rows;
                        }
                    }
                }
            }
            // Unsatisfied-grow publish (frame 1 and degenerate cases):
            // a trailing in-col region that only has room when the bands
            // don't fill the viewport — carrying the TRUE area so the
            // second frame builds at the right width.
            ui.container().grow(1).draw(move |_, rect| {
                publish_area_slt(&area_only, rect, total_rows);
            });
            *regions_slot.lock().expect("paint handoff") = (tool_spans, notice_rows);
        } else {
            *regions_slot.lock().expect("paint handoff") = (Vec::new(), Vec::new());
        }
    });

    // Bookkeeping (regions/width/msg_area): an overlay-layer 1x1 region —
    // it runs EVERY frame (slt skips deferred draws whose rect collapses
    // to zero, and a satisfied grow does exactly that once the bands fill
    // the viewport) — carrying the grow container's previous-frame rect,
    // one frame stale like the rest of the handoff.
    let (tool_spans, notice_rows) =
        std::mem::take(&mut *regions_out.lock().expect("paint handoff"));
    let paint_for_publish = paint.clone();
    ui.overlay(|ui| {
        ui.container().w(1).h(1).draw(move |_, _rect| {
            publish_frame_slt(
                &paint_for_publish,
                grow_resp.rect,
                &tool_spans,
                &notice_rows,
            );
        });
    });
}

/// Paint pre-wrapped app lines into an slt buffer region: one display row
/// per line, spans left to right, advancing `x` by each span's unicode
/// width. App styles are slt styles already; unset span backgrounds inherit
/// the app background, mirroring how the ratatui Paragraph's style patch
/// kept the root fill under the text. The band's rect is the inner column
/// (the declaring container carries the margins), so `set_string`'s clip
/// trims unwrappable rows exactly where the legacy Paragraph did.
fn paint_rows_slt(
    buf: &mut slt::buffer::Buffer,
    rect: slt::Rect,
    lines: &[Line<'static>],
    links: &[(u16, u16, u16, String)],
    bg: Option<crate::tui::style::Color>,
) {
    use unicode_width::UnicodeWidthStr;

    let x_max = rect.x.saturating_add(rect.width);
    for (dy, line) in lines.iter().enumerate() {
        let y = rect.y + dy as u32;
        let mut x = rect.x;
        for span in &line.spans {
            let mut style = span.style;
            if style.bg.is_none() {
                style.bg = bg;
            }
            if x < x_max {
                buf.set_string(x, y, &span.content, style);
            }
            x = x.saturating_add(span.width() as u32);
        }
        // ratatui parity: a wide grapheme's continuation cell reads as a
        // plain space in the frame dump (ratatui resets the skipped cell);
        // slt leaves it empty-symbol, so normalize it here.
        let mut cx = rect.x;
        while cx < x_max {
            let width = buf
                .try_get(cx, y)
                .map(|c| UnicodeWidthStr::width(c.symbol.as_str()) as u32)
                .unwrap_or(1);
            if width == 2 {
                if let Some(cell) = buf.try_get_mut(cx + 1, y) {
                    cell.set_char(' ');
                }
                cx += 2;
            } else {
                cx += 1;
            }
        }
    }
    for (dy, col_start, col_end, uri) in links {
        let y = rect.y + u32::from(*dy);
        if y >= rect.bottom() {
            continue;
        }
        for col in *col_start..*col_end {
            let x = rect.x + u32::from(col);
            if x >= x_max {
                continue;
            }
            if let Some(cell) = buf.try_get_mut(x, y) {
                apply_link_slt(cell, uri);
            }
        }
    }
}

/// Wrap a linked cell's symbol in OSC-8 and set the first-class
/// [`slt::Cell::hyperlink`] target. slt counterpart of
/// [`hyperlink::apply_to_cell`]: the escape bytes live in the symbol (what
/// the golden fixtures capture byte-for-byte), while `hyperlink` carries
/// the URL for terminals that render OSC 8 natively.
fn apply_link_slt(cell: &mut slt::Cell, uri: &str) {
    let prev = cell.symbol.to_string();
    let wrapped = format!("\u{1b}]8;;{uri}\u{1b}\\{prev}\u{1b}]8;;\u{1b}\\");
    cell.symbol = wrapped.into();
    cell.hyperlink = Some(uri.to_string().into());
}

/// Publish the transcript frame handoff from the leftover region under the
/// last band: its x/width match the transcript area, its bottom is the
/// area's bottom, and the content rows above it are ours, so the area's
/// origin and the inner-relative hit regions resolve exactly.
/// Publish one frame's handoff. `area` is the FULL transcript area (the
/// grow container's rect): regions are offset by the inner margins from
/// its top-left, exactly where the bands painted.
fn publish_frame_slt(
    paint: &std::sync::Arc<std::sync::Mutex<crate::tui::app::PaintData>>,
    area: slt::Rect,
    tool_spans: &[(usize, u16, u16)],
    notice_rows: &[(usize, u16)],
) {
    let clamp16 = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
    let mut p = paint.lock().expect("paint handoff");
    // Frame 1: the grow Response rect is still 0x0 — keep whatever the
    // in-col unsatisfied-grow publish discovered (the true area) and only
    // refresh the regions.
    if area.width > 0 && area.height > 0 {
        p.msg_area = geom::Rect::new(
            clamp16(area.x),
            clamp16(area.y),
            clamp16(area.width),
            clamp16(area.height),
        );
        p.width = clamp16(area.width);
    }
    let area = if area.width > 0 && area.height > 0 {
        area
    } else {
        slt::Rect::new(
            u32::from(p.msg_area.x),
            u32::from(p.msg_area.y),
            u32::from(p.msg_area.width),
            u32::from(p.msg_area.height),
        )
    };
    let inner_x = area.x + u32::from(MARGIN);
    let inner_y = area.y + 1;
    let inner_w = area.width.saturating_sub(u32::from(MARGIN) * 2);
    p.tool_regions = tool_spans
        .iter()
        .map(|&(idx, s, e)| {
            (
                idx,
                geom::Rect::new(
                    clamp16(inner_x),
                    clamp16(inner_y + u32::from(s)),
                    clamp16(inner_w),
                    e - s,
                ),
            )
        })
        .collect();
    p.notice_regions = notice_rows
        .iter()
        .map(|&(idx, r)| {
            (
                idx,
                geom::Rect::new(
                    clamp16(inner_x),
                    clamp16(inner_y + u32::from(r)),
                    clamp16(inner_w),
                    1,
                ),
            )
        })
        .collect();
}

/// Publish the area only, from the leftover region below the bands (the
/// trailing in-col draw that only has room while the grow is unsatisfied —
/// frame 1 before the width handoff settles).
fn publish_area_slt(
    paint: &std::sync::Arc<std::sync::Mutex<crate::tui::app::PaintData>>,
    leftover: slt::Rect,
    total_rows: u32,
) {
    let area = slt::Rect::new(
        leftover.x,
        leftover.y.saturating_sub(total_rows + 1),
        leftover.width,
        leftover.height.saturating_add(total_rows + 1),
    );
    let clamp16 = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
    let mut p = paint.lock().expect("paint handoff");
    p.msg_area = geom::Rect::new(
        clamp16(area.x),
        clamp16(area.y),
        clamp16(area.width),
        clamp16(area.height),
    );
    p.width = clamp16(area.width);
}

/// iTerm2 terminals get the OSC 1337 widget (encoded payload); everything
/// else goes through `kitty_image`, whose own capability probe falls back
/// to halfblock cells. Env-based so tests deterministically take the
/// halfblock path.
fn iterm_terminal() -> bool {
    std::env::var_os("TERM_PROGRAM").is_some_and(|v| v == "iTerm.app")
}

/// Trailing per-frame buffer pass, declared last so it sees the finished
/// cells: the search-highlight fg↔bg swap, the full-screen `frame_text`
/// snapshot, and the current selection as REVERSED cells (the same math as
/// the legacy `render_selection`).
pub(crate) fn overlay_pass_slt(ui: &mut slt::Context, app: &App) {
    let sel = app.view.selection;
    let paint = app.view.paint.clone();
    // The highlight span is recomputed from the doc engine (the same math
    // `build_frame` used this frame); the closure translates it to screen
    // rows through the fresh `msg_area` the transcript just published.
    let hl_rows = {
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        let top_row = layout.doc_row(app.view.scroll);
        highlight_visible(
            app,
            &layout,
            top_row,
            geom::Rect::new(0, 0, app.view.view_width, app.view.view_height),
        )
    };

    ui.overlay(|ui| {
        let _ = ui.container().grow(1).draw(move |buf, _rect| {
            let (inner_x, inner_y, inner_w) = {
                let p = paint.lock().expect("paint handoff");
                let m = p.msg_area;
                (
                    u32::from(m.x + MARGIN),
                    u32::from(m.y + 1),
                    u32::from(m.width.saturating_sub(MARGIN * 2)),
                )
            };

            // Search highlight (F.3): the segment's visible cells swap
            // fg↔bg, mirroring the reference's `Cursor::render(highlight)`.
            if let Some((start, end)) = hl_rows
                && inner_w > 0
            {
                for row in start..end {
                    let y = inner_y + u32::from(row);
                    for col in 0..inner_w {
                        if let Some(cell) = buf.try_get_mut(inner_x + u32::from(col), y) {
                            std::mem::swap(&mut cell.style.fg, &mut cell.style.bg);
                        }
                    }
                }
            }

            // Snapshot the frame as plain text (selection copy extracts from
            // this); link cells carry OSC 8 in their symbols, so strip it.
            let mut frame_text: Vec<String> = Vec::with_capacity(buf.area.height as usize);
            for r in 0..buf.area.height {
                let mut line = String::with_capacity(buf.area.width as usize);
                for c in 0..buf.area.width {
                    let symbol = buf.get(c, r).symbol.as_str();
                    if symbol.contains('\u{1b}') {
                        line.push_str(&hyperlink::strip_osc8(symbol));
                    } else {
                        line.push_str(symbol);
                    }
                }
                frame_text.push(line);
            }

            // The current selection as REVERSED cells: the highlight binds
            // to real text (per-row extents inside the selection's region)
            // so padding, margins, and decoration glyphs never highlight.
            if let Some(sel) = sel
                && !sel.is_empty()
            {
                let ((r1, c1), (r2, c2)) = sel.normalized();
                let region = sel.region;
                for r in r1..=r2.min(
                    u16::try_from(buf.area.height)
                        .unwrap_or(u16::MAX)
                        .saturating_sub(1),
                ) {
                    if r < region.y || r >= region.y + region.height {
                        continue;
                    }
                    let Some(row) = frame_text.get(r as usize) else {
                        continue;
                    };
                    let Some((first, last)) = crate::tui::selection::text_extent(row, region)
                    else {
                        continue;
                    };
                    let (first, last) = (first as u16, last as u16);
                    let row_from = if r == r1 { c1 } else { region.x };
                    let row_to = if r == r2 {
                        c2
                    } else {
                        region.x + region.width - 1
                    };
                    let from = row_from.max(first);
                    let to = row_to.min(last);
                    for c in from..=to {
                        if let Some(cell) = buf.try_get_mut(u32::from(c), u32::from(r)) {
                            cell.style
                                .modifiers
                                .insert(crate::tui::style::Modifiers::REVERSED);
                        }
                    }
                }
            }

            paint.lock().expect("paint handoff").frame_text = frame_text;
        });
    });
}
