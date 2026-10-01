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

/// Agent text: parsed and rendered through the markdown engine (blocks,
/// inline styles, highlighted code fences) at the wrap width. Code-block
/// highlighting hits the global block cache, so re-rendering each frame
/// stays cheap.
pub(super) fn assistant_block(text: &str, width: usize) -> Vec<Line<'static>> {
    let wrap_w = width.min(88) as u16;
    render::render(text, wrap_w)
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
