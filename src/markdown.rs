use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    AnyElement, FontStyle, FontWeight, HighlightStyle, InteractiveText, SharedString,
    StrikethroughStyle, StyledText, UnderlineStyle, div, px, rgb,
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineKind {
    Emphasis,
    Strong,
    Strikethrough,
    Code,
    Link,
}

#[derive(Clone, Debug)]
struct InlineMark {
    kind: InlineKind,
    range: Range<usize>,
    url: Option<String>,
}

#[derive(Clone, Debug)]
enum BlockKind {
    Paragraph,
    Heading(u8),
    Quote(u8),
    ListItem { depth: usize, marker: String },
    Code,
    TableRow,
    Rule,
}

#[derive(Clone, Debug)]
struct MarkdownBlock {
    kind: BlockKind,
    text: String,
    marks: Vec<InlineMark>,
}

struct BlockBuilder {
    kind: BlockKind,
    text: String,
    marks: Vec<InlineMark>,
    open_marks: Vec<(InlineKind, usize, Option<String>)>,
}

impl BlockBuilder {
    fn new(kind: BlockKind) -> Self {
        Self {
            kind,
            text: String::new(),
            marks: vec![],
            open_marks: vec![],
        }
    }

    fn push(&mut self, text: &str) {
        self.text.push_str(text);
    }

    fn mark_text(&mut self, text: &str, kind: InlineKind) {
        let start = self.text.len();
        self.push(text);
        self.marks.push(InlineMark {
            kind,
            range: start..self.text.len(),
            url: None,
        });
    }

    fn start_mark(&mut self, kind: InlineKind, url: Option<String>) {
        self.open_marks.push((kind, self.text.len(), url));
    }

    fn end_mark(&mut self, kind: InlineKind) {
        let Some(index) = self
            .open_marks
            .iter()
            .rposition(|(open_kind, _, _)| *open_kind == kind)
        else {
            return;
        };
        let (_, start, url) = self.open_marks.remove(index);
        if start < self.text.len() {
            self.marks.push(InlineMark {
                kind,
                range: start..self.text.len(),
                url,
            });
        }
    }

    fn finish(self) -> MarkdownBlock {
        MarkdownBlock {
            kind: self.kind,
            text: self.text,
            marks: self.marks,
        }
    }
}

struct ListState {
    next: Option<u64>,
}

fn parse(source: &str) -> Vec<MarkdownBlock> {
    let options = Options::ENABLE_GFM
        | Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH;
    let mut blocks = Vec::new();
    let mut current: Option<BlockBuilder> = None;
    let mut lists: Vec<ListState> = vec![];
    let mut quote_depth = 0_u8;

    let flush = |current: &mut Option<BlockBuilder>, blocks: &mut Vec<MarkdownBlock>| {
        if let Some(builder) = current.take()
            && (!builder.text.is_empty() || matches!(builder.kind, BlockKind::Code))
        {
            blocks.push(builder.finish());
        }
    };

    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(Tag::Paragraph) => {
                let kind = if quote_depth > 0 {
                    BlockKind::Quote(quote_depth)
                } else {
                    BlockKind::Paragraph
                };
                current.get_or_insert_with(|| BlockBuilder::new(kind));
            }
            Event::Start(Tag::Heading { level, .. }) => {
                flush(&mut current, &mut blocks);
                current = Some(BlockBuilder::new(BlockKind::Heading(heading_level(level))));
            }
            Event::Start(Tag::CodeBlock(_)) => {
                flush(&mut current, &mut blocks);
                current = Some(BlockBuilder::new(BlockKind::Code));
            }
            Event::Start(Tag::BlockQuote(_)) => quote_depth = quote_depth.saturating_add(1),
            Event::Start(Tag::List(start)) => lists.push(ListState { next: start }),
            Event::Start(Tag::Item) => {
                flush(&mut current, &mut blocks);
                let marker = match lists.last_mut() {
                    Some(ListState { next: Some(number) }) => {
                        let marker = format!("{number}.");
                        *number += 1;
                        marker
                    }
                    _ => "•".into(),
                };
                current = Some(BlockBuilder::new(BlockKind::ListItem {
                    depth: lists.len().saturating_sub(1),
                    marker,
                }));
            }
            Event::Start(Tag::TableRow) => {
                flush(&mut current, &mut blocks);
                current = Some(BlockBuilder::new(BlockKind::TableRow));
            }
            Event::Start(Tag::TableCell) => {
                let builder = current.get_or_insert_with(|| BlockBuilder::new(BlockKind::TableRow));
                if !builder.text.is_empty() {
                    builder.push("  |  ");
                }
            }
            Event::Start(Tag::Emphasis) => current
                .get_or_insert_with(|| BlockBuilder::new(BlockKind::Paragraph))
                .start_mark(InlineKind::Emphasis, None),
            Event::Start(Tag::Strong) => current
                .get_or_insert_with(|| BlockBuilder::new(BlockKind::Paragraph))
                .start_mark(InlineKind::Strong, None),
            Event::Start(Tag::Strikethrough) => current
                .get_or_insert_with(|| BlockBuilder::new(BlockKind::Paragraph))
                .start_mark(InlineKind::Strikethrough, None),
            Event::Start(Tag::Link { dest_url, .. })
            | Event::Start(Tag::Image { dest_url, .. }) => current
                .get_or_insert_with(|| BlockBuilder::new(BlockKind::Paragraph))
                .start_mark(InlineKind::Link, Some(dest_url.into_string())),
            Event::End(TagEnd::Paragraph)
            | Event::End(TagEnd::Heading(_))
            | Event::End(TagEnd::CodeBlock)
            | Event::End(TagEnd::TableRow) => flush(&mut current, &mut blocks),
            Event::End(TagEnd::BlockQuote(_)) => quote_depth = quote_depth.saturating_sub(1),
            Event::End(TagEnd::List(_)) => {
                flush(&mut current, &mut blocks);
                lists.pop();
            }
            Event::End(TagEnd::Item) => flush(&mut current, &mut blocks),
            Event::End(TagEnd::Emphasis) => {
                if let Some(builder) = current.as_mut() {
                    builder.end_mark(InlineKind::Emphasis);
                }
            }
            Event::End(TagEnd::Strong) => {
                if let Some(builder) = current.as_mut() {
                    builder.end_mark(InlineKind::Strong);
                }
            }
            Event::End(TagEnd::Strikethrough) => {
                if let Some(builder) = current.as_mut() {
                    builder.end_mark(InlineKind::Strikethrough);
                }
            }
            Event::End(TagEnd::Link) | Event::End(TagEnd::Image) => {
                if let Some(builder) = current.as_mut() {
                    builder.end_mark(InlineKind::Link);
                }
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => current
                .get_or_insert_with(|| BlockBuilder::new(BlockKind::Paragraph))
                .push(&text),
            Event::Code(code) | Event::InlineMath(code) => current
                .get_or_insert_with(|| BlockBuilder::new(BlockKind::Paragraph))
                .mark_text(&code, InlineKind::Code),
            Event::DisplayMath(math) => {
                flush(&mut current, &mut blocks);
                let mut builder = BlockBuilder::new(BlockKind::Code);
                builder.push(&math);
                blocks.push(builder.finish());
            }
            Event::SoftBreak => current
                .get_or_insert_with(|| BlockBuilder::new(BlockKind::Paragraph))
                .push(" "),
            Event::HardBreak => current
                .get_or_insert_with(|| BlockBuilder::new(BlockKind::Paragraph))
                .push("\n"),
            Event::Rule => {
                flush(&mut current, &mut blocks);
                blocks.push(MarkdownBlock {
                    kind: BlockKind::Rule,
                    text: String::new(),
                    marks: vec![],
                });
            }
            Event::TaskListMarker(checked) => current
                .get_or_insert_with(|| BlockBuilder::new(BlockKind::Paragraph))
                .push(if checked { "[x] " } else { "[ ] " }),
            Event::FootnoteReference(reference) => current
                .get_or_insert_with(|| BlockBuilder::new(BlockKind::Paragraph))
                .push(&format!("[^{reference}]")),
            Event::Start(_) | Event::End(_) => {}
        }
    }
    flush(&mut current, &mut blocks);
    blocks
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn highlight(mark: &InlineMark) -> HighlightStyle {
    match mark.kind {
        InlineKind::Emphasis => HighlightStyle {
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        },
        InlineKind::Strong => HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        },
        InlineKind::Strikethrough => HighlightStyle {
            strikethrough: Some(StrikethroughStyle {
                thickness: px(1.),
                color: Some(rgb(theme::TEXT_MUTED).into()),
            }),
            ..Default::default()
        },
        InlineKind::Code => HighlightStyle {
            color: Some(rgb(theme::TEXT_SECONDARY).into()),
            background_color: Some(rgb(theme::INPUT_BG).into()),
            ..Default::default()
        },
        InlineKind::Link => HighlightStyle {
            color: Some(rgb(theme::ACCENT).into()),
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(rgb(theme::ACCENT).into()),
                wavy: false,
            }),
            ..Default::default()
        },
    }
}

fn inline_text(block: &MarkdownBlock, id: String) -> AnyElement {
    let styled = StyledText::new(block.text.clone()).with_highlights(
        block
            .marks
            .iter()
            .map(|mark| (mark.range.clone(), highlight(mark))),
    );
    let links = block
        .marks
        .iter()
        .filter_map(|mark| {
            mark.url
                .as_ref()
                .map(|url| (mark.range.clone(), url.clone()))
        })
        .collect::<Vec<_>>();
    if links.is_empty() {
        styled.into_any_element()
    } else {
        let ranges = links.iter().map(|(range, _)| range.clone()).collect();
        let urls = links.into_iter().map(|(_, url)| url).collect::<Vec<_>>();
        InteractiveText::new(SharedString::from(id), styled)
            .on_click(ranges, move |index, _, cx| cx.open_url(&urls[index]))
            .into_any_element()
    }
}

fn block_view(block: MarkdownBlock, id: &str, index: usize) -> AnyElement {
    let inline_id = format!("{id}-inline-{index}");
    match block.kind {
        BlockKind::Paragraph => div()
            .w_full()
            .min_w(px(0.))
            .whitespace_normal()
            .text_size(px(13.))
            .line_height(px(21.))
            .text_color(rgb(theme::TEXT_PRIMARY))
            .child(inline_text(&block, inline_id))
            .into_any_element(),
        BlockKind::Heading(level) => {
            let size = match level {
                1 => 18.,
                2 => 16.,
                3 => 14.,
                _ => 13.,
            };
            div()
                .w_full()
                .min_w(px(0.))
                .whitespace_normal()
                .text_size(px(size))
                .line_height(px(size + 7.))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(inline_text(&block, inline_id))
                .into_any_element()
        }
        BlockKind::Quote(depth) => div()
            .w_full()
            .min_w(px(0.))
            .ml(px((depth.saturating_sub(1) as f32) * 8.))
            .pl(px(10.))
            .border_l_2()
            .border_color(rgb(theme::BORDER))
            .whitespace_normal()
            .text_size(px(12.))
            .line_height(px(19.))
            .text_color(rgb(theme::TEXT_SECONDARY))
            .child(inline_text(&block, inline_id))
            .into_any_element(),
        BlockKind::ListItem { depth, ref marker } => div()
            .w_full()
            .min_w(px(0.))
            .pl(px((depth as f32) * 16.))
            .flex()
            .items_start()
            .gap(px(6.))
            .text_size(px(13.))
            .line_height(px(20.))
            .child(
                div()
                    .w(px(22.))
                    .flex_shrink_0()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(marker.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .whitespace_normal()
                    .text_color(rgb(theme::TEXT_PRIMARY))
                    .child(inline_text(&block, inline_id)),
            )
            .into_any_element(),
        BlockKind::Code => div()
            .id(SharedString::from(format!("{id}-code-{index}")))
            .w_full()
            .min_w(px(0.))
            .overflow_x_scroll()
            .whitespace_nowrap()
            .bg(rgb(theme::TERMINAL_BG))
            .border_1()
            .border_color(rgb(theme::TERMINAL_BORDER))
            .px(px(10.))
            .py(px(8.))
            .text_size(px(12.))
            .line_height(px(18.))
            .text_color(rgb(theme::TEXT_SECONDARY))
            .child(block.text)
            .into_any_element(),
        BlockKind::TableRow => div()
            .w_full()
            .min_w(px(0.))
            .whitespace_normal()
            .border_b_1()
            .border_color(rgb(theme::TERMINAL_BORDER))
            .px(px(8.))
            .py(px(5.))
            .text_size(px(12.))
            .line_height(px(18.))
            .text_color(rgb(theme::TEXT_SECONDARY))
            .child(inline_text(&block, inline_id))
            .into_any_element(),
        BlockKind::Rule => div()
            .w_full()
            .h(px(1.))
            .my(px(4.))
            .bg(rgb(theme::BORDER))
            .into_any_element(),
    }
}

pub fn markdown_view(source: &str, id: impl Into<String>) -> impl IntoElement {
    let id = id.into();
    div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .children(
            parse(source)
                .into_iter()
                .enumerate()
                .map(|(index, block)| block_view(block, &id, index)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_blocks_and_inline_styles() {
        let blocks = parse(
            "# Heading\n\nA **bold** and `code` [link](https://example.com).\n\n- item\n\n> quote\n\n```rs\nfn main() {}\n```",
        );

        assert!(matches!(blocks[0].kind, BlockKind::Heading(1)));
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block.kind, BlockKind::ListItem { .. }))
        );
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block.kind, BlockKind::Quote(_)))
        );
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block.kind, BlockKind::Code))
        );
        assert!(blocks.iter().flat_map(|block| &block.marks).any(|mark| {
            mark.kind == InlineKind::Link && mark.url.as_deref() == Some("https://example.com")
        }));
    }
}
