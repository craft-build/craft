//! Message list rendering: user / assistant / tool blocks, plus scrolling.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;
use crate::tui::app::{App, Message};
use crate::tui::hyperlink;
use crate::tui::ui::scrollback::{Layout, ScrollPos, Segment};

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
    let t = theme::current();

    let inner = Rect {
        x: area.x + MARGIN,
        y: area.y + 1,
        width: area.width.saturating_sub(MARGIN * 2),
        height: area.height.saturating_sub(1),
    };
    let width = inner.width as usize;
    if width == 0 {
        return;
    }

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
    // injected into the buffer after layout, once the row is known.
    let mut card_links: Vec<(usize, hyperlink::Hyperlink)> = Vec::new();
    // (segment, caption rows, render state) for inline images; drawn into
    // the buffer after the Paragraph, mirroring the hyperlink injection.
    let mut image_renders: Vec<(
        usize,
        u16,
        std::sync::Arc<crate::tui::ui::image::ImageRenderState>,
    )> = Vec::new();
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
                // covers them.
                // The message loop holds `&app.conversation`; decoding is
                // deferred to after it (the picker cache needs `&mut app`).
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
        let caption_rows = app
            .view
            .segments
            .get(seg)
            .map(|s| s.lines().len().min(u16::MAX as usize) as u16)
            .unwrap_or(0);
        if let Some(state) = app.image_state(&id, &data, inner.width) {
            image_renders.push((seg, caption_rows, state.clone()));
            if let Some(s) = app.view.segments.get_mut(seg) {
                s.set_image(Some(state));
            }
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
    // rows is slicing lines.
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner.height as usize);
    let mut pos = app.view.scroll;
    while lines.len() < inner.height as usize && pos.seg < app.view.segments.len() {
        let seg = app
            .view
            .segments
            .get(pos.seg)
            .expect("bounds checked above");
        let caption = seg.lines().len().min(u16::MAX as usize) as u16;
        let img_rows = seg.image.as_ref().map_or(0, |img| img.rows);
        let capacity = inner.height - lines.len() as u16;
        // Caption rows are real lines; image rows have none, so they are
        // blank-padded to keep following segments below the image. A
        // position can point inside the image region, so clamp the slice
        // start to the caption lines (height() counts image rows too).
        let take_lines = caption.saturating_sub(pos.row).min(capacity);
        let start = (pos.row as usize).min(seg.lines().len());
        lines.extend(seg.lines().range(start, take_lines as usize).cloned());
        let img_start = caption.max(pos.row);
        let img_end = (caption + img_rows).min(pos.row + capacity);
        for _ in img_start..img_end {
            lines.push(Line::default());
        }
        pos.seg += 1;
        pos.row = 0;
    }

    // Visible screen rects of collapsible tool cards (for hover/click),
    // addressed in doc rows so they track the scroll position.
    app.view.tool_regions = tool_seg_ranges
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
            if vis_end <= vis_start || vis_start >= inner.height {
                None
            } else {
                Some((
                    idx,
                    Rect {
                        x: inner.x,
                        y: inner.y + vis_start,
                        width: inner.width,
                        height: vis_end - vis_start,
                    },
                ))
            }
        })
        .collect();

    // Visible one-row rects of "click to expand" notice rows, addressed the
    // same way as card regions so they track the scroll position.
    app.view.notice_regions = notice_seg_rows
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
            (vis < inner.height).then_some((
                idx,
                Rect {
                    x: inner.x,
                    y: inner.y + vis,
                    width: inner.width,
                    height: 1,
                },
            ))
        })
        .collect();

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);

    // Draw inline images over their reserved rows. Like the hyperlink
    // injection this happens after the Paragraph so the layout math is
    // untouched; skipped rows (scrolled out above or below) simply don't
    // draw.
    for (seg, caption_rows, state) in image_renders {
        let doc = layout.doc_row(ScrollPos {
            seg,
            row: caption_rows,
        });
        if doc < top_row {
            continue;
        }
        let vis = clamp_row_u16(doc - top_row);
        if vis >= inner.height {
            continue;
        }
        let height = state.rows.min(inner.height - vis);
        state.render(
            Rect {
                x: inner.x,
                y: inner.y + vis,
                width: inner.width,
                height,
            },
            f,
        );
    }

    // The search modal's current match highlight (F.3): the segment's
    // visible cells are reversed, mirroring the reference's
    // `Cursor::render(..., highlight)`.
    if let Some(seg) = app.view.highlight_segment
        && let Some(s) = app.view.segments.get(seg)
    {
        let start = layout.doc_row(ScrollPos { seg, row: 0 });
        let end = start + u32::from(s.height(inner.width));
        for doc in start.max(top_row)..end.min(top_row + u32::from(inner.height)) {
            let vis = (doc - top_row) as u16;
            for col in 0..inner.width {
                let cell = f
                    .buffer_mut()
                    .cell_mut((inner.x + col, inner.y + vis))
                    .expect("bounded by inner");
                std::mem::swap(&mut cell.fg, &mut cell.bg);
            }
        }
    }

    // Rewrite the linked header cells in place: spans stayed plain text
    // during layout, so wrap math is unaffected. Skipped under tmux, whose
    // passthrough mangles OSC-8. The single-row guard mirrors the
    // reference: a header is one pre-wrapped row, and column bounds keep
    // any overflow from writing outside the viewport.
    if !card_links.is_empty() && !hyperlink::is_muxed() {
        for (seg, hl) in card_links {
            // The header sits one row under the card's top-padding blank.
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
            for col in hl.col_start..hl.col_end {
                let cell = f
                    .buffer_mut()
                    .cell_mut((inner.x + col, inner.y + vis))
                    .expect("col bounded by inner.width, vis by inner.height");
                hyperlink::apply_to_cell(cell, &hl.uri);
            }
        }
    }
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

        // Agent text is markdown: heading rendered in accent bold.
        let hy = rows.iter().position(|r| r.contains("Title")).unwrap();
        let hx = rows[hy].find("Title").unwrap() as u16;
        let c = &buf[(hx, hy as u16)];
        assert_eq!(c.fg, t.accent, "heading fg");
        assert!(c.modifier.contains(Modifier::BOLD), "heading bold");

        // Read card body is syntax highlighted: some source cell carries a
        // color other than the plain secondary/tertiary card text.
        let cy = rows.iter().position(|r| r.contains("fn main() {")).unwrap();
        let hl_found = (0..buf.area.width).any(|x| {
            let c = &buf[(x, cy as u16)];
            matches!(c.symbol(), "f" | "m" | "(" | ")")
                && c.fg != Color::Reset
                && c.fg != t.text_secondary
                && c.fg != t.text_tertiary
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
