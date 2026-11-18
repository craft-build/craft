//! Message list rendering: user / assistant / tool blocks, plus scrolling.

use super::theme;
use crate::config::ToolDisplay;
use crate::tui::app::{App, Message};
use crate::tui::geom;
use crate::tui::provider::{CallState, ToolKind, ToolLine};
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
use self::text::{
    compaction_block, notice_block, thinking_block, thinking_header, tool_group_header,
    tool_oneliner, user_block,
};

/// Transcript side margins. The left one is two cells deeper than the
/// composer's (which sits at 2), so message blocks read as indented under
/// the input column — the design's 20px block inset.
const MARGIN_L: u16 = 4;
const MARGIN_R: u16 = 2;
const MARGINS: u16 = MARGIN_L + MARGIN_R;

/// Doc-row deltas feed screen rows (hit regions, link rows, image
/// blits), which are `u16`. A huge document delta must clamp, not wrap:
/// a wrapped cast drops a far-offscreen card back onto the top of the
/// viewport and misaligns every click target under it.
fn clamp_row_u16(delta: u32) -> u16 {
    u16::try_from(delta).unwrap_or(u16::MAX)
}

// ---------------------------------------------------------------------------
// OSC-8 hyperlinks
// ---------------------------------------------------------------------------

/// OSC-8 link wrapper. `\x1b]8;;` opens, the URI follows, `ST` (`ESC \`)
/// terminates the parameter, then the visible text, then the closer.
const OSC8_OPEN: &str = "\u{1b}]8;;";
const OSC8_CLOSE: &str = "\u{1b}\\";
/// The bare closer (empty target). slt owns OSC-8 emission via
/// `Cell::hyperlink`; the constant stays for the `strip_osc8` tests.
#[cfg_attr(not(test), allow(dead_code))]
const OSC8_EMPTY: &str = "\u{1b}]8;;\u{1b}\\";

/// A hyperlink target: which cells of one display row to wrap, and the URI.
/// `row` addresses the message document's rows (the same coordinate space
/// the scrollback engine's `doc_row` produces); column ranges are within
/// that row. Each entry targets a single row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Hyperlink {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub uri: String,
}

impl Hyperlink {
    pub fn new(row: u16, col_start: u16, col_end: u16, uri: String) -> Self {
        Self {
            row,
            col_start,
            col_end,
            uri,
        }
    }
}

/// Builds a `file://` URI from a path string, resolving `~` and
/// cwd-relative paths to absolute. Returns `None` if the path cannot be
/// made absolute (e.g. empty, or a bare filename with no cwd context).
pub(crate) fn file_uri(path: &str) -> Option<String> {
    let resolved = resolve_path(path)?;
    let canonical = std::fs::canonicalize(&resolved).unwrap_or(resolved);
    Some(uri_from_path(&canonical))
}

fn resolve_path(path: &str) -> Option<std::path::PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        let home = crate::paths::home()?;
        return Some(home.join(rest));
    }
    if trimmed == "~" {
        return crate::paths::home();
    }
    if trimmed.starts_with('/') || (cfg!(windows) && trimmed.get(1..3) == Some(":\\")) {
        return Some(std::path::PathBuf::from(trimmed));
    }
    let cwd = std::env::current_dir().ok()?;
    Some(cwd.join(trimmed))
}

/// Encodes an absolute path into a `file://` URI, percent-encoding
/// bytes that are not unreserved per RFC 3986. Non-existent paths pass
/// through uncanonicalized (a diff may show a not-yet-written file).
fn uri_from_path(path: &std::path::Path) -> String {
    let mut out = String::from("file://");
    for &byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

/// Whether the terminal multiplexer would corrupt OSC-8 passthrough.
/// tmux re-escapes sequences it does not recognize, so skip injection
/// entirely under it.
pub(crate) fn is_muxed() -> bool {
    std::env::var_os("TMUX").is_some()
}

/// Removes OSC-8 hyperlink sequences (`ESC ]8;;…ST`) from a cell symbol,
/// leaving the visible text. Used when snapshotting the frame as plain
/// text so selection extents and clipboard copies stay clean.
pub(crate) fn strip_osc8(symbol: &str) -> String {
    let mut out = String::with_capacity(symbol.len());
    let mut rest = symbol;
    while let Some(i) = rest.find(OSC8_OPEN) {
        out.push_str(&rest[..i]);
        // Skip through the ST terminator that closes the URI params.
        match rest[i..].find(OSC8_CLOSE) {
            Some(end) => rest = &rest[i + end + OSC8_CLOSE.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
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

/// The painter-neutral transcript frame: visible bands plus hit-region and
/// link data, all addressed in inner-rect-relative rows. The trailing
/// overlay pass recomputes the search highlight itself (it must run after
/// the whole frame is painted).
struct FrameBuild {
    bands: Vec<Band>,
    /// (message index, start..end rows) of collapsible tool cards.
    tool_spans: Vec<(usize, u16, u16)>,
    /// (message index, row) of "click to expand" notice rows.
    notice_rows: Vec<(usize, u16)>,
    /// (message index, row) of collapsed thinking headers.
    thinking_rows: Vec<(usize, u16)>,
    /// (message index of the group's first member, row) of tool-group
    /// header rows.
    group_rows: Vec<(usize, u16)>,
    /// (row, col_start, col_end, uri) of card link headers; empty under a
    /// muxed terminal or when every header is off-screen.
    links: Vec<(u16, u16, u16, String)>,
}

/// Hit regions handed from the col closure to the always-run overlay
/// publish: the `FrameBuild` region fields (tool spans, notice rows,
/// thinking rows, group header rows).
type FrameRegions = (
    Vec<(usize, u16, u16)>,
    Vec<(usize, u16)>,
    Vec<(usize, u16)>,
    Vec<(usize, u16)>,
);

/// Per-frame render-walk state shared by the message, card and group
/// renderers: inner width plus the hit-region and deferred-work lists
/// the walk fills.
struct RenderWalk {
    width: usize,
    /// (msg idx, seg start, seg end) of collapsible cards.
    tool_seg_ranges: Vec<(usize, usize, usize)>,
    /// (msg idx, seg, row) of "click to expand" notice rows.
    notice_seg_rows: Vec<(usize, usize, usize)>,
    /// (msg idx, seg, row) of group header rows.
    group_seg_rows: Vec<(usize, usize, usize)>,
    /// (seg, link) of card headers carrying an OSC-8 target.
    card_links: Vec<(usize, Hyperlink)>,
    /// (segment, tool id, base64 payload) deferred until after the walk.
    pending_images: Vec<(usize, String, String)>,
}

/// Counts a group's members for its header: per-kind tallies of
/// running and complete tools, thinking blocks (root compact groups
/// absorb them), failures kept separate (failed members render as full
/// cards), and the live-member count that drives the header's running
/// cue.
#[derive(Clone, Copy, Debug, Default)]
struct GroupTally {
    reads: usize,
    greps: usize,
    commands: usize,
    edits: usize,
    scripts: usize,
    tasks: usize,
    tools: usize,
    thoughts: usize,
    running: usize,
    failed: usize,
}

impl GroupTally {
    /// Tally the members in `messages`. `roots_only` counts root-level
    /// tools and thinking blocks (a root group's header skips its nested
    /// children); nested groups count every member and never contain
    /// thinking.
    fn count(messages: &[Message], roots_only: bool) -> Self {
        let mut t = Self::default();
        for msg in messages {
            match msg {
                Message::Thinking(_) => t.thoughts += 1,
                Message::Tool {
                    kind,
                    parent_call_id,
                    state,
                    ..
                } if !roots_only || parent_call_id.is_none() => match state {
                    CallState::Failed => t.failed += 1,
                    CallState::Running => {
                        t.running += 1;
                        t.count_kind(kind);
                    }
                    CallState::Complete => t.count_kind(kind),
                },
                _ => {}
            }
        }
        t
    }

    fn count_kind(&mut self, kind: &ToolKind) {
        match kind {
            ToolKind::Read { .. } => self.reads += 1,
            ToolKind::Grep { .. } => self.greps += 1,
            ToolKind::Bash { .. } => self.commands += 1,
            ToolKind::Edit { .. } => self.edits += 1,
            ToolKind::Code { .. } => self.scripts += 1,
            ToolKind::Task { .. } => self.tasks += 1,
            ToolKind::Card { .. } => self.tools += 1,
        }
    }
}

/// Render one tool card (message `idx`) exactly as an ungrouped card
/// renders: chrome, notice row, link, hit region, deferred image and
/// auto-review line.
fn push_tool_card(app: &mut App, walk: &mut RenderWalk, idx: usize, nested: bool) {
    let Message::Tool {
        id,
        kind,
        lines: body,
        full,
        diff,
        review,
        image,
        ..
    } = &app.conversation.messages[idx]
    else {
        return;
    };
    let collapsed = app.conversation.collapsed.iter().any(|c| c == id);
    let body_expanded = app.conversation.expanded_bodies.iter().any(|c| c == id);
    let focused = app.conversation.focused == Some(idx);
    let hovered = app.view.hover_tool == Some(idx);
    // An expanded replay card with a verbatim original shows the
    // original; everything else shows the current body.
    let body: &[ToolLine] = if body_expanded {
        full.as_deref().unwrap_or(body)
    } else {
        body
    };
    let (lines, notice_row, link) = tool_block(
        kind,
        id,
        body,
        full.is_some(),
        *diff,
        focused,
        collapsed,
        hovered,
        body_expanded,
        walk.width.saturating_sub(if nested { 2 } else { 0 }),
        nested,
    );
    let seg = app.view.segments.len();
    if let Some(row) = notice_row {
        walk.notice_seg_rows.push((idx, seg, row));
    }
    if let Some(hl) = link {
        walk.card_links.push((seg, hl));
    }
    if kind.collapsible() {
        walk.tool_seg_ranges.push((idx, seg, seg + 1));
    }
    app.view.segments.push(Segment::with_lines(lines));
    // `view_image` results render below the card body, in the same
    // segment so scroll/height math covers them. Decoding is deferred
    // past the walk (the picker cache needs `&mut app`).
    if let Some(data) = image {
        walk.pending_images
            .push((app.view.segments.len(), id.clone(), data.clone()));
    }
    // Auto-review rides just below the card, outside its chrome.
    if let Some(review) = review {
        app.view.segments.push(Segment::with_lines(notice_block(
            review.tone,
            &review.text,
            walk.width,
        )));
    }
}

/// Render the nested (codemode children) group whose first child is
/// message `idx`: one header plus, while expanded, the member cards.
/// Records a `msg_seg_start` entry for every member past the first:
/// each rendered member's own segment when expanded, the position past
/// the group when collapsed. Returns the message index one past the
/// group's last member.
fn push_nested_group(
    app: &mut App,
    walk: &mut RenderWalk,
    idx: usize,
    msg_seg_start: &mut Vec<usize>,
) -> usize {
    let parent = match &app.conversation.messages[idx] {
        Message::Tool {
            parent_call_id: Some(parent),
            ..
        } => parent.clone(),
        _ => return idx,
    };
    let end = app.conversation.child_run_end(idx, &parent);
    let tally = GroupTally::count(&app.conversation.messages[idx..end], false);
    let key = format!("child:{parent}");
    let expanded = app.conversation.group_expanded(&key, tally.running > 0);
    let lines = tool_group_header(&tally, expanded, tally.running > 0, app.status_tick, true);
    walk.group_seg_rows.push((idx, app.view.segments.len(), 0));
    app.view.segments.push(Segment::with_lines(lines));
    if expanded {
        // The caller recorded the first member's start entry; its card
        // renders here with the rest.
        for member in idx..end {
            if member > idx {
                msg_seg_start.push(app.view.segments.len());
            }
            push_tool_card(app, walk, member, true);
        }
    } else {
        for _ in idx + 1..end {
            msg_seg_start.push(app.view.segments.len());
        }
    }
    end
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
    let mut thinking_seg_rows: Vec<(usize, usize, usize)> = Vec::new(); // (msg idx, seg, row)
    let mut walk = RenderWalk {
        width,
        tool_seg_ranges: Vec::new(),
        notice_seg_rows: Vec::new(),
        group_seg_rows: Vec::new(),
        card_links: Vec::new(),
        pending_images: Vec::new(),
    };
    // Index-based walk: group runs are consumed in one step, so every
    // consumed member still gets a start entry (one per message,
    // always) — its own segment when it renders, the position past its
    // group when hidden.
    let mut idx = 0;
    while idx < app.conversation.messages.len() {
        msg_seg_start.push(app.view.segments.len());
        // Group rendering consumes whole runs and hands `&mut app` to the
        // shared card renderer, so it dispatches before the per-message
        // match borrows a message. Each branch owns its trailing blank
        // separator and index advance.
        let group_end = match app.conversation.messages.get(idx) {
            // A task child keeps its dedicated card even under its code
            // parent: the nested group splits around it. Nested style
            // keeps it indented with the parent's run.
            Some(Message::Tool {
                kind: ToolKind::Task { .. },
                parent_call_id: Some(_),
                ..
            }) => {
                push_tool_card(app, &mut walk, idx, true);
                Some(idx + 1)
            }
            // Nested codemode children always render inside a compact
            // group behind one header, regardless of `tool_display`.
            Some(Message::Tool {
                parent_call_id: Some(_),
                ..
            }) => Some(push_nested_group(app, &mut walk, idx, &mut msg_seg_start)),
            _ if app.tool_display == ToolDisplay::Compact
                && matches!(
                    app.conversation.messages.get(idx),
                    Some(Message::Tool { .. } | Message::Thinking(_))
                )
                && app.conversation.compact_run_has_tool(idx) =>
            {
                // Contiguous root tool runs share a group header; in compact
                // display the run also absorbs the thinking blocks
                // interleaved with its tools (a thinking block groups only
                // when tools follow) and splits around always-card calls
                // (`task`, root `Edit` and `Code`), which always render
                // their dedicated cards. The group opens only by a header
                // click (a pin) — while live the header tracks the running
                // count. Within an expanded group image-bearing and failed
                // members keep full cards (their whole output must stay
                // reachable); every other tool collapses to a one-line
                // entry.
                let end = app.conversation.compact_run_end(idx);
                let tally = GroupTally::count(&app.conversation.messages[idx..end], true);
                let key = app.conversation.group_key(idx).unwrap_or_default();
                let active = tally.running > 0 || app.conversation.trailing_thinking_active(end);
                let expanded = app.conversation.group_expanded(&key, false);
                let lines = tool_group_header(&tally, expanded, active, app.status_tick, false);
                walk.group_seg_rows.push((idx, app.view.segments.len(), 0));
                app.view.segments.push(Segment::with_lines(lines));
                if expanded {
                    let mut member = idx;
                    while member < end {
                        let m = &app.conversation.messages[member];
                        if member > idx {
                            msg_seg_start.push(app.view.segments.len());
                        }
                        if matches!(
                            m,
                            Message::Tool {
                                kind: ToolKind::Task { .. },
                                parent_call_id: Some(_),
                                ..
                            }
                        ) {
                            // A task child splits the nested group: its
                            // dedicated card renders directly.
                            push_tool_card(app, &mut walk, member, true);
                            member += 1;
                        } else if matches!(
                            m,
                            Message::Tool {
                                parent_call_id: Some(_),
                                ..
                            }
                        ) {
                            // Nested groups keep their own header (and cards)
                            // directly under their code parent.
                            member = push_nested_group(app, &mut walk, member, &mut msg_seg_start);
                        } else if matches!(
                            m,
                            Message::Tool { image: Some(_), .. }
                                | Message::Tool {
                                    state: CallState::Failed,
                                    ..
                                }
                        ) {
                            push_tool_card(app, &mut walk, member, false);
                            member += 1;
                        } else if let Message::Tool { kind, state, .. } = m {
                            app.view
                                .segments
                                .push(Segment::with_lines(vec![tool_oneliner(
                                    kind, *state, width, false,
                                )]));
                            member += 1;
                        } else if let Message::Thinking(text) = m {
                            // An absorbed thinking block renders as the
                            // standalone block does: a clickable collapsed
                            // header of its own.
                            let tidx = member;
                            let expanded_thinking =
                                app.conversation.expanded_thinking.contains(&tidx);
                            let active_thinking = app.conversation.thinking_active(tidx);
                            let body = app.conversation.visible_text(tidx).unwrap_or(text);
                            let mut tlines = thinking_header(
                                active_thinking,
                                expanded_thinking,
                                app.status_tick,
                            );
                            if expanded_thinking {
                                tlines.extend(thinking_block(body, width));
                            }
                            thinking_seg_rows.push((tidx, app.view.segments.len(), 0));
                            app.view.segments.push(Segment::with_lines(tlines));
                            member += 1;
                        } else {
                            member += 1;
                        }
                    }
                } else {
                    for _ in idx + 1..end {
                        msg_seg_start.push(app.view.segments.len());
                    }
                }
                Some(end)
            }
            _ => None,
        };
        if let Some(end) = group_end {
            idx = end;
            app.view
                .segments
                .push(Segment::with_lines(vec![Line::default()]));
            continue;
        }
        if matches!(
            app.conversation.messages.get(idx),
            Some(Message::Tool { .. })
        ) {
            // Default: every root tool renders its own full card.
            push_tool_card(app, &mut walk, idx, false);
            idx += 1;
            app.view
                .segments
                .push(Segment::with_lines(vec![Line::default()]));
            continue;
        }
        match &app.conversation.messages[idx] {
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
                let expanded = app.conversation.expanded_thinking.contains(&idx);
                let active = app.conversation.thinking_active(idx);
                let text = app.conversation.visible_text(idx).unwrap_or(text);
                let mut lines = thinking_header(active, expanded, app.status_tick);
                if expanded {
                    lines.extend(thinking_block(text, width));
                }
                thinking_seg_rows.push((idx, app.view.segments.len(), 0));
                app.view.segments.push(Segment::with_lines(lines));
            }
            Message::Notice { tone, text } => app
                .view
                .segments
                .push(Segment::with_lines(notice_block(*tone, text, width))),
            Message::Compaction { kind, text } => app
                .view
                .segments
                .push(Segment::with_lines(compaction_block(kind, text, width))),
            // Tool arms are handled above; keep the match exhaustive
            // without duplicating the card renderer here.
            Message::Tool { .. } => {}
        }
        idx += 1;
        app.view
            .segments
            .push(Segment::with_lines(vec![Line::default()]));
    }
    app.view.assistant_cache.end_frame();

    for (seg, id, data) in walk.pending_images {
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
        if img_taken > 0
            && let Some(state) = seg.image.clone()
        {
            bands.push(Band::Image {
                state,
                rows: img_taken,
            });
            taken += img_taken;
        }
        pos.seg += 1;
        pos.row = 0;
    }

    // Visible screen rows of collapsible tool cards (for hover/click),
    // addressed in doc rows so they track the scroll position.
    let tool_spans = walk
        .tool_seg_ranges
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
    let notice_rows = walk
        .notice_seg_rows
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

    // Visible thinking headers, addressed like notice rows.
    let thinking_rows = thinking_seg_rows
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

    // Visible tool-group header rows, addressed like notice rows.
    let group_rows = walk
        .group_seg_rows
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
    // mangles OSC-8. The single-row guard: a header is one pre-wrapped
    // row, and column bounds keep any overflow from
    // writing outside the viewport.
    let mut links = Vec::new();
    if !is_muxed() {
        for (seg, hl) in walk.card_links {
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

    FrameBuild {
        bands,
        tool_spans,
        notice_rows,
        thinking_rows,
        group_rows,
        links,
    }
}

/// The search modal's current match highlight as visible rows: the
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

// ─────────────────────────────────────────────────────────────────────────
// slt transcript painter
// ─────────────────────────────────────────────────────────────────────────

/// Paint the transcript: the visible bands from [`build_frame`] become
/// per-segment children in document order — text runs as fixed-height
/// `.draw()` regions painting app `Line`s via `Buffer::set_string`, image
/// segments as slt image widgets — plus a trailing bookkeeping region that
/// publishes `msg_area` / hit regions / width into [`PaintData`] for the
/// next frame's event handlers and wrap math (one frame stale).
pub(crate) fn render_slt(ui: &mut slt::Context, app: &mut App) {
    // slt lays out after the frame closure returns, so the wrap width can
    // only come from the previous frame's handoff; the bookkeeping draw
    // below republishes the true rect every frame (the parity harness
    // renders twice, so frame 2 is exact). 80 columns is the first-frame
    // fallback.
    let (area_w, stale_h) = {
        let p = app.view.paint.lock().expect("paint handoff");
        (if p.width == 0 { 80 } else { p.width }, p.msg_area.height)
    };
    // The viewport HEIGHT, though, must not be stale: the transcript
    // sits above the todo/files sections and the composer block, and a
    // ctrl+b / ctrl+t toggle (or a composer row count change) reshuffles
    // those heights on this very frame. Bands built with the stale
    // height overflow the shrunk grow(1) area (slt is overflow-by-design)
    // and paint over the sections, composer and status line for a frame.
    // Those sibling heights are pure functions of app state, so budget
    // synchronously; the handoff stays as fallback for the form-open
    // layouts whose heights aren't cheaply derivable.
    let term_h = u16::try_from(ui.height()).unwrap_or(u16::MAX);
    let forms_open = app.overlays.permission_prompt.is_open()
        || app.overlays.question_form.is_open()
        || app.plan_mode.plan_form.is_visible();
    let area_h = if forms_open {
        stale_h
    } else {
        let task_h = u16::from(app.active_task.is_some());
        let todo_w =
            (u32::from(area_w).saturating_sub(super::composer::SIDE_MARGIN * 2) as usize).max(1);
        let todo_h = super::todo::block_height(app, todo_w) as u16;
        let composer_h = super::composer::block_height(app, u32::from(area_w)) as u16;
        term_h.saturating_sub(task_h + todo_h + composer_h)
    };
    let inner = geom::Rect {
        x: 0,
        y: 0,
        width: area_w.saturating_sub(MARGINS),
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
    let regions_out: std::sync::Arc<std::sync::Mutex<FrameRegions>> = std::sync::Arc::new(
        std::sync::Mutex::new((Vec::new(), Vec::new(), Vec::new(), Vec::new())),
    );
    let regions_slot = std::sync::Arc::clone(&regions_out);
    let grow_resp = ui.container().grow(1).col(move |ui| {
        // Top margin row (`inner.y = area.y + 1`).
        ui.container().h(1).draw(|_, _| {});

        let mut total_rows: u32 = 0;
        if let Some(build) = build {
            let FrameBuild {
                bands,
                tool_spans,
                notice_rows,
                thinking_rows,
                group_rows,
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
                                .ml(u32::from(MARGIN_L))
                                .mr(u32::from(MARGIN_R))
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
                                let _ = ui
                                    .container()
                                    .ml(u32::from(MARGIN_L))
                                    .mr(u32::from(MARGIN_R))
                                    .h(u32::from(rows))
                                    .row(|ui| {
                                        if iterm_terminal() {
                                            let file = px.file.clone();
                                            let _ = ui.iterm_image(&file, cols, u32::from(rows));
                                        } else {
                                            let rgba = px.rgba.clone();
                                            let _ = ui.kitty_image(
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
            *regions_slot.lock().expect("paint handoff") =
                (tool_spans, notice_rows, thinking_rows, group_rows);
        } else {
            *regions_slot.lock().expect("paint handoff") =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        }
    });

    // Bookkeeping (regions/width/msg_area): an overlay-layer 1x1 region —
    // it runs EVERY frame (slt skips deferred draws whose rect collapses
    // to zero, and a satisfied grow does exactly that once the bands fill
    // the viewport) — carrying the grow container's previous-frame rect,
    // one frame stale like the rest of the handoff.
    let (tool_spans, notice_rows, thinking_rows, group_rows) =
        std::mem::take(&mut *regions_out.lock().expect("paint handoff"));
    let paint_for_publish = paint.clone();
    let _ = ui.overlay(|ui| {
        ui.container().w(1).h(1).draw(move |_, _rect| {
            publish_frame_slt(
                &paint_for_publish,
                grow_resp.rect,
                &tool_spans,
                &notice_rows,
                &thinking_rows,
                &group_rows,
            );
        });
    });
}

/// Paint pre-wrapped app lines into an slt buffer region: one display row
/// per line, spans left to right, advancing `x` by each span's unicode
/// width. App styles are slt styles already; unset span backgrounds inherit
/// the app background, keeping the root fill under the text. The band's
/// rect is the inner column (the declaring container carries the
/// margins), so `set_string`'s clip trims unwrappable rows at the margins.
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

/// Mark a cell as part of an OSC-8 hyperlink via the first-class
/// [`slt::Cell::hyperlink`] field; the slt backend emits and closes the
/// escape sequence itself (run-coalesced across contiguous linked cells).
/// Escape bytes must NOT be embedded in `cell.symbol`: slt normalizes
/// symbols at the flush boundary and replaces C0/C1 controls (ESC) with
/// U+FFFD, which renders as `�`/`?` garbage instead of the filename.
fn apply_link_slt(cell: &mut slt::Cell, uri: &str) {
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
    thinking_rows: &[(usize, u16)],
    group_rows: &[(usize, u16)],
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
    let inner_x = area.x + u32::from(MARGIN_L);
    let inner_y = area.y + 1;
    let inner_w = area.width.saturating_sub(u32::from(MARGINS));
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
    p.thinking_regions = thinking_rows
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
    p.group_regions = group_rows
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
/// snapshot, and the current selection as REVERSED cells.
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

    let _ = ui.overlay(|ui| {
        ui.container().grow(1).draw(move |buf, _rect| {
            let (inner_x, inner_y, inner_w) = {
                let p = paint.lock().expect("paint handoff");
                let m = p.msg_area;
                (
                    u32::from(m.x + MARGIN_L),
                    u32::from(m.y + 1),
                    u32::from(m.width.saturating_sub(MARGINS)),
                )
            };

            // Search highlight: the segment's visible cells swap fg↔bg.
            if let Some((start, end)) = hl_rows
                && inner_w > 0
            {
                for row in start..end {
                    let y = inner_y + u32::from(row);
                    for col in 0..inner_w {
                        if let Some(cell) = buf.try_get_mut(inner_x + col, y) {
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
                        line.push_str(&strip_osc8(symbol));
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
#[cfg(test)]
mod tests {
    use crate::tui::app::App;
    use crate::tui::provider::{AgentEvent, CallState, ToolLine};

    /// Renders the app through the slt frame (two passes so the layout
    /// handoff settles) and returns the buffer rows, trailing blanks
    /// trimmed.
    fn frame_rows(app: &mut crate::tui::app::App, w: u32, h: u32) -> Vec<String> {
        let mut view = crate::tui::ui::frame::View::default();
        let mut tb = slt::TestBackend::new(w, h);
        tb.render(|ui| crate::tui::ui::frame::frame(ui, app, &mut view));
        tb.render(|ui| crate::tui::ui::frame::frame(ui, app, &mut view));
        (0..h).map(|y| tb.line(y)).collect()
    }
    /// Thinking renders collapsed behind a header: the body stays hidden
    /// until the message is expanded, the header carries the spinner while
    /// the block streams and the settled sigil after.
    #[test]
    fn thinking_blocks_render_collapsed_behind_a_clickable_header() {
        use crate::tui::app::App;
        use crate::tui::provider::AgentEvent;

        let mut app = App::new();
        app.handle_event(AgentEvent::ReasoningDelta(
            "weighing the options carefully".into(),
        ));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("Thinking…")),
            "streaming header: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("weighing")),
            "the body stays hidden while collapsed: {rows:?}"
        );

        app.handle_event(AgentEvent::AssistantEnd);
        let rows = frame_rows(&mut app, 80, 24);
        let header = rows
            .iter()
            .find(|r| r.contains("Thinking"))
            .expect("settled header renders");
        assert!(
            header.contains('✻') && header.contains('▸'),
            "settled collapsed header: {header:?}"
        );

        app.conversation.expanded_thinking.push(0);
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("weighing")),
            "expansion shows the body: {rows:?}"
        );
        let header = rows
            .iter()
            .find(|r| r.contains("Thinking"))
            .expect("header still renders");
        assert!(header.contains('▾'), "expanded chevron: {header:?}");
    }

    #[test]
    fn streaming_frames_share_completed_transcript_lines() {
        use crate::tui::app::App;
        use crate::tui::provider::AgentEvent;
        use std::time::{Duration, Instant};

        let mut app = App::new();
        for _ in 0..100 {
            app.handle_event(AgentEvent::AssistantText(
                "Completed **Markdown** paragraph with `inline code`.".into(),
            ));
        }
        app.handle_event(AgentEvent::AssistantDelta("Streaming reply.".into()));
        let _ = frame_rows(&mut app, 80, 24);
        let cached = app.view.segments.get(0).unwrap().lines().clone();
        let now = Instant::now();
        for elapsed in [16, 32, 48] {
            app.tick_reveal(now + Duration::from_millis(elapsed));
            let _ = frame_rows(&mut app, 80, 24);
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

    /// A `view_image` result renders an inline image under the card
    /// body — the segment grows by the image's cell rows and halfblock
    /// glyphs land in the buffer below the header.
    #[test]
    fn view_image_card_renders_inline_image() {
        use crate::tui::app::App;
        use crate::tui::provider::{AgentEvent, ToolCallData, ToolKind};

        let img = image::RgbaImage::from_pixel(16, 8, image::Rgba([255, 255, 255, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);

        let mut app = App::new();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            parent_call_id: None,
            id: "t1".into(),
            kind: ToolKind::Bash {
                cmd: "view_image shot.png".into(),
            },
            lines: Vec::new(),
            awaiting_approval: false,
            image: Some(b64),
            state: crate::tui::provider::CallState::Complete,
        }));
        let rows = frame_rows(&mut app, 60, 30);

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
        assert!(
            rows.iter()
                .any(|r| r.contains('\u{2580}') || r.contains('\u{2584}')),
            "halfblock glyphs rendered"
        );
    }

    /// Scrolling into a segment's image region must not slice the caption
    /// lines out of range: image rows are blank-padded, not real lines.
    #[test]
    fn scrolling_into_image_rows_does_not_panic() {
        use crate::tui::app::App;
        use crate::tui::provider::{AgentEvent, ToolCallData, ToolKind};

        let img = image::RgbaImage::from_pixel(16, 8, image::Rgba([255, 255, 255, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);

        let mut app = App::new();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            parent_call_id: None,
            id: "t1".into(),
            kind: ToolKind::Bash {
                cmd: "view_image shot.png".into(),
            },
            lines: Vec::new(),
            awaiting_approval: false,
            image: Some(b64),
            state: crate::tui::provider::CallState::Complete,
        }));
        let _ = frame_rows(&mut app, 60, 10);
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
        let _ = frame_rows(&mut app, 60, 10);
    }

    /// Auto-review renders as its own line *under* the tool card; the card
    /// keeps showing the tool's own header and output.
    #[test]
    fn auto_review_renders_under_the_card_not_inside_it() {
        use crate::tui::app::App;
        use crate::tui::provider::{AgentEvent, Tone, ToolCallData, ToolKind};

        let mut app = App::new();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            parent_call_id: None,
            id: "t1".into(),
            kind: ToolKind::Bash {
                cmd: "cargo test".into(),
            },
            lines: Vec::new(),
            awaiting_approval: false,
            image: None,
            state: crate::tui::provider::CallState::Complete,
        }));
        app.handle_event(AgentEvent::AutoReview {
            id: "t1".into(),
            tone: Tone::Success,
            text: "auto-review allow: low — in-project edit".into(),
        });

        let rows = frame_rows(&mut app, 70, 16);
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
        use crate::tui::app::{App, Message};
        use crate::tui::style::Modifiers;
        use crate::tui::ui::theme;

        let t = theme::current();

        let mut app = App::new();
        app.conversation.messages.push(Message::Assistant(
            "# Title\n\nsome **bold** and `code`\n\n```rust\nfn main() {}\n```".into(),
        ));
        let path = std::env::temp_dir().join("probe.rs");
        app.conversation.messages.push(Message::Tool {
            parent_call_id: None,
            id: "t1".into(),
            full: None,
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
            state: crate::tui::provider::CallState::Complete,
        });
        let mut tb = slt::TestBackend::new(80, 20);
        tb.render(|ui| super::render_slt(ui, &mut app));
        tb.render(|ui| super::render_slt(ui, &mut app));
        let rows: Vec<String> = (0..tb.height()).map(|y| tb.line(y)).collect();
        let buf = tb.buffer();

        // Agent text is markdown: heading rendered in accent bold.
        let hy = rows.iter().position(|r| r.contains("Title")).unwrap();
        let hx = rows[hy].find("Title").unwrap() as u32;
        let c = buf.get(hx, hy as u32);
        assert_eq!(c.style.fg, Some(t.accent), "heading fg");
        assert!(c.style.modifiers.contains(Modifiers::BOLD), "heading bold");

        // Read card body is syntax highlighted: some source cell carries a
        // color other than the plain secondary/tertiary card text.
        let cy = rows.iter().position(|r| r.contains("fn main() {")).unwrap();
        let hl_found = (0..tb.width()).any(|x| {
            let c = buf.get(x, cy as u32);
            matches!(c.symbol.as_ref(), "f" | "m" | "(" | ")")
                && c.style.fg.is_some()
                && c.style.fg != Some(t.text_secondary)
                && c.style.fg != Some(t.text_tertiary)
        });
        assert!(
            hl_found,
            "read card code should be syntax colored; row={:?}",
            rows[cy]
        );

        // Card header: native OSC-8 link attached via `Cell::hyperlink`
        // (slt emits the escapes), full path text survives in the symbols,
        // and no cell inside the message area falls back to an unset
        // background.
        let ry = rows.iter().position(|r| r.contains("Read ")).unwrap();
        assert!(rows[ry].contains("probe.rs"));
        assert!(
            (0..tb.width()).any(|x| buf
                .get(x, ry as u32)
                .hyperlink
                .as_deref()
                .is_some_and(|u| u.starts_with("file://"))),
            "header cells must carry a file:// hyperlink"
        );
        for x in 4..78u32 {
            assert!(
                buf.get(x, ry as u32).style.bg.is_some(),
                "unpainted cell at x={x}"
            );
        }
    }

    /// Ctrl+b / ctrl+t toggle the todo/files sections, shifting the
    /// composer and status line on that same frame. The composer overlay
    /// rect and the transcript bands are computed synchronously, so the
    /// toggle frame must be byte-identical to the settled frame.
    #[test]
    fn section_toggle_reflows_without_a_glitch_frame() {
        use crate::tui::app::{App, Message};
        use crate::tui::provider::{PlanItem, Tone, TouchedFile};

        let mut app = App::new();
        for i in 0..40 {
            app.conversation
                .messages
                .push(Message::Assistant(format!("filler line {i}")));
        }
        app.plan = vec![PlanItem {
            label: "step one".into(),
            done: false,
            active: true,
        }];
        app.files = vec![TouchedFile {
            path: "src/a.rs".into(),
            status: "M".into(),
            tone: Tone::Warning,
        }];

        let mut view = crate::tui::ui::frame::View::default();
        let mut tb = slt::TestBackend::new(80, 24);
        let dump = |tb: &slt::TestBackend| {
            (0..tb.height())
                .map(|y| tb.line(y).to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        tb.render(|ui| crate::tui::ui::frame::frame(ui, &mut app, &mut view));
        tb.render(|ui| crate::tui::ui::frame::frame(ui, &mut app, &mut view));
        let before = dump(&tb);

        // The glitch frame: expand the files list — the transcript area
        // shrinks and the composer moves up right now. The FIRST render
        // after the toggle must already match the settled state.
        app.session.files_open = true;
        tb.render(|ui| crate::tui::ui::frame::frame(ui, &mut app, &mut view));
        let toggled = dump(&tb);
        assert!(
            toggled.contains("▾ files 1") && toggled.contains("src/a.rs"),
            "expanded files section rendered"
        );
        assert_ne!(before, toggled, "the toggle must change the layout");
        tb.render(|ui| crate::tui::ui::frame::frame(ui, &mut app, &mut view));
        let settled = dump(&tb);
        assert_eq!(
            toggled, settled,
            "the toggle frame must already be the settled layout"
        );

        // And the shrink direction (collapse again) must be clean too.
        app.session.files_open = false;
        tb.render(|ui| crate::tui::ui::frame::frame(ui, &mut app, &mut view));
        let collapsed = dump(&tb);
        assert_eq!(
            collapsed, before,
            "collapse returns to the folded layout on the first frame"
        );
    }

    /// Like [`transcript_rows`] but returns the backend so tests can
    /// inspect `Cell::hyperlink` (OSC-8 is emitted by the slt backend,
    /// not embedded in symbols).
    fn transcript_backend(app: &mut crate::tui::app::App, w: u32, h: u32) -> slt::TestBackend {
        let mut tb = slt::TestBackend::new(w, h);
        tb.render(|ui| super::render_slt(ui, app));
        tb.render(|ui| super::render_slt(ui, app));
        tb
    }

    /// Whether any cell in the frame carries an OSC-8 hyperlink.
    fn any_linked(tb: &slt::TestBackend) -> bool {
        (0..tb.height()).any(|y| (0..tb.width()).any(|x| tb.buffer().get(x, y).hyperlink.is_some()))
    }

    #[test]
    fn render_injects_osc8_into_visible_header_and_skips_scrolled_out() {
        use crate::tui::app::{App, Message};

        let mut app = App::new();
        let path = std::env::temp_dir().join("osc8_probe.rs");
        app.conversation.messages.push(Message::Tool {
            parent_call_id: None,
            id: "t1".into(),
            full: None,
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
            state: crate::tui::provider::CallState::Complete,
        });
        // Fill the document past the viewport so a scroll below the card
        // survives the layout clamp instead of snapping back to the top.
        for i in 0..15 {
            app.conversation
                .messages
                .push(Message::Assistant(format!("filler {i}")));
        }

        app.view.follow = false;
        let tb = transcript_backend(&mut app, 80, 12);
        let rows: Vec<String> = (0..tb.height()).map(|y| tb.line(y).to_string()).collect();
        let header_row = rows
            .iter()
            .find(|r| r.contains("Read "))
            .expect("header rendered");
        assert!(
            header_row.contains("osc8_probe.rs"),
            "header filename visible: {header_row:?}"
        );
        assert!(any_linked(&tb), "header cells must carry a hyperlink");

        // Scroll the card fully above the viewport: no links anywhere.
        app.view.follow = false;
        app.view.scroll = crate::tui::ui::scrollback::ScrollPos { seg: 1, row: 0 };
        let tb = transcript_backend(&mut app, 80, 12);
        assert!(!any_linked(&tb), "scrolled-out link must not be attached");
        // A link whose column range runs past the viewport is skipped, but
        // its visible text must survive untouched (spans render first;
        // injection only adds escapes to cells it wraps).
        let mut narrow_app = App::new();
        let long = std::env::temp_dir().join("osc8_probe.rs");
        narrow_app.conversation.messages.push(Message::Tool {
            parent_call_id: None,
            id: "t1".into(),
            full: None,
            kind: crate::tui::provider::ToolKind::Read {
                path: format!("{}?padpadpadpadpad", long.display()),
                summary: String::new(),
            },
            lines: vec![ToolLine::new(crate::tui::provider::LineKind::Context, "x")],
            diff: None,
            review: None,
            image: None,
            state: crate::tui::provider::CallState::Complete,
        });
        narrow_app.view.follow = false;
        let tb = transcript_backend(&mut narrow_app, 24, 12);
        let screen = (0..tb.height())
            .map(|y| tb.line(y).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("Read") && screen.contains("/var/folders"),
            "off-screen link's visible text dropped: {screen:?}"
        );
        assert!(!any_linked(&tb));
    }

    // ── hyperlink helpers ──

    use super::{Hyperlink, OSC8_EMPTY, file_uri, strip_osc8};

    #[test]
    fn file_uri_starts_with_scheme_for_absolute() {
        let uri = file_uri("/abs/file.rs").expect("uri");
        assert!(uri.starts_with("file:///abs/file.rs"), "{uri}");
    }

    #[test]
    fn file_uri_resolves_tilde() {
        let uri = file_uri("~/notes.txt").expect("uri");
        let home = crate::paths::home().expect("home");
        let expect = format!("file://{}", home.join("notes.txt").display());
        assert_eq!(uri, expect);
    }

    #[test]
    fn file_uri_percent_encodes_space() {
        let uri = file_uri("/abs/with space.rs").expect("uri");
        assert!(uri.contains("with%20space.rs"), "{uri}");
        assert!(!uri.contains(' '), "{uri}");
    }

    #[test]
    fn file_uri_none_for_empty() {
        assert!(file_uri("").is_none());
        assert!(file_uri("   ").is_none());
    }

    #[test]
    fn strip_osc8_restores_visible_text() {
        let wrapped = format!("{OSC8_EMPTY}x\u{1b}]8;;file:///f\u{1b}\\");
        assert_eq!(strip_osc8(&wrapped), "x");
        assert_eq!(strip_osc8("plain"), "plain");
    }

    #[test]
    fn hyperlink_new_roundtrips() {
        let h = Hyperlink::new(2, 4, 8, "file:///a".into());
        assert_eq!(
            h,
            Hyperlink {
                row: 2,
                col_start: 4,
                col_end: 8,
                uri: "file:///a".into()
            }
        );
    }

    fn code_parent(id: &str) -> AgentEvent {
        AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
            parent_call_id: None,
            id: id.into(),
            kind: crate::tui::provider::ToolKind::Code {
                script: "print(1)".into(),
                summary: String::new(),
                failure: None,
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Running,
        })
    }

    fn read_call(id: &str, parent: Option<&str>, summary: &str, state: CallState) -> AgentEvent {
        AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
            parent_call_id: parent.map(str::to_owned),
            id: id.into(),
            kind: crate::tui::provider::ToolKind::Read {
                path: "src/main.rs".into(),
                summary: summary.into(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state,
        })
    }

    /// Nested codemode children render behind one collapsible group header
    /// with live counters: expanded (cards visible) while children run,
    /// auto-collapsed once they settle, and shown again by a pinned pin.
    #[test]
    fn nested_children_render_behind_a_group_header() {
        let mut app = App::new();
        app.handle_event(code_parent("t1"));
        app.handle_event(read_call("t1:c0", Some("t1"), "", CallState::Running));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("1 read 1 running")),
            "live header while a child streams: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("src/main.rs")),
            "running group shows member cards: {rows:?}"
        );

        // The finish merge settles the group: it auto-collapses to the
        // header.
        app.handle_event(read_call(
            "t1:c0",
            Some("t1"),
            "10 lines",
            CallState::Complete,
        ));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("1 read")),
            "settled header counts: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("src/main.rs")),
            "settled group hides member cards: {rows:?}"
        );

        // A pinned expansion survives the settled state.
        app.conversation.toggle_group("child:t1", false);
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("src/main.rs")),
            "pinned expansion shows member cards: {rows:?}"
        );
    }

    /// The header counts members by kind across their lifetime, and
    /// failures split out of the kind counts (failed members keep full
    /// cards).
    #[test]
    fn nested_group_counts_track_member_states() {
        let mut app = App::new();
        app.handle_event(code_parent("t1"));
        app.handle_event(read_call("t1:c0", Some("t1"), "", CallState::Running));
        app.handle_event(read_call("t1:c1", Some("t1"), "", CallState::Running));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("2 reads 2 running")),
            "{rows:?}"
        );
        // A completed member stays counted under its kind while its
        // sibling still runs.
        app.handle_event(read_call("t1:c0", Some("t1"), "ok", CallState::Complete));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("2 reads 1 running")),
            "one child still running: {rows:?}"
        );
        app.handle_event(read_call("t1:c1", Some("t1"), "boom", CallState::Failed));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("1 read · 1 failed")),
            "{rows:?}"
        );
    }

    /// Compact mode: contiguous root tool runs render behind a group
    /// header that never auto-expands — the header tracks the running
    /// count — and a pinned expansion shows Edit/Code full cards while
    /// read-only tools collapse to one-line entries.
    #[test]
    fn compact_mode_groups_root_tools_and_simplifies_cards() {
        use crate::config::ToolDisplay;
        let mut app = App::new();
        app.tool_display = ToolDisplay::Compact;
        app.handle_event(read_call("r1", None, "10 lines", CallState::Running));
        app.handle_event(AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
            parent_call_id: None,
            id: "r2".into(),
            kind: crate::tui::provider::ToolKind::Bash {
                cmd: "cargo build".into(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Complete,
        }));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter()
                .any(|r| r.contains("1 read · 1 command 1 running")),
            "live header counts members and running: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("src/main.rs")),
            "a running group stays collapsed: {rows:?}"
        );

        // The run settles: the live cue drops, the group stays closed.
        app.handle_event(read_call("r1", None, "10 lines", CallState::Complete));
        let rows = frame_rows(&mut app, 80, 24);
        let header = rows
            .iter()
            .find(|r| r.contains("1 read · 1 command"))
            .expect("settled root header counts by kind");
        assert!(
            !header.contains("running"),
            "settled header drops the running cue: {header:?}"
        );

        // A pinned expansion renders the members: read-only tools as
        // one-line entries, bash without card chrome.
        app.conversation.toggle_group("root:r1", false);
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("src/main.rs · 10 lines")),
            "read-only tools render one-line entries: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("cargo build")),
            "bash one-liner keeps its command: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("$ cargo build")),
            "bash renders without card chrome in compact mode: {rows:?}"
        );
    }

    /// Compact mode never folds a `task` call into a group: the run
    /// splits around its dedicated card, headers count only their own
    /// members, and the card survives when the surrounding groups
    /// collapse.
    #[test]
    fn task_cards_stay_outside_compact_groups() {
        use crate::config::ToolDisplay;
        let task_call = |id: &str, state: CallState| {
            AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
                parent_call_id: None,
                id: id.into(),
                kind: crate::tui::provider::ToolKind::Task {
                    description: "investigate the flake".into(),
                    summary: String::new(),
                },
                lines: vec![],
                awaiting_approval: false,
                image: None,
                state,
            })
        };
        let mut app = App::new();
        app.tool_display = ToolDisplay::Compact;
        app.handle_event(read_call("r1", None, "10 lines", CallState::Running));
        app.handle_event(task_call("t1", CallState::Running));
        app.handle_event(read_call("r2", None, "4 lines", CallState::Running));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter()
                .any(|r| r.contains("» Task investigate the flake")),
            "task renders its dedicated card while running: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("1 read 1 running")),
            "each side group's header tracks its running member: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("src/main.rs")),
            "surrounding groups stay collapsed while members run: {rows:?}"
        );

        // Everything settles: the groups collapse but the task card
        // stays.
        app.handle_event(read_call("r1", None, "10 lines", CallState::Complete));
        app.handle_event(read_call("r2", None, "4 lines", CallState::Complete));
        app.handle_event(task_call("t1", CallState::Complete));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().all(|r| !r.contains("src/main.rs")),
            "settled groups hide their members: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("» Task investigate the flake")),
            "the task card outlives the collapsed groups: {rows:?}"
        );
    }

    /// Like `task`, a root `Edit` never folds into a compact group: the
    /// run splits around its card so file mutations are always visible,
    /// and the card survives the surrounding groups' collapse.
    #[test]
    fn edit_cards_stay_outside_compact_groups() {
        use crate::config::ToolDisplay;
        let edit_call = |id: &str, state: CallState| {
            AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
                parent_call_id: None,
                id: id.into(),
                kind: crate::tui::provider::ToolKind::Edit {
                    path: "src/main.rs".into(),
                    summary: "overwrote (10 bytes)".into(),
                },
                lines: vec![],
                awaiting_approval: false,
                image: None,
                state,
            })
        };
        let mut app = App::new();
        app.tool_display = ToolDisplay::Compact;
        app.handle_event(read_call("r1", None, "10 lines", CallState::Running));
        app.handle_event(edit_call("e1", CallState::Running));
        app.handle_event(read_call("r2", None, "4 lines", CallState::Running));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("Edit src/main.rs")),
            "the edit card renders outside the groups while running: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("1 read · 1 edit")),
            "headers count only their side of the split: {rows:?}"
        );

        // Everything settles: the groups collapse, the mutation stays.
        app.handle_event(read_call("r1", None, "10 lines", CallState::Complete));
        app.handle_event(read_call("r2", None, "4 lines", CallState::Complete));
        app.handle_event(edit_call("e1", CallState::Complete));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().all(|r| !r.contains("Read src/main.rs")),
            "settled groups hide their members: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("Edit src/main.rs")),
            "the edit card outlives the collapsed groups: {rows:?}"
        );
    }

    /// A thinking block followed by a `task` call never groups: without
    /// a groupable tool after it, the thinking block keeps its own
    /// header and the task keeps its card.
    #[test]
    fn thinking_before_a_task_stays_standalone() {
        use crate::config::ToolDisplay;
        let mut app = App::new();
        app.tool_display = ToolDisplay::Compact;
        app.handle_event(AgentEvent::ReasoningDelta("hmm".into()));
        app.handle_event(AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
            parent_call_id: None,
            id: "t1".into(),
            kind: crate::tui::provider::ToolKind::Task {
                description: "investigate the flake".into(),
                summary: String::new(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Running,
        }));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("Thinking")),
            "the thinking block keeps its own header: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("» Task investigate the flake")),
            "the task card follows it: {rows:?}"
        );
    }

    /// A settled root run's header counts members by kind: reads, greps,
    /// commands, and generic Card tools each get their own
    /// (singular/plural) segment.
    #[test]
    fn compact_header_counts_tool_categories() {
        use crate::config::ToolDisplay;
        use crate::tui::provider::ToolCallData;

        let call = |id: &str, kind| {
            AgentEvent::ToolCall(ToolCallData {
                parent_call_id: None,
                id: id.into(),
                kind,
                lines: vec![],
                awaiting_approval: false,
                image: None,
                state: CallState::Complete,
            })
        };
        let mut app = App::new();
        app.tool_display = ToolDisplay::Compact;
        app.handle_event(read_call("r1", None, "10 lines", CallState::Complete));
        app.handle_event(read_call("r2", None, "4 lines", CallState::Complete));
        app.handle_event(call(
            "g1",
            crate::tui::provider::ToolKind::Grep {
                pattern: "todo".into(),
                summary: "2 matches".into(),
            },
        ));
        app.handle_event(call(
            "b1",
            crate::tui::provider::ToolKind::Bash {
                cmd: "cargo build".into(),
            },
        ));
        app.handle_event(call(
            "a1",
            crate::tui::provider::ToolKind::Card {
                cmd: "search q".into(),
                summary: "3 hits".into(),
            },
        ));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter()
                .any(|r| r.contains("2 reads · 1 grep · 1 command · 1 tool")),
            "settled header counts every kind: {rows:?}"
        );
    }

    /// Compact groups absorb the thinking blocks interleaved with their
    /// tools: one header counts them as thoughts and hides their
    /// standalone rendering; a pinned expansion shows them as collapsed
    /// headers inside the group.
    #[test]
    fn compact_group_absorbs_thinking_blocks() {
        use crate::config::ToolDisplay;

        let mut app = App::new();
        app.tool_display = ToolDisplay::Compact;
        app.handle_event(AgentEvent::ReasoningDelta("first thought".into()));
        app.handle_event(read_call("r1", None, "10 lines", CallState::Complete));
        app.handle_event(AgentEvent::ReasoningDelta("second thought".into()));
        app.handle_event(read_call("r2", None, "4 lines", CallState::Complete));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("2 reads · 2 thoughts")),
            "one header counts the whole think-work cycle: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("Thinking")),
            "absorbed blocks hide their standalone headers: {rows:?}"
        );

        app.conversation.toggle_group("root:r1", false);
        let rows = frame_rows(&mut app, 80, 24);
        assert_eq!(
            rows.iter().filter(|r| r.contains("Thinking")).count(),
            2,
            "pinned expansion shows both thinking headers: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("src/main.rs")),
            "tool members render inside the expansion: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("first thought")),
            "thinking bodies stay collapsed behind their headers: {rows:?}"
        );
    }

    /// A thinking block still streaming after a settled run keeps the
    /// group live (bare "running" cue in the header — no tool runs) but
    /// never expands it; the run still collapses behind its header —
    /// thought included — once the turn moves on.
    #[test]
    fn streaming_thinking_keeps_the_group_live_but_collapsed() {
        use crate::config::ToolDisplay;

        let mut app = App::new();
        app.tool_display = ToolDisplay::Compact;
        app.handle_event(read_call("r1", None, "10 lines", CallState::Complete));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().all(|r| !r.contains("Thinking")),
            "settled run starts without thinking: {rows:?}"
        );

        app.handle_event(AgentEvent::ReasoningDelta("next step".into()));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter()
                .any(|r| r.contains("1 read · 1 thought running")),
            "streaming thinking keeps the header live: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("Thinking…")),
            "the streaming block stays hidden behind the collapsed group: {rows:?}"
        );

        // A pin opens the group and reveals the streaming block's own
        // header.
        app.conversation.toggle_group("root:r1", false);
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("Thinking…")),
            "the pinned expansion shows the streaming block: {rows:?}"
        );

        app.handle_event(AgentEvent::AssistantText("done".into()));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("1 read · 1 thought")),
            "settled header counts the thought: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("running")),
            "the live cue drops once the turn moves on: {rows:?}"
        );
    }

    /// A thinking block never followed by a tool stays standalone in
    /// compact display: groups form around tools, not thinking alone.
    #[test]
    fn pure_thinking_turn_stays_standalone_in_compact() {
        use crate::config::ToolDisplay;

        let mut app = App::new();
        app.tool_display = ToolDisplay::Compact;
        app.handle_event(AgentEvent::ReasoningDelta("no tools here".into()));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("Thinking")),
            "an ungrouped thinking block keeps its standalone header: {rows:?}"
        );

        app.handle_event(AgentEvent::AssistantText("answer".into()));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("Thinking")),
            "still standalone after settling: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("thought")),
            "nothing was grouped or counted: {rows:?}"
        );
    }

    /// Cards mode (the default) never groups: thinking blocks between
    /// tool calls keep standalone headers and no group header counts
    /// them.
    #[test]
    fn cards_mode_keeps_thinking_standalone_between_runs() {
        let mut app = App::new();
        app.handle_event(AgentEvent::ReasoningDelta("first".into()));
        app.handle_event(read_call("r1", None, "10 lines", CallState::Complete));
        app.handle_event(AgentEvent::ReasoningDelta("second".into()));
        app.handle_event(read_call("r2", None, "4 lines", CallState::Complete));
        let rows = frame_rows(&mut app, 80, 24);
        assert_eq!(
            rows.iter().filter(|r| r.contains("Thinking")).count(),
            2,
            "both thinking blocks render standalone in cards mode: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("thought")),
            "no group header counts them: {rows:?}"
        );
    }

    /// Compact groups keep full cards for image-bearing and failed
    /// members: the one-line entry has no room for an image or the
    /// failure body, and neither is reachable any other way.
    #[test]
    fn compact_groups_keep_full_cards_for_images_and_failures() {
        use crate::config::ToolDisplay;

        let img = image::RgbaImage::from_pixel(16, 8, image::Rgba([255, 255, 255, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);

        let mut app = App::new();
        app.tool_display = ToolDisplay::Compact;
        app.handle_event(read_call("r0", None, "", CallState::Running));
        app.handle_event(AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
            parent_call_id: None,
            id: "r-img".into(),
            kind: crate::tui::provider::ToolKind::Bash {
                cmd: "view_image shot.png".into(),
            },
            lines: Vec::new(),
            awaiting_approval: false,
            image: Some(b64),
            state: CallState::Complete,
        }));
        app.handle_event(AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
            parent_call_id: None,
            id: "r-fail".into(),
            kind: crate::tui::provider::ToolKind::Bash {
                cmd: "cargo build".into(),
            },
            lines: vec![ToolLine::new(
                crate::tui::provider::LineKind::Del,
                "error: could not compile",
            )],
            awaiting_approval: false,
            image: None,
            state: CallState::Failed,
        }));
        // Running members never auto-expand the group; open it with a
        // pin so the full-card members render.
        app.conversation.toggle_group("root:r0", false);
        let rows = frame_rows(&mut app, 80, 30);

        assert!(
            (0..app.view.segments.len()).any(|i| app
                .view
                .segments
                .get(i)
                .is_some_and(|s| s.image.is_some())),
            "the image member renders through the full-card path"
        );
        assert!(
            rows.iter().any(|r| r.contains("error: could not compile")),
            "the failure body renders: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("$ cargo build")),
            "the failed member keeps card chrome: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("1 failed")),
            "the header still counts the failure: {rows:?}"
        );
    }

    /// Expanded-group members each record their own render position (so
    /// focus scrolls to the right row), and collapsed groups hide their
    /// members from keyboard focus entirely — running included, since
    /// root groups only open by a pin.
    #[test]
    fn compact_groups_record_member_starts_and_exclude_hidden_focus() {
        use crate::config::ToolDisplay;

        let mut app = App::new();
        app.tool_display = ToolDisplay::Compact;
        let bash = AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
            parent_call_id: None,
            id: "b1".into(),
            kind: crate::tui::provider::ToolKind::Bash {
                cmd: "cargo build".into(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Running,
        });
        app.handle_event(bash);
        app.handle_event(read_call("r1", None, "10 lines", CallState::Complete));
        app.handle_event(read_call("r2", None, "4 lines", CallState::Complete));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter()
                .any(|r| r.contains("2 reads · 1 command 1 running")),
            "the running member shows in the header count, not by expanding: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("src/main.rs")),
            "members stay hidden behind the header while it runs: {rows:?}"
        );
        assert!(
            app.focus_targets().is_empty(),
            "hidden members are not focusable"
        );
        assert_eq!(
            app.view.msg_starts[1], app.view.msg_starts[2],
            "unrendered members share the position past the group"
        );

        // Settle the run: nothing changes — the group was never open.
        app.handle_event(AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
            parent_call_id: None,
            id: "b1".into(),
            kind: crate::tui::provider::ToolKind::Bash {
                cmd: "cargo build".into(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Complete,
        }));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().all(|r| !r.contains("src/main.rs")),
            "collapsed group hides members: {rows:?}"
        );

        // A pinned expansion restores the members as focus targets.
        app.conversation.toggle_group("root:b1", false);
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("src/main.rs")),
            "pinned expansion shows members: {rows:?}"
        );
        let starts = app.view.msg_starts.clone();
        assert!(
            starts[0] < starts[1] && starts[1] < starts[2],
            "rendered members record their own rows: {starts:?}"
        );
        assert_eq!(app.focus_targets(), vec![1, 2]);
    }

    /// A root `Code` call is an always-card: it renders its own card
    /// outside any root group, its children nest directly under it,
    /// and the following root tools form their own group.
    #[test]
    fn nested_group_renders_inside_an_expanded_root_group() {
        use crate::config::ToolDisplay;
        let mut app = App::new();
        app.tool_display = ToolDisplay::Compact;
        app.handle_event(code_parent("t1"));
        app.handle_event(read_call("t1:c0", Some("t1"), "", CallState::Running));
        app.handle_event(AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
            parent_call_id: None,
            id: "b1".into(),
            kind: crate::tui::provider::ToolKind::Bash {
                cmd: "cargo build".into(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Running,
        }));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("py Code")),
            "the code parent renders its own card: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("1 read 1 running") && !r.contains("script")),
            "the nested header counts its children and auto-expands while they run: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("src/main.rs")),
            "nested member card visible while the nested group runs: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("1 command 1 running")),
            "the following root tools form their own group: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("cargo build ·")),
            "the root group stays collapsed while it runs: {rows:?}"
        );

        // Everything settles: the nested group auto-collapses, the root
        // group stays closed.
        app.handle_event(read_call(
            "t1:c0",
            Some("t1"),
            "5 lines",
            CallState::Complete,
        ));
        app.handle_event(AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
            parent_call_id: None,
            id: "b1".into(),
            kind: crate::tui::provider::ToolKind::Bash {
                cmd: "cargo build".into(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Complete,
        }));
        app.handle_event(AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
            parent_call_id: None,
            id: "t1".into(),
            kind: crate::tui::provider::ToolKind::Code {
                script: "print(1)".into(),
                summary: "1 call · ok".into(),
                failure: None,
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Complete,
        }));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().all(|r| !r.contains("src/main.rs")),
            "the settled nested group hides its members: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("py Code")),
            "the code card stays outside the groups: {rows:?}"
        );
    }

    /// Like `task` and root `Edit`, a root `Code` call never folds into
    /// a compact group: the run splits around its card.
    #[test]
    fn code_cards_stay_outside_compact_groups() {
        use crate::config::ToolDisplay;
        let mut app = App::new();
        app.tool_display = ToolDisplay::Compact;
        app.handle_event(read_call("r1", None, "10 lines", CallState::Running));
        app.handle_event(code_parent("t1"));
        app.handle_event(read_call("r2", None, "4 lines", CallState::Running));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("py Code")),
            "the code card renders outside the groups while running: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("1 read · 1 script")),
            "headers count only their side of the split: {rows:?}"
        );
    }

    /// A `task` child splits its code parent's nested group — its
    /// dedicated card always renders — while every other child kind
    /// (edits included) stays folded behind the nested header.
    #[test]
    fn task_children_split_the_nested_group() {
        let task_child = AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
            parent_call_id: Some("t1".into()),
            id: "t1:task".into(),
            kind: crate::tui::provider::ToolKind::Task {
                description: "investigate the flake".into(),
                summary: String::new(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Running,
        });
        let edit_child = |state: CallState| {
            AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
                parent_call_id: Some("t1".into()),
                id: "t1:e0".into(),
                kind: crate::tui::provider::ToolKind::Edit {
                    path: "src/lib.rs".into(),
                    summary: "overwrote (5 bytes)".into(),
                },
                lines: vec![],
                awaiting_approval: false,
                image: None,
                state,
            })
        };
        let mut app = App::new();
        app.handle_event(code_parent("t1"));
        app.handle_event(read_call("t1:c0", Some("t1"), "", CallState::Running));
        app.handle_event(task_child);
        app.handle_event(edit_child(CallState::Complete));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter()
                .any(|r| r.contains("» Task investigate the flake")),
            "the task child renders its dedicated card under the code parent: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("src/main.rs")),
            "the running read group before it auto-expands: {rows:?}"
        );
        // The settled edit-only group behind the task stays collapsed.
        assert!(
            rows.iter().all(|r| !r.contains("src/lib.rs")),
            "the settled nested group keeps its edit folded: {rows:?}"
        );

        // Settle everything: both nested groups collapse, the task card
        // stays.
        app.handle_event(read_call(
            "t1:c0",
            Some("t1"),
            "5 lines",
            CallState::Complete,
        ));
        app.handle_event(AgentEvent::ToolCall(crate::tui::provider::ToolCallData {
            parent_call_id: Some("t1".into()),
            id: "t1:task".into(),
            kind: crate::tui::provider::ToolKind::Task {
                description: "investigate the flake".into(),
                summary: "2 lines".into(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Complete,
        }));
        app.handle_event(edit_child(CallState::Complete));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().all(|r| !r.contains("src/main.rs")),
            "the settled read group collapses: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("» Task investigate the flake")),
            "the task child card outlives the nested groups: {rows:?}"
        );
    }

    /// Default mode keeps root tools ungrouped: one card per call, no
    /// group header between them.
    #[test]
    fn default_mode_leaves_root_tools_ungrouped() {
        let mut app = App::new();
        app.handle_event(read_call("r1", None, "10 lines", CallState::Complete));
        app.handle_event(read_call("r2", None, "4 lines", CallState::Complete));
        let rows = frame_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("src/main.rs")),
            "cards render directly: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| !r.contains("complete")),
            "no group header in cards mode: {rows:?}"
        );
    }
}
