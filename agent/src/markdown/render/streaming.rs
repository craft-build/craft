//! Append-only semantic output cache.
//!
//! Completed prose lines and terminated tables/fences move into a borrowed
//! stable prefix. The unresolved suffix still uses the ordinary parser and
//! renderer: an open fence/table is deliberately reparsed and rewrapped.
//! Fence boundaries and table candidates use the parser's own helpers.

use std::ops::Range;

use super::{Block, Line, RenderCtx, RenderState, Renderer, finalize_lines, parse, render_block};
use crate::markdown::{find_code_fence, is_table_row};

const NORMAL_BLOCK_SENTINEL: &str = "x\n";

/// Consumers append `stable_added` to their styled history and replace their
/// active tail. On `reset`, discard the previous styled history first.
#[derive(Debug)]
pub struct StreamingRenderUpdate {
    pub reset: bool,
    pub stable_added: Range<usize>,
}

/// Caches completed layout without cloning completed semantic history.
///
/// Width changes, syntax theme changes, and non-append replacements invalidate
/// all output. Append detection compares the previous source prefix (O(n)
/// bytes), but parsing/wrapping only visits the unresolved suffix.
pub struct StreamingRenderCache {
    source: String,
    consumed: usize,
    stable: Vec<Line>,
    tail: Vec<Line>,
    pending_blank: Option<Line>,
    renderer: Renderer,
    width: Option<u16>,
    code_idx: usize,
    table_idx: usize,
    #[cfg(test)]
    stable_source_bytes: usize,
    #[cfg(test)]
    parsed_source_bytes: usize,
    #[cfg(test)]
    rendered_prose_lines: usize,
}

impl Default for StreamingRenderCache {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingRenderCache {
    pub fn new() -> Self {
        Self::with_renderer(Renderer::streaming_wrapped())
    }

    pub fn unwrapped() -> Self {
        Self::with_renderer(Renderer::streaming())
    }

    fn with_renderer(renderer: Renderer) -> Self {
        Self {
            source: String::new(),
            consumed: 0,
            stable: Vec::new(),
            tail: Vec::new(),
            pending_blank: None,
            renderer,
            width: None,
            code_idx: 0,
            table_idx: 0,
            #[cfg(test)]
            stable_source_bytes: 0,
            #[cfg(test)]
            parsed_source_bytes: 0,
            #[cfg(test)]
            rendered_prose_lines: 0,
        }
    }

    pub fn stable_lines(&self) -> &[Line] {
        &self.stable
    }

    pub fn tail_lines(&self) -> &[Line] {
        &self.tail
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Move new stable output and the current tail into a consumer's cache.
    /// Previously taken stable lines remain that consumer's responsibility;
    /// the next update still parses only the unresolved source suffix.
    pub fn take_output(&mut self) -> (Vec<Line>, Vec<Line>) {
        (
            std::mem::take(&mut self.stable),
            std::mem::take(&mut self.tail),
        )
    }

    pub fn update(&mut self, text: &str, width: u16) -> StreamingRenderUpdate {
        let theme = super::super::highlight::theme_generation();
        let reset = self.width != Some(width)
            || self.renderer.theme_gen != theme
            || !text.starts_with(&self.source);
        if reset {
            let wrap = self.renderer.wrap_paragraphs;
            *self = Self::with_renderer(if wrap {
                Renderer::streaming_wrapped()
            } else {
                Renderer::streaming()
            });
        }
        let start = self.stable.len();
        self.width = Some(width);
        // Only copy appended bytes, not the completed source.
        self.source.push_str(&text[self.source.len()..]);
        while let Some(end) = stable_unit(&text[self.consumed..]) {
            let source = &text[self.consumed..self.consumed + end];
            let blocks = self.parse_suffix(source.strip_suffix('\n').unwrap_or(source));
            let (raw, code_idx, table_idx) = self.render_blocks(&blocks, width);
            self.code_idx = code_idx;
            self.table_idx = table_idx;
            self.commit(raw);
            self.consumed += end;
            #[cfg(test)]
            {
                self.stable_source_bytes += end;
            }
        }
        let blocks = self.parse_suffix(&text[self.consumed..]);
        let (raw, code_idx, table_idx) = self.render_blocks(&blocks, width);
        self.renderer.highlighters.truncate(code_idx);
        self.renderer.table_col_widths.truncate(table_idx);
        self.tail.clear();
        if let Some(blank) = &self.pending_blank {
            self.tail.push(blank.clone());
        }
        self.tail.extend(raw);
        finalize_lines(&mut self.tail);
        // A pending stable blank is already the first blank at the join.
        StreamingRenderUpdate {
            reset,
            stable_added: start..self.stable.len(),
        }
    }

    fn parse_suffix(&mut self, text: &str) -> Vec<Block> {
        if self.consumed == 0 {
            let text = text.trim_start_matches('\n');
            #[cfg(test)]
            {
                self.parsed_source_bytes += text.len();
            }
            return parse(text);
        }
        // Normal block parsing trims leading newlines. A preceding ordinary
        // line keeps the source join inside the same normal block; remove that
        // sentinel before rendering. The parser still owns trimming before
        // fences/tables, including their intentionally different blank kinds.
        let source = format!("{NORMAL_BLOCK_SENTINEL}{text}");
        #[cfg(test)]
        {
            self.parsed_source_bytes += source.len();
        }
        let mut blocks = parse(&source);
        if let Some(Block::Lines(lines)) = blocks.first_mut() {
            lines.remove(0);
        }
        blocks
    }

    fn render_blocks(&mut self, blocks: &[Block], width: u16) -> (Vec<Line>, usize, usize) {
        let mut raw = Vec::new();
        let mut state = RenderState {
            code_idx: self.code_idx,
            table_idx: self.table_idx,
            highlighters: &mut self.renderer.highlighters,
            table_col_widths: &mut self.renderer.table_col_widths,
            incremental: true,
        };
        let ctx = RenderCtx {
            width,
            wrap_paragraphs: self.renderer.wrap_paragraphs,
        };
        for block in blocks {
            #[cfg(test)]
            if let Block::Lines(lines) = block {
                self.rendered_prose_lines += lines.len();
            }
            render_block(block, &mut raw, &mut state, &ctx);
        }
        (raw, state.code_idx, state.table_idx)
    }

    fn commit(&mut self, raw: Vec<Line>) {
        for line in raw {
            if line.is_blank() {
                if self.pending_blank.is_none() {
                    self.pending_blank = Some(line);
                }
            } else {
                if let Some(blank) = self.pending_blank.take() {
                    self.stable.push(blank);
                }
                self.stable.push(line);
            }
        }
    }
}

/// One irrevocable source unit, including its terminating newline.
/// Hold blank source lines until their following block is known: the parser
/// trims them before a fence/table but retains them between prose lines.
fn stable_unit(text: &str) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if !line.ends_with('\n') {
            return None;
        }
        let content = &line[..line.len() - 1];
        if content.is_empty() {
            offset += line.len();
            continue;
        }
        if find_code_fence(line).is_some() {
            let fence = find_code_fence(&text[offset..])?;
            let end = offset + fence.block_end;
            // A closing line without its newline can still grow into a longer
            // backtick run, undoing the close. Trailing text closes are kept
            // dynamic too because their next block begins mid-source-line.
            return (text.as_bytes().get(end) == Some(&b'\n')).then_some(end + 1);
        }
        if is_table_row(content) {
            offset += line.len();
            for next in text[offset..].split_inclusive('\n') {
                if !next.ends_with('\n') {
                    return None;
                }
                if !is_table_row(&next[..next.len() - 1]) {
                    return Some(offset);
                }
                offset += next.len();
            }
            return None;
        }
        return Some(offset + line.len());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::LineKind;
    use super::*;

    fn assert_prefixes(text: &str, width: u16, wrapped: bool) {
        let mut cache = if wrapped {
            StreamingRenderCache::new()
        } else {
            StreamingRenderCache::unwrapped()
        };
        let mut oracle = if wrapped {
            Renderer::streaming_wrapped()
        } else {
            Renderer::streaming()
        };
        let mut previous = Vec::new();
        for end in (0..=text.len()).filter(|&end| text.is_char_boundary(end)) {
            let update = cache.update(&text[..end], width);
            if !update.reset {
                assert_eq!(&cache.stable_lines()[..previous.len()], previous);
                assert_eq!(update.stable_added.start, previous.len());
            }
            previous = cache.stable_lines().to_vec();
            let actual: Vec<_> = cache
                .stable_lines()
                .iter()
                .chain(cache.tail_lines())
                .cloned()
                .collect();
            assert_eq!(
                actual,
                oracle.render(&text[..end], width),
                "width={width}, wrapped={wrapped}, prefix={:?}",
                &text[..end]
            );
        }
    }

    #[test]
    fn every_utf8_prefix_agrees_with_existing_streaming_renderer() {
        let _theme = crate::markdown::highlight::pin_default_theme_for_tests();
        let corpus = [
            "\n\nhello **世界** 👩‍💻\n\n\n# heading\n- list `code`\n\nend",
            "before\n\n```rust\nfn main() {\n  println!(\"hello\");\n}\n```\n\n\nnext\n",
            "````python\nx = 1\n```\nx = 2\n````\nnext\n```text\nopen\n",
            "before\n\n| a | bb |\n| -- | -- |\n| lengthy value | 界 |\n\nnext\n",
            "| a |\n| b |\n| --- |\n| c |\nnot table\n\n| invalid |\nend\n",
            "```bad`info\nordinary\n```ok\ncode\n```trailing prose\nmore\n",
            "\n\n---\n \n\n```txt\n\n\n```\n\n|a|\n|-|\n\n\nend\n",
            "a\n\n|abc|\n\nb\n\n`````\nx\n````` extra\nend\n",
        ];
        for text in corpus {
            for width in [0, 1, 8, 32, 80] {
                assert_prefixes(text, width, true);
                assert_prefixes(text, width, false);
            }
        }
    }

    #[test]
    fn adversarial_block_combinations_match_every_prefix() {
        let _theme = crate::markdown::highlight::pin_default_theme_for_tests();
        let fragments = [
            "\n",
            "\r\n",
            " \n",
            "# 世\n",
            "```rust\n",
            "````\n",
            "```text`bad\n",
            "``` trailing\n",
            "|a|b|\n",
            "|---|---|\n",
            "|long 世界|x|\n",
            "**bold\n",
            "```\n",
            "text\n",
            "\t\n",
        ];
        let mut seed = 0x12345678_u64;
        for _ in 0..30 {
            let mut text = String::new();
            for _ in 0..12 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                text.push_str(fragments[(seed >> 32) as usize % fragments.len()]);
            }
            assert_prefixes(&text, 17, true);
        }
    }

    #[test]
    fn completed_history_is_not_parsed_or_wrapped_again() {
        let _theme = crate::markdown::highlight::pin_default_theme_for_tests();
        let history = "completed prose with **emphasis**\n".repeat(100);
        let code = "let value = 123; // completed code\n".repeat(50);
        let rows = "|completed table row|世界|\n".repeat(50);
        let mut text = format!("{history}```rust\n{code}```\n|a|b|\n|-|-|\n{rows}end\n");
        let mut cache = StreamingRenderCache::new();
        cache.update(&text, 40);
        assert_eq!(cache.consumed, text.len());
        assert!(cache.stable.iter().any(|line| line.kind == LineKind::Code));
        assert!(
            cache
                .stable
                .iter()
                .any(|line| line.kind == LineKind::TableRow)
        );
        let stable = cache.stable.clone();
        let parsed = cache.stable_source_bytes;
        for ch in "dynamic Unicode 世界 tail".chars() {
            text.push(ch);
            let parsed_before = cache.parsed_source_bytes;
            let wrapped_before = cache.rendered_prose_lines;
            let update = cache.update(&text, 40);
            assert!(!update.reset);
            assert!(update.stable_added.is_empty());
            assert_eq!(cache.stable, stable);
            assert_eq!(cache.stable_source_bytes, parsed);
            assert_eq!(
                cache.parsed_source_bytes - parsed_before,
                text.len() - parsed + NORMAL_BLOCK_SENTINEL.len()
            );
            assert_eq!(cache.rendered_prose_lines - wrapped_before, 1);
        }
        text.push('\n');
        cache.update(&text, 40);
        assert_eq!(cache.stable_source_bytes, text.len());
    }

    #[test]
    fn syntax_theme_generation_invalidates_stable_and_active_output() {
        let _theme = crate::markdown::highlight::pin_default_theme_for_tests();
        let text = "history\n```rust\nfn main() {}\n```\ntail";
        let mut cache = StreamingRenderCache::new();
        cache.update(text, 40);
        let old = cache.stable.clone();
        // Simulate the cache holding an earlier generation without changing
        // the process-global theme underneath concurrently running tests.
        cache.renderer.theme_gen = cache.renderer.theme_gen.wrapping_sub(1);
        let update = cache.update(text, 40);
        assert!(update.reset);
        assert_eq!(update.stable_added, 0..old.len());
        assert_eq!(cache.stable, old);
        let actual: Vec<_> = cache.stable.iter().chain(&cache.tail).cloned().collect();
        assert_eq!(actual, Renderer::streaming_wrapped().render(text, 40));
    }

    #[test]
    fn replacement_and_width_changes_reset_history() {
        let _theme = crate::markdown::highlight::pin_default_theme_for_tests();
        let mut cache = StreamingRenderCache::new();
        for (text, width) in [
            ("old history\nold tail", 40),
            ("replacement\nnew tail", 40),
            ("replacement\nnew tail", 8),
            ("", 8),
            ("new\n", 8),
        ] {
            let update = cache.update(text, width);
            assert!(update.reset || text == "new\n");
            assert_eq!(update.stable_added.start, 0);
            let actual: Vec<_> = cache.stable.iter().chain(&cache.tail).cloned().collect();
            assert_eq!(actual, Renderer::streaming_wrapped().render(text, width));
        }
    }
}
