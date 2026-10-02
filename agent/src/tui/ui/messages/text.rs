//! Per-kind text renderers: user / assistant / thinking / notice blocks.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::theme;
use super::tools::seg_color;
use super::wrap::{spans_width, wrap_text};
use crate::markdown::Emphasis;
use crate::markdown::render::{self, StyleToken};
use crate::tui::provider::Tone;

fn surface_style() -> Style {
    Style::default().bg(theme::current().bg_surface)
}

pub(super) fn surface_style_with(bg: ratatui::style::Color) -> Style {
    Style::default().bg(bg)
}

/// User-message row: `▎` on the app background, then the tool-card surface
/// background from the cell right of the bar to one cell before the edge.
pub(super) fn user_line(mut spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let target = width.saturating_sub(1);
    let w = spans_width(&spans);
    if w < target {
        spans.push(Span::styled(" ".repeat(target - w), surface_style()));
    }
    Line::from(spans)
}

pub(super) fn user_block(text: &str, width: usize) -> Vec<Line<'static>> {
    let surf = surface_style();
    let mut lines = Vec::new();
    for chunk in wrap_text(text, width.saturating_sub(3)) {
        lines.push(user_line(
            vec![
                Span::styled("▎", Style::default().fg(theme::current().accent)),
                Span::styled(
                    format!(" {chunk}"),
                    Style::default()
                        .fg(theme::current().text_primary)
                        .patch(surf),
                ),
            ],
            width,
        ));
    }
    lines
}

/// Map the markdown engine's semantic tokens onto the TUI theme.
fn md_style(token: &StyleToken, emph: &Emphasis) -> Style {
    let t = theme::current();

    let mut style = match token {
        StyleToken::Text => Style::default().fg(t.text_primary),
        StyleToken::InlineCode => Style::default().fg(t.cyan),
        StyleToken::Highlight {
            fg,
            bold,
            italic,
            underline,
        } => {
            let mut s = Style::default().fg(seg_color(*fg).unwrap_or(t.text_primary));
            if *bold {
                s = s.bold();
            }
            if *italic {
                s = s.italic();
            }
            if *underline {
                s = s.underlined();
            }
            s
        }
        StyleToken::Heading => Style::default().fg(t.accent).bold(),
        StyleToken::CodeBar | StyleToken::ListMarker => Style::default().fg(t.text_tertiary),
        StyleToken::TableBorder | StyleToken::HorizontalRule => {
            Style::default().fg(t.text_tertiary)
        }
    };
    if emph.bold {
        style = style.bold();
    }
    if emph.italic {
        style = style.italic();
    }
    if emph.strike {
        style = style.crossed_out();
    }
    if emph.underline {
        style = style.underlined();
    }
    style
}

/// View-local entries keyed by message position; each stores only the latest
/// exact text and layout. Entries not visited in the current frame are dropped.
#[derive(Default)]
pub(crate) struct AssistantCache {
    entries: std::collections::BTreeMap<usize, AssistantEntry>,
}

struct AssistantEntry {
    text: String,
    width: usize,
    theme: (u64, u64),
    renderer: render::Renderer,
    lines: Vec<Line<'static>>,
    initialized: bool,
    seen: bool,
    #[cfg(test)]
    renders: usize,
    #[cfg(test)]
    resets: usize,
}

impl AssistantCache {
    pub(crate) fn begin_frame(&mut self) {
        for entry in self.entries.values_mut() {
            entry.seen = false;
        }
    }

    pub(crate) fn end_frame(&mut self) {
        self.entries.retain(|_, entry| entry.seen);
    }

    pub(crate) fn render(&mut self, id: usize, text: &str, width: usize) -> Vec<Line<'static>> {
        self.render_with_theme(
            id,
            text,
            width,
            (
                theme::generation(),
                crate::markdown::highlight::theme_generation(),
            ),
        )
    }

    fn render_with_theme(
        &mut self,
        id: usize,
        text: &str,
        width: usize,
        theme: (u64, u64),
    ) -> Vec<Line<'static>> {
        let entry = self.entries.entry(id).or_insert_with(|| AssistantEntry {
            text: String::new(),
            width,
            theme,
            renderer: render::Renderer::streaming_wrapped(),
            lines: Vec::new(),
            initialized: false,
            seen: false,
            #[cfg(test)]
            renders: 0,
            #[cfg(test)]
            resets: 0,
        });
        let invalid =
            entry.width != width || entry.theme != theme || !text.starts_with(&entry.text);
        if invalid {
            entry.renderer = render::Renderer::streaming_wrapped();
            #[cfg(test)]
            {
                entry.resets += 1;
            }
        }
        if invalid || entry.text != text || !entry.initialized {
            entry.lines = styled_markdown(entry.renderer.render(text, width.min(88) as u16));
            entry.initialized = true;
            entry.text.clear();
            entry.text.push_str(text);
            entry.width = width;
            entry.theme = theme;
            #[cfg(test)]
            {
                entry.renders += 1;
            }
        }
        entry.seen = true;
        entry.lines.clone()
    }
}

fn styled_markdown(lines: Vec<render::Line>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| {
            Line::from(
                line.spans
                    .into_iter()
                    .map(|s| Span::styled(s.text, md_style(&s.style, &s.emphasis)))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

pub(super) fn thinking_block(text: &str, width: usize) -> Vec<Line<'static>> {
    let wrap_w = width.min(88);
    wrap_text(text, wrap_w)
        .into_iter()
        .map(|chunk| {
            Line::from(Span::styled(
                chunk,
                Style::default().fg(theme::current().text_secondary),
            ))
        })
        .collect()
}

/// A system notice: one muted line (wrapped when long) with a tone-colored
/// prefix glyph, without card chrome.
pub(super) fn notice_block(tone: Tone, text: &str, width: usize) -> Vec<Line<'static>> {
    let wrap_w = width.saturating_sub(4).clamp(1, 88);
    wrap_text(text, wrap_w)
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            Line::from(vec![
                Span::styled(
                    if i == 0 { "◆ " } else { "  " },
                    Style::default().fg(theme::current().tone_color(tone)),
                ),
                Span::styled("  ", Style::default()),
                Span::styled(chunk, Style::default().fg(theme::current().text_tertiary)),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected(text: &str, width: usize) -> Vec<Line<'static>> {
        styled_markdown(render::render(text, width.min(88) as u16))
    }

    #[test]
    fn assistant_cache_hits_and_retains_incremental_renderer() {
        let mut cache = AssistantCache::default();
        let text =
            "A paragraph with **bold** words and enough text to wrap.\n\n```rust\nfn main() {\n";
        let first = cache.render_with_theme(0, text, 18, (0, 0));
        assert_eq!(first, expected(text, 18));
        assert_eq!(cache.render_with_theme(0, text, 18, (0, 0)), first);
        assert_eq!(cache.entries[&0].renders, 1);
        let grown =
            format!("{text}    println!(\"hello\");\n}}\n```\n\n| a | b |\n|---|---|\n| c | d |");
        assert_eq!(
            cache.render_with_theme(0, &grown, 18, (0, 0)),
            expected(&grown, 18)
        );
        assert_eq!(cache.entries[&0].renders, 2);
        assert_eq!(cache.entries[&0].resets, 0);
    }

    #[test]
    fn assistant_cache_invalidates_replacement_resize_and_themes() {
        let mut cache = AssistantCache::default();
        cache.render_with_theme(0, "old message", 18, (0, 0));
        for (text, width, theme) in [
            (
                "# replacement\n\nnew paragraph wraps across narrow rows",
                18,
                (0, 0),
            ),
            (
                "# replacement\n\nnew paragraph wraps across narrow rows",
                10,
                (0, 0),
            ),
            (
                "# replacement\n\nnew paragraph wraps across narrow rows",
                10,
                (1, 0),
            ),
            (
                "# replacement\n\nnew paragraph wraps across narrow rows",
                10,
                (1, 1),
            ),
        ] {
            assert_eq!(
                cache.render_with_theme(0, text, width, theme),
                expected(text, width)
            );
        }
        assert_eq!(cache.entries[&0].resets, 4);
        cache.render_with_theme(0, "", 10, (1, 1));
        let renders = cache.entries[&0].renders;
        cache.render_with_theme(0, "", 10, (1, 1));
        assert_eq!(cache.entries[&0].renders, renders);
    }

    #[test]
    fn assistant_cache_drops_entries_absent_from_current_session() {
        let mut cache = AssistantCache::default();
        cache.begin_frame();
        cache.render_with_theme(0, "first", 80, (0, 0));
        cache.render_with_theme(2, "second", 80, (0, 0));
        cache.end_frame();
        cache.begin_frame();
        cache.render_with_theme(0, "replacement session", 80, (0, 0));
        cache.end_frame();
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.entries[&0].text, "replacement session");
        cache.begin_frame();
        cache.end_frame();
        assert!(cache.entries.is_empty());
    }

    /// W1: a notice is one muted line with a tone-colored prefix glyph.
    #[test]
    fn notice_renders_tone_glyph_and_muted_text() {
        let t = theme::current();

        use super::{notice_block, theme};
        use crate::tui::app::{App, Message};
        use crate::tui::provider::{AgentEvent, Tone};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // Unit shape: glyph + muted text, continuation lines keep the indent.
        let lines = notice_block(Tone::Warning, "retrying (attempt 1)", 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, "◆ ");
        assert_eq!(lines[0].spans[0].style.fg, Some(t.warning));
        assert_eq!(lines[0].spans[2].style.fg, Some(t.text_tertiary));

        // App level: AgentEvent::Notice lands as a Message::Notice and
        // renders into the transcript.
        let mut app = App::new();
        app.handle_event(AgentEvent::Notice {
            tone: Tone::Danger,
            text: "agent looks stuck in a loop".into(),
        });
        assert!(matches!(
            app.conversation.messages.last(),
            Some(Message::Notice {
                tone: Tone::Danger,
                ..
            })
        ));
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal
            .draw(|f| super::super::render(f, &mut app, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let y = rows
            .iter()
            .position(|r| r.contains("stuck in a loop"))
            .expect("notice text rendered");
        assert!(rows[y].contains('◆'), "tone glyph rendered: {:?}", rows[y]);
        let gx = rows[y].find('◆').unwrap() as u16;
        assert_eq!(buf[(gx, y as u16)].fg, t.danger);
    }
}
