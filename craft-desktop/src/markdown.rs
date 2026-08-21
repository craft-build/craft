//! Markdown rendering: `craft_markdown::parse` → Dioxus nodes, with fenced
//! code highlighted via `craft_highlight`.

use craft_markdown::{Block as MdBlock, BlockKind, InlineSpan, SpanKind, parse, parse_inline};
use dioxus::prelude::*;

/// One parsed inline run: plain text or `` `code` ``, with emphasis flags.
#[derive(Clone, PartialEq)]
struct Run {
    text: String,
    code: bool,
    bold: bool,
    italic: bool,
    strike: bool,
}

fn runs_of(spans: &[InlineSpan]) -> Vec<Run> {
    spans
        .iter()
        .map(|s| Run {
            text: s.text.clone(),
            code: s.kind == SpanKind::Code,
            bold: s.emphasis.bold,
            italic: s.emphasis.italic,
            strike: s.emphasis.strike,
        })
        .collect()
}

#[component]
fn RunsView(runs: Vec<Run>) -> Element {
    rsx! {
        for (i, r) in runs.iter().enumerate() {
            if r.code {
                code { key: "c{i}", class: "inline-code", "{r.text}" }
            } else if r.bold && r.italic {
                strong { key: "b{i}", em { "{r.text}" } }
            } else if r.bold {
                strong { key: "b{i}", "{r.text}" }
            } else if r.italic {
                em { key: "i{i}", "{r.text}" }
            } else if r.strike {
                span { key: "k{i}", text_decoration: "line-through", "{r.text}" }
            } else {
                span { key: "t{i}", "{r.text}" }
            }
        }
    }
}

/// Render a markdown string into a column of styled blocks.
#[component]
pub fn Markdown(text: String) -> Element {
    let blocks = parse(&text);
    rsx! {
        div { class: "md",
            for (i, block) in blocks.iter().enumerate() {
                MdBlockView { block: block.clone(), key: "{i}" }
            }
        }
    }
}

#[component]
fn MdBlockView(block: MdBlock) -> Element {
    match block {
        MdBlock::Code { lang, code } => rsx! {
            CodeFence { lang, code }
        },
        MdBlock::Table { rows, header_end } => rsx! {
            table { class: "md-table",
                for (r, row) in rows.iter().enumerate() {
                    tr { key: "r{r}",
                        for (c, cell) in row.iter().enumerate() {
                            if r < header_end {
                                th { key: "h{c}", "{cell}" }
                            } else {
                                td { key: "d{c}", "{cell}" }
                            }
                        }
                    }
                }
            }
        },
        MdBlock::Lines(lines) => rsx! {
            div { class: "md-lines",
                for (j, line) in lines.iter().enumerate() {
                    {match &line.kind {
                        BlockKind::Paragraph => rsx! {
                            p { class: "md-p", key: "p{j}",
                                RunsView { runs: runs_of(&parse_inline(&line.inline)) }
                            }
                        },
                        BlockKind::Heading(level) => rsx! {
                            div { class: "md-h md-h{level}", key: "h{j}",
                                RunsView { runs: runs_of(&parse_inline(&line.inline)) }
                            }
                        },
                        // Body wrapper keeps inline runs (incl. `code` chips) inside a
                        // block container; as direct flex items they would blockify.
                        BlockKind::UnorderedListItem { depth } => rsx! {
                            div { class: "md-li", key: "u{j}", style: format!("padding-left:{}px", *depth * 16 + 8),
                                span { class: "md-bullet", "•" }
                                div { class: "md-li-body", RunsView { runs: runs_of(&parse_inline(&line.inline)) } }
                            }
                        },
                        BlockKind::OrderedListItem { depth, marker } => rsx! {
                            div { class: "md-li", key: "o{j}", style: format!("padding-left:{}px", *depth * 16 + 8),
                                span { class: "md-marker", "{marker}" }
                                div { class: "md-li-body", RunsView { runs: runs_of(&parse_inline(&line.inline)) } }
                            }
                        },
                        BlockKind::HorizontalRule => rsx! {
                            div { class: "md-hr", key: "r{j}" }
                        },
                    }}
                }
            }
        },
    }
}

/// A fenced code block with syntect highlighting. Highlighting runs per
/// render; transcripts are short enough that this stays cheap.
#[component]
pub fn CodeFence(lang: String, code: String) -> Element {
    let mut hl = craft_highlight::Highlighter::for_token(&lang);
    let lines: Vec<Vec<craft_highlight::StyledSegment>> =
        code.lines().map(|l| hl.highlight_line(l)).collect();
    rsx! {
        div { class: "md-code",
            for (i, line) in lines.iter().enumerate() {
                div { class: "md-code-line", key: "{i}",
                    for (j, seg) in line.iter().enumerate() {
                        span {
                            key: "{j}",
                            style: format!("color:rgb({},{},{})", seg.fg.0, seg.fg.1, seg.fg.2),
                            if seg.bold && seg.italic {
                                strong { em { "{seg.text}" } }
                            } else if seg.bold {
                                strong { "{seg.text}" }
                            } else if seg.italic {
                                em { "{seg.text}" }
                            } else {
                                "{seg.text}"
                            }
                        }
                    }
                }
            }
        }
    }
}
