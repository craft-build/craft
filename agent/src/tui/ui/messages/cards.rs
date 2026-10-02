//! Tool-card layout: header row with wrapping label and right-aligned
//! badge, then the (optionally truncated) body rows inside the card frame.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::theme;
use super::text::surface_style_with;
use super::tools::{diff_badge, styled_diff_spans, tool_label, tool_summary};
use super::wrap::{cell_len, pad_row, spans_width, wrap_spans, wrap_text};
use crate::markdown::highlight::Highlighter;
use crate::tui::app::DiffState;
use crate::tui::hyperlink;
use crate::tui::provider::{LineKind, ToolKind};

const BODY_INDENT: usize = 4;

/// Bodies hiding fewer lines than this render in full; a notice for a
/// couple of hidden lines buys nothing.
const MIN_HIDDEN_LINES: usize = 5;

#[allow(clippy::too_many_arguments)]
pub(super) fn tool_block(
    kind: &ToolKind,
    _id: &str,
    body: &[crate::tui::provider::ToolLine],
    diff: Option<DiffState>,
    focused: bool,
    collapsed: bool,
    hovered: bool,
    body_expanded: bool,
    width: usize,
) -> (
    Vec<Line<'static>>,
    Option<usize>,
    Option<hyperlink::Hyperlink>,
) {
    let t = theme::current();

    // Hovered collapsible cards lift to a slightly lighter background.
    let card_bg = if hovered { t.bg_overlay } else { t.bg_surface };
    let surf = surface_style_with(card_bg);
    let marker = if focused { "▌" } else { " " };
    let marker_style = if focused {
        Style::default().fg(t.accent).bg(card_bg)
    } else {
        surf
    };

    // --- header row ---
    let mut prefix: Vec<Span<'static>> = vec![Span::styled(marker, marker_style)];
    match kind {
        ToolKind::Read { .. } | ToolKind::Grep { .. } | ToolKind::Card { .. } => {
            prefix.push(Span::styled(
                if collapsed { "▸ " } else { "▾ " }.to_string(),
                Style::default().fg(t.text_tertiary).bg(card_bg),
            ));
        }
        ToolKind::Bash { .. } => {
            prefix.push(Span::styled(
                "$ ",
                Style::default().fg(t.text_tertiary).bg(card_bg),
            ));
        }
        ToolKind::Edit { .. } => {
            prefix.push(Span::styled(
                if collapsed { "▸ " } else { "▾ " }.to_string(),
                Style::default().fg(t.text_tertiary).bg(card_bg),
            ));
        }
    }
    let prefix_w = spans_width(&prefix);
    // The label wraps onto continuation rows indented under it, so a long
    // title — a full shell command especially — stays readable instead of
    // clipping at the card edge.
    let label = tool_label(kind);
    let label_style = Style::default().fg(t.text_secondary).bg(card_bg);
    let label_avail = width.saturating_sub(prefix_w).max(1);
    let mut label_rows: Vec<Vec<Span<'static>>> = Vec::new();
    for part in label.split('\n') {
        let mut rows = wrap_spans(
            vec![Span::styled(part.to_string(), label_style)],
            label_avail,
        );
        label_rows.append(&mut rows);
    }
    // OSC-8 link target: the path text inside the header label. Columns
    // count from the row start (marker + caret + label prefix). The row
    // itself is filled in by the renderer once the segment is placed.
    // Edit labels are verb summaries (`deleted: a.txt`), so the path is
    // located by search rather than a fixed prefix.
    let link = match kind {
        ToolKind::Read { path, .. } | ToolKind::Edit { path, .. } => {
            let path_off = if matches!(kind, ToolKind::Read { .. }) {
                Some(cell_len("Read "))
            } else {
                label
                    .find(path.as_str())
                    .map(|byte| label[..byte].chars().count())
            };
            hyperlink::file_uri(path).zip(path_off).map(|(uri, off)| {
                let start = prefix_w + off;
                hyperlink::Hyperlink::new(0, start as u16, (start + cell_len(path)) as u16, uri)
            })
        }
        ToolKind::Grep { .. } | ToolKind::Bash { .. } | ToolKind::Card { .. } => None,
    };
    let blank = |lines: &mut Vec<Line<'static>>| lines.push(pad_row(Vec::new(), width, surf));

    // right-aligned badge / summary
    let badge = diff_badge(diff);
    let right = match (badge, kind) {
        (Some((text, color)), _) => Some((text.to_string(), color)),
        (None, k) if k.collapsible() => Some((tool_summary(k), t.text_tertiary)),
        _ => None,
    };
    let mut lines = Vec::new();
    blank(&mut lines); // top padding
    for (row, label_row) in label_rows.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = if row == 0 {
            prefix.clone()
        } else {
            vec![Span::styled(" ".repeat(prefix_w), surf)]
        };
        spans.extend(label_row.iter().cloned());
        // The badge rides the first row, right-aligned, when it fits beside
        // the label; a wrapped label leaves no room, so it is dropped.
        if row == 0
            && let Some((text, color)) = &right
        {
            let used = spans_width(&spans) + cell_len(text) + 2;
            if used < width {
                spans.push(Span::styled(" ".repeat(width - used), surf));
                spans.push(Span::styled(
                    format!("{text} "),
                    Style::default().fg(*color).bg(card_bg),
                ));
            }
        }
        lines.push(pad_row(spans, width, surf));
    }

    let expanded = !kind.collapsible() || !collapsed;
    if !expanded {
        blank(&mut lines); // bottom padding
        return (lines, None, link);
    }

    fn gutter_span(nr: usize, w: usize, bg: ratatui::style::Color) -> Span<'static> {
        let t = theme::current();
        let text = if nr == 0 {
            " ".repeat(w)
        } else {
            format!("{nr:>w$}")
        };
        Span::styled(
            format!("{text} "),
            Style::default().fg(t.text_tertiary).bg(bg),
        )
    }

    /// One row of a diff body: line-number gutter, +/- sign, and the content
    /// split into word-level emphasis with syntax highlighting patched under
    /// the diff colors.
    #[allow(clippy::too_many_arguments)]
    fn diff_row(
        ln: &crate::tui::provider::ToolLine,
        is_add: bool,
        gutter_w: usize,
        hl: &mut Option<Highlighter>,
        width: usize,
        card_bg: ratatui::style::Color,
        surf: Style,
    ) -> Vec<Line<'static>> {
        let t = theme::current();
        let (sign, fg, bg) = if is_add {
            ("+ ", t.diff_add_text, t.diff_add_bg)
        } else {
            ("- ", t.diff_del_text, t.diff_del_bg)
        };
        let base = Style::default().fg(fg).bg(bg);
        let emph_style = base.bold();

        let segs = hl
            .as_mut()
            .map(|h| h.highlight_line(&ln.text))
            .filter(|s| !s.is_empty());
        let content = styled_diff_spans(&ln.text, segs.as_deref(), &ln.emph, base, emph_style);

        let lead = vec![
            Span::styled(" ".repeat(BODY_INDENT), surf),
            gutter_span(ln.nr, gutter_w, card_bg),
            Span::styled(sign.to_string(), base),
        ];
        let avail = width.saturating_sub(spans_width(&lead)).max(1);
        let mut rows = wrap_spans(content, avail)
            .into_iter()
            .map(|content| {
                let mut spans = [lead.clone(), content].concat();
                let w = spans_width(&spans);
                if w < width {
                    spans.push(Span::styled(" ".repeat(width - w), Style::default().bg(bg)));
                }
                Line::from(spans)
            })
            .collect::<Vec<Line<'static>>>();
        if rows.is_empty() {
            rows.push(pad_row(lead, width, surf));
        }
        rows
    }

    // --- body truncation (ported from the reference's tool display) ---
    let indent = " ".repeat(BODY_INDENT);
    let (cap, keep) = kind.body_hints();
    let hidden = body.len().saturating_sub(cap);
    let truncating = !body_expanded && hidden >= MIN_HIDDEN_LINES;
    let (shown, notice_before) = if truncating {
        match keep {
            crate::tui::provider::Keep::Head => (&body[..cap], false),
            crate::tui::provider::Keep::Tail => (&body[body.len() - cap..], true),
        }
    } else {
        (body, false)
    };
    let notice_text = if truncating {
        Some(format!("\u{2026} ({hidden} lines) click to expand"))
    } else if body_expanded && body.len() > cap {
        // The expanded body keeps a row to fold it back down.
        Some("\u{2026} click to collapse".to_string())
    } else {
        None
    };
    let mut notice_row: Option<usize> = None;
    let mut push_notice = |lines: &mut Vec<Line<'static>>| {
        if let Some(text) = &notice_text {
            notice_row = Some(lines.len());
            lines.push(pad_row(
                vec![
                    Span::styled(indent.clone(), surf),
                    Span::styled(
                        text.clone(),
                        Style::default().fg(t.text_tertiary).bg(card_bg),
                    ),
                ],
                width,
                surf,
            ));
        }
    };
    if notice_before {
        push_notice(&mut lines);
    }

    // --- body rows ---
    let is_edit = matches!(kind, ToolKind::Edit { .. });
    let gutter_w = if is_edit {
        body.iter()
            .map(|ln| ln.nr)
            .max()
            .unwrap_or(0)
            .max(1)
            .ilog10() as usize
            + 1
    } else {
        0
    };
    let mut hl = match kind {
        // Edit diffs and Read file bodies both carry source code; one
        // stateful highlighter walks the card's lines top to bottom.
        ToolKind::Edit { path, .. } | ToolKind::Read { path, .. } => {
            Some(Highlighter::for_path(path))
        }
        _ => None,
    };
    for ln in shown {
        match ln.kind {
            LineKind::Gap if is_edit => lines.push(pad_row(
                vec![
                    Span::styled(indent.clone(), surf),
                    Span::styled(
                        " ...".to_string(),
                        Style::default().fg(t.text_tertiary).bg(card_bg),
                    ),
                ],
                width,
                surf,
            )),
            LineKind::Add if is_edit => lines.append(&mut diff_row(
                ln, true, gutter_w, &mut hl, width, card_bg, surf,
            )),
            LineKind::Del if is_edit => lines.append(&mut diff_row(
                ln, false, gutter_w, &mut hl, width, card_bg, surf,
            )),
            LineKind::Add | LineKind::Del => {
                let (sign, fg, bg) = if ln.kind == LineKind::Add {
                    ("+ ", t.diff_add_text, t.diff_add_bg)
                } else {
                    ("- ", t.diff_del_text, t.diff_del_bg)
                };
                let fill = Style::default().bg(bg);
                let lead = vec![Span::styled(indent.clone(), surf), Span::styled(sign, fill)];
                let content = vec![Span::styled(
                    ln.text.clone(),
                    Style::default().fg(fg).bg(bg),
                )];
                let avail = width.saturating_sub(spans_width(&lead)).max(1);
                let mut rows = wrap_spans(content, avail)
                    .into_iter()
                    .map(|spans| {
                        let mut spans = [lead.clone(), spans].concat();
                        let w = spans_width(&spans);
                        if w < width {
                            spans.push(Span::styled(" ".repeat(width - w), fill));
                        }
                        Line::from(spans)
                    })
                    .collect::<Vec<_>>();
                lines.append(&mut rows);
                continue;
            }
            LineKind::Context => {
                // Source-bearing Context rows (Read cards, edit context)
                // get syntax colors patched over the card surface.
                let base = Style::default().fg(t.text_secondary).bg(card_bg);
                let segs = hl
                    .as_mut()
                    .map(|h| h.highlight_line(&ln.text))
                    .filter(|s| !s.is_empty());
                let mut lead = vec![Span::styled(indent.clone(), surf)];
                if is_edit {
                    lead.push(gutter_span(ln.nr, gutter_w, card_bg));
                    lead.push(Span::styled("  ".to_string(), surf));
                }
                let content = styled_diff_spans(&ln.text, segs.as_deref(), &[], base, base);
                let avail = width.saturating_sub(spans_width(&lead)).max(1);
                let mut rows = wrap_spans(content, avail)
                    .into_iter()
                    .map(|spans| pad_row([lead.clone(), spans].concat(), width, surf))
                    .collect::<Vec<_>>();
                lines.append(&mut rows);
                continue;
            }
            kind => {
                let (prefix, fg) = match kind {
                    LineKind::Cmd => ("$ ", t.text_primary),
                    LineKind::Muted => ("", t.text_tertiary),
                    LineKind::Success => ("", t.success),
                    _ => ("", t.text_secondary),
                };
                let style = Style::default().fg(fg).bg(card_bg);
                let avail = width.saturating_sub(BODY_INDENT + cell_len(prefix)).max(1);
                let wrapped = wrap_text(&ln.text, avail);
                for (i, chunk) in wrapped.iter().enumerate() {
                    let lead = if i == 0 {
                        prefix
                    } else {
                        &" ".repeat(prefix.len())
                    };
                    lines.push(pad_row(
                        vec![
                            Span::styled(indent.clone(), surf),
                            Span::styled(format!("{lead}{chunk}"), style),
                        ],
                        width,
                        surf,
                    ));
                }
                continue;
            }
        }
    }

    if !notice_before {
        push_notice(&mut lines);
    }

    blank(&mut lines); // bottom padding
    (lines, notice_row, link)
}

#[cfg(test)]
mod tests {
    use super::{Line, tool_block};
    use crate::tui::provider::ToolLine;
    use ratatui::style::Modifier;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.clone()).collect()
    }

    #[test]
    fn edit_card_renders_gutter_signs_gap_and_emphasis() {
        let kind = crate::tui::provider::ToolKind::Edit {
            path: "src/a.rs".into(),
            summary: String::new(),
        };
        let body = vec![
            ToolLine {
                kind: crate::tui::provider::LineKind::Del,
                text: "fn a() { one }".into(),
                nr: 2,
                emph: vec![(9, 12)],
            },
            ToolLine {
                kind: crate::tui::provider::LineKind::Add,
                text: "fn a() { two }".into(),
                nr: 0,
                emph: vec![(9, 12)],
            },
            ToolLine::new(crate::tui::provider::LineKind::Gap, "..."),
            ToolLine {
                kind: crate::tui::provider::LineKind::Context,
                text: "}".into(),
                nr: 4,
                ..Default::default()
            },
        ];
        let lines = tool_block(&kind, "t1", &body, None, false, false, false, false, 80).0;
        let joined: Vec<String> = lines.iter().map(line_text).collect();
        // Numbered gutter on removed/context lines, blank on added.
        assert!(
            joined.iter().any(|l| l.contains(" 2 - fn a() { one }")),
            "{joined:?}"
        );
        assert!(
            joined.iter().any(|l| l.contains("   + fn a() { two }")),
            "{joined:?}"
        );
        assert!(joined.iter().any(|l| l.contains(" ...")), "{joined:?}");
        assert!(joined.iter().any(|l| l.contains(" 4   }")), "{joined:?}");
        // Word-level emphasis renders bold.
        let del_line = lines
            .iter()
            .find(|l| line_text(l).contains("- fn a() { one }"))
            .unwrap();
        assert!(del_line.spans.iter().any(|s| {
            s.content.contains("one") && s.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    /// Unchanged (context) rows inside an edit diff are syntax highlighted like
    /// the added/removed rows, not left as plain secondary text.
    #[test]
    fn edit_card_highlights_context_lines() {
        use super::theme;

        let kind = crate::tui::provider::ToolKind::Edit {
            path: "src/a.rs".into(),
            summary: String::new(),
        };
        let body = vec![
            ToolLine::new(crate::tui::provider::LineKind::Context, "let x = 1;"),
            ToolLine {
                kind: crate::tui::provider::LineKind::Add,
                text: "let y = 2;".into(),
                ..Default::default()
            },
        ];
        let lines = tool_block(&kind, "t1", &body, None, false, false, false, false, 80).0;
        let ctx = lines
            .iter()
            .find(|l| line_text(l).contains("let x = 1;"))
            .expect("context row rendered");
        // The `let` keyword carries a syntax color, not the plain secondary fg.
        assert!(
            ctx.spans.iter().any(|s| {
                s.content.contains("let")
                    && s.style.fg.is_some()
                    && s.style.fg != Some(theme::current().text_secondary)
            }),
            "context row is not syntax highlighted: {:?}",
            ctx.spans
        );
    }

    #[test]
    fn edit_card_collapses_to_header_only() {
        let kind = crate::tui::provider::ToolKind::Edit {
            path: "src/a.rs".into(),
            summary: String::new(),
        };
        let body = vec![ToolLine::new(crate::tui::provider::LineKind::Add, "x")];
        let expanded = tool_block(&kind, "t1", &body, None, false, false, false, false, 80).0;
        let collapsed = tool_block(&kind, "t1", &body, None, false, true, false, false, 80).0;
        assert!(expanded.len() > collapsed.len());
        let header: String = collapsed[1]
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect();
        assert!(header.contains("▸"), "collapsed header shows ▸: {header:?}");
        assert!(!header.contains("+ x"));
        let collapsed_text: String = collapsed.iter().map(line_text).collect();
        assert!(!collapsed_text.contains("+ x"));
    }

    #[test]
    fn long_head_kept_body_truncates_with_notice() {
        let kind = crate::tui::provider::ToolKind::Read {
            path: "big.txt".into(),
            summary: String::new(),
        };
        let body: Vec<ToolLine> = (0..60)
            .map(|i| ToolLine::new(crate::tui::provider::LineKind::Context, format!("line {i}")))
            .collect();
        let (lines, notice, _link) =
            tool_block(&kind, "t1", &body, None, false, false, false, false, 80);
        // 40 head lines kept; the notice names the hidden count and sits
        // just above the bottom padding row.
        assert_eq!(notice, Some(lines.len() - 2));
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text.iter().any(|l| l.contains("line 39")), "head kept");
        assert!(
            !text.iter().any(|l| l.contains("line 40")),
            "line past the cap hidden"
        );
        assert!(
            text.iter()
                .any(|l| l.contains("\u{2026} (20 lines) click to expand")),
            "{text:?}"
        );
        // Expanded body renders everything, with a fold-back notice.
        let (lines, notice, _link) =
            tool_block(&kind, "t1", &body, None, false, false, false, true, 80);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            text.iter().any(|l| l.contains("line 59")),
            "full body rendered"
        );
        assert!(text[notice.unwrap()].contains("click to collapse"));
    }

    #[test]
    fn bash_body_keeps_the_tail() {
        let kind = crate::tui::provider::ToolKind::Bash {
            cmd: "cargo test".into(),
        };
        let body: Vec<ToolLine> = (0..60)
            .map(|i| ToolLine::new(crate::tui::provider::LineKind::Context, format!("out {i}")))
            .collect();
        let (lines, notice, _link) =
            tool_block(&kind, "t1", &body, None, false, false, false, false, 80);
        assert!(notice.is_some(), "tail-kept bodies still carry a notice");
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text.iter().any(|l| l.contains("out 59")), "tail kept");
        assert!(!text.iter().any(|l| l.contains("out 0")), "head hidden");
        // The notice sits above the kept tail.
        let notice_idx = notice.unwrap();
        assert!(text[notice_idx].contains("click to expand"));
        assert!(text[notice_idx + 1].contains("out 30"));
    }

    #[test]
    fn bodies_hiding_too_few_lines_render_in_full() {
        let kind = crate::tui::provider::ToolKind::Read {
            path: "small.txt".into(),
            summary: String::new(),
        };
        let body: Vec<ToolLine> = (0..43)
            .map(|i| ToolLine::new(crate::tui::provider::LineKind::Context, format!("l {i}")))
            .collect();
        let (lines, notice, _link) =
            tool_block(&kind, "t1", &body, None, false, false, false, false, 80);
        assert_eq!(notice, None, "3 hidden lines buy no notice");
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("l 42"), "full body rendered");
    }

    #[test]
    fn read_and_edit_cards_carry_header_link() {
        let kind = crate::tui::provider::ToolKind::Read {
            path: "src/a.rs".into(),
            summary: String::new(),
        };
        let body = vec![ToolLine::new(crate::tui::provider::LineKind::Context, "x")];
        let (_, _, link) = tool_block(&kind, "t1", &body, None, false, false, false, false, 80);
        let hl = link.expect("read card carries a link");
        assert!(hl.uri.starts_with("file://"), "{}", hl.uri);
        // marker (1) + caret (2) + "Read " (5) precede the path text.
        assert_eq!(hl.col_start, 8);
        assert_eq!(hl.col_end, 8 + "src/a.rs".len() as u16);

        let kind = crate::tui::provider::ToolKind::Edit {
            path: "src/b.rs".into(),
            summary: String::new(),
        };
        let (_, _, link) = tool_block(&kind, "t1", &body, None, false, true, false, false, 80);
        assert!(link.is_some(), "collapsed edit card still links");
    }

    #[test]
    fn bash_and_grep_cards_carry_no_link() {
        for kind in [
            crate::tui::provider::ToolKind::Bash { cmd: "ls".into() },
            crate::tui::provider::ToolKind::Grep {
                pattern: "x".into(),
                summary: String::new(),
            },
        ] {
            let body = vec![ToolLine::new(crate::tui::provider::LineKind::Context, "x")];
            let (_, _, link) = tool_block(&kind, "t1", &body, None, false, false, false, false, 80);
            assert!(link.is_none());
        }
    }

    /// F: long tool-card body rows (e.g. permission-denied reasons, command
    /// output) wrap onto continuation rows instead of overflowing the card.
    #[test]
    fn long_body_lines_wrap_within_the_card() {
        let kind = crate::tui::provider::ToolKind::Bash {
            cmd: "cargo build".into(),
        };
        let long = "permission denied: rm -rf outside the project auto-review: \
                    this looks destructive and was blocked by the guardrail";
        let body = vec![ToolLine::new(crate::tui::provider::LineKind::Muted, long)];
        let (lines, _, _) = tool_block(&kind, "t1", &body, None, false, false, false, false, 40);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        // No row exceeds the card width (minus trailing pad is included).
        assert!(
            text.iter().all(|l| l.chars().count() <= 40),
            "row overflowed: {text:?}"
        );
        let joined = text.join("\n");
        assert!(
            joined.contains("guardrail"),
            "text was clipped, not wrapped"
        );
    }

    /// A long tool title (a full shell command) wraps onto continuation rows
    /// indented under the header instead of clipping at the card edge.
    #[test]
    fn long_tool_title_wraps_within_the_card() {
        let kind = crate::tui::provider::ToolKind::Bash {
            cmd: "cargo test --package craft --lib some::rather::long::module::path".into(),
        };
        let width = 30;
        let (lines, _, _) = tool_block(&kind, "t1", &[], None, false, false, false, false, width);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            text.iter().all(|l| l.chars().count() <= width),
            "title row overflowed: {text:?}"
        );
        let joined = text.join("\n");
        assert!(joined.contains("path"), "title was clipped, not wrapped");
        assert!(
            joined.contains("$ cargo test"),
            "first row keeps the prefix"
        );
    }
}
