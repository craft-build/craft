use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

pub(super) struct RenderCursor {
    skip: u16,
    y: u16,
    bottom: u16,
    viewport: Rect,
}

impl RenderCursor {
    pub fn new(scroll_top: u16, viewport: Rect) -> Self {
        Self {
            skip: scroll_top,
            y: viewport.y,
            bottom: viewport.y + viewport.height,
            viewport,
        }
    }

    pub fn past_bottom(&self) -> bool {
        self.y >= self.bottom
    }

    pub fn render(
        &mut self,
        lines: &[Line<'static>],
        h: u16,
        style: Option<Style>,
        highlight: bool,
        hyperlinks: &[crate::hyperlink::Hyperlink],
        frame: &mut Frame,
    ) {
        if self.skip >= h {
            self.skip -= h;
            return;
        }
        if self.y >= self.bottom {
            return;
        }
        let visible_h = h
            .saturating_sub(self.skip)
            .min(self.bottom.saturating_sub(self.y));
        let seg_area = Rect::new(self.viewport.x, self.y, self.viewport.width, visible_h);
        let mut p = Paragraph::new(lines.to_vec()).wrap(Wrap { trim: false });
        let mut base = style.unwrap_or_default();
        if highlight {
            base = base.add_modifier(Modifier::REVERSED);
        }
        p = p.style(base);
        let scroll = self.skip;
        if scroll > 0 {
            p = p.scroll((scroll, 0));
            self.skip = 0;
        }
        frame.render_widget(p, seg_area);
        if !hyperlinks.is_empty() && !crate::terminal::is_muxed() {
            inject_hyperlinks(hyperlinks, seg_area, scroll, frame);
        }
        self.y += visible_h;
    }

    /// Render an image segment: the caption lines (via Paragraph) then the
    /// image widget below them. Honors scroll like the text path.
    pub fn render_image(
        &mut self,
        img: &crate::image_render::ImageRenderState,
        caption: &[Line<'static>],
        style: Option<Style>,
        frame: &mut Frame,
    ) {
        let caption_h = crate::wrap::total_rows(caption, self.viewport.width);
        let total_h = caption_h.saturating_add(img.rows);
        if self.skip >= total_h {
            self.skip -= total_h;
            return;
        }
        if self.y >= self.bottom {
            return;
        }

        if self.skip < caption_h {
            let vis_cap = caption_h
                .saturating_sub(self.skip)
                .min(self.bottom.saturating_sub(self.y));
            let cap_area = Rect::new(self.viewport.x, self.y, self.viewport.width, vis_cap);
            let mut p = Paragraph::new(caption.to_vec()).wrap(Wrap { trim: false });
            if let Some(s) = style {
                p = p.style(s);
            }
            if self.skip > 0 {
                p = p.scroll((self.skip, 0));
                self.skip = 0;
            }
            frame.render_widget(p, cap_area);
            self.y += vis_cap;
        } else {
            self.skip -= caption_h;
        }

        if self.y >= self.bottom {
            return;
        }
        let vis_rows = img
            .rows
            .saturating_sub(self.skip)
            .min(self.bottom.saturating_sub(self.y));
        if vis_rows == 0 {
            return;
        }
        let img_area = Rect::new(self.viewport.x, self.y, self.viewport.width, vis_rows);
        img.render(img_area, frame);
        self.skip = 0;
        self.y += vis_rows;
    }
}

/// Rewrites the target cells' symbols to wrap them in OSC-8 hyperlink
/// escapes. Runs *after* `Paragraph` has placed the text, so wrap math
/// is unaffected. `scroll` is the segment-local row offset already
/// skipped, used to drop hyperlinks that fall in the scrolled-out region.
///
/// **Single-row invariant**: each `Hyperlink` maps one logical line to
/// one visual row. This holds for the only current producer (tool
/// headers, `line: 0`) because headers are short and rarely wrap. If the
/// target logical line wraps across multiple visual rows the link lands
/// on the first row only — a misplaced-but-safe degradation. The column
/// bounds guard prevents any out-of-bounds write.
fn inject_hyperlinks(
    hyperlinks: &[crate::hyperlink::Hyperlink],
    seg_area: Rect,
    scroll: u16,
    frame: &mut Frame,
) {
    for hl in hyperlinks {
        if (hl.line as u16) < scroll {
            continue;
        }
        let row = seg_area.y + (hl.line as u16).saturating_sub(scroll);
        if row < seg_area.y || row >= seg_area.y + seg_area.height {
            continue;
        }
        for col_off in hl.col_start..hl.col_end {
            let col = seg_area.x + col_off;
            if col < seg_area.x || col >= seg_area.x + seg_area.width {
                continue;
            }
            if let Some(cell) = frame.buffer_mut().cell_mut((col, row)) {
                crate::hyperlink::apply_to_cell(cell, &hl.uri);
            }
        }
    }
}
