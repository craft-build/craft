mod render;
mod scroll;
mod segment;
mod selection;
#[cfg(test)]
mod tests;

pub use self::scroll::ScrollPos;

use self::render::RenderCursor;
use self::scroll::{Layout, TailPart};
use self::segment::{Segment, SegmentCache};
use self::selection::parse_batch_inner_id;

use super::render_hints::RenderHintsRegistry;
use super::tool_display::{
    BatchChildState, RenderCtx, ToolLines, append_annotation, append_right_info, assistant_style,
    build_batch_entry_lines, build_instructions_lines, build_tool_lines, done_style, error_style,
    format_timestamp_now, output_limits_from_hints, thinking_style, tool_output_annotation,
    truncate_to_header, user_style,
};
use super::{DisplayMessage, DisplayRole, ToolRole, ToolStatus, code_view::SectionFlags};
use crate::animation::spinner_str;
use crate::components::keybindings::key;
use crate::markdown::{hr_line, plain_lines, text_to_lines, truncate_output};
use crate::render_worker::RenderWorker;
use crate::repaint::{Cadence, Dirty};
use crate::selection::Selection;
use crate::splash::{ColorTransition, Splash};
use crate::theme;
use crate::update;
use crate::wrap;
use craft_config::{ClockFormat, ToolOutputLines, UiConfig};
use craft_lua::{EventHandle, RestoreItem, WinView};
use serde_json::Value;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use super::scrollbar::{self, render_vertical_scrollbar};
use super::streaming_content::StreamingContent;
use craft_agent::{
    BatchToolEntry, BatchToolStatus, BufferSnapshot, InstructionBlock, NO_FILES_FOUND, SharedBuf,
    ToolDoneEvent, ToolOutput, ToolStartEvent,
};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use tracing::warn;

const THINKING_HIDDEN_HEADER: &str = "thinking> ...";
const REFLOW_MARGIN_VIEWPORTS: u32 = 1;
/// Spacer plus content segment, inserted as a pair.
const PAIR_SEGMENTS: usize = 2;

#[derive(Clone, Copy)]
pub struct PromptProgress {
    pub processed: u32,
    pub total: u32,
    pub cache: u32,
}

struct LiveBufEntry {
    buf: Arc<SharedBuf>,
    dirty_seen: bool,
}

pub struct MessagesPanel {
    messages: Vec<DisplayMessage>,
    streaming_thinking: StreamingContent,
    streaming_text: StreamingContent,
    started_at: Instant,
    scroll: ScrollPos,
    auto_scroll: bool,
    viewport_height: u16,
    viewport_width: u16,
    cache: SegmentCache,
    /// The streaming tail the last `view` drew, in the order it drew it. Lets
    /// the row walk and clicks address the tail between frames.
    tail: Vec<(TailPart, u16)>,
    hl_worker: RenderWorker,
    theme_generation: u64,
    highlight_segment: Option<usize>,
    idle_splash: Splash,
    accent: ColorTransition,
    expanded_tools: HashMap<String, SectionFlags>,
    lua_expanded: HashSet<String>,
    live_bufs: HashMap<String, LiveBufEntry>,
    batch_children: HashMap<String, BatchChildState>,
    tool_output_lines: ToolOutputLines,
    render_hints: RenderHintsRegistry,
    pending_restores: Vec<RestoreItem>,
    lua_event_handle: EventHandle,
    restore_event_tx: Option<craft_agent::EventSender>,
    image_picker: crate::image_render::ImagePicker,
    show_thinking: bool,
    thinking_collapsed: bool,
    clock_format: ClockFormat,
    prompt_progress: Option<PromptProgress>,
}

impl MessagesPanel {
    pub fn new(ui_config: UiConfig, lua_event_handle: EventHandle) -> Self {
        let thinking = thinking_style();
        let assistant = assistant_style();
        let ms = ui_config.typewriter_ms_per_char;
        Self {
            messages: Vec::new(),
            streaming_thinking: StreamingContent::new(
                thinking.prefix,
                thinking.text_style,
                thinking.prefix_style,
                ms,
            ),
            streaming_text: StreamingContent::new(
                assistant.prefix,
                assistant.text_style,
                assistant.prefix_style,
                ms,
            ),
            started_at: Instant::now(),
            scroll: ScrollPos::default(),
            auto_scroll: true,
            viewport_height: 24,
            viewport_width: crossterm::terminal::size().map_or(80, |(w, _)| w.saturating_sub(1)),
            cache: SegmentCache::new(),
            tail: Vec::new(),
            hl_worker: RenderWorker::new(),
            theme_generation: theme::generation(),
            highlight_segment: None,
            idle_splash: Splash::new(ui_config.splash_animation),
            accent: ColorTransition::new(theme::current().mode_build),
            expanded_tools: HashMap::new(),
            lua_expanded: HashSet::new(),
            live_bufs: HashMap::new(),
            batch_children: HashMap::new(),
            tool_output_lines: ui_config.tool_output_lines,
            render_hints: RenderHintsRegistry::new(),
            pending_restores: Vec::new(),
            lua_event_handle,
            restore_event_tx: None,
            image_picker: crate::image_render::ImagePicker::new(),
            show_thinking: ui_config.show_thinking,
            thinking_collapsed: !ui_config.show_thinking,
            clock_format: ui_config.clock_format,
            prompt_progress: None,
        }
    }

    /// Hands back the index of the message, which [`Self::replace`] needs to
    /// correct it later.
    pub fn push(&mut self, msg: DisplayMessage) -> usize {
        self.messages.push(msg);
        self.messages.len() - 1
    }

    /// Drops the whole segment cache, so keep it for one-off corrections and
    /// never for streaming. Marking the message stale is not enough: only the
    /// segments the viewport reaches get reflowed, so a fix above it would
    /// keep painting the old bubble.
    pub fn replace(&mut self, index: usize, msg: DisplayMessage) {
        let Some(slot) = self.messages.get_mut(index) else {
            return;
        };
        *slot = msg;
        self.cache.clear();
    }

    pub fn load_messages(&mut self, mut msgs: Vec<DisplayMessage>) {
        if !self.show_thinking {
            for msg in &mut msgs {
                if matches!(msg.role, DisplayRole::Thinking) {
                    msg.thinking_collapsed = true;
                }
            }
        }
        self.messages = msgs;
        self.cache.clear();
        self.expanded_tools.clear();
        self.lua_expanded.clear();
        self.batch_children.clear();
        self.live_bufs.clear();
        self.highlight_segment = None;
        self.thinking_collapsed = !self.show_thinking;
    }

    pub fn set_restore_event_tx(&mut self, tx: Option<craft_agent::EventSender>) {
        self.restore_event_tx = tx;
    }

    pub fn thinking_delta(&mut self, text: &str) {
        self.streaming_thinking.push(text);
    }

    pub fn text_delta(&mut self, text: &str) {
        self.flush_thinking();
        self.streaming_text.push(text);
    }

    pub fn tool_pending(&mut self, id: String, name: &str) {
        self.flush();
        let role = DisplayRole::Tool(Box::new(ToolRole {
            id,
            status: ToolStatus::InProgress,
            name: Arc::from(name),
        }));
        let mut msg = DisplayMessage::new(role, String::new());
        msg.timestamp = Some(format_timestamp_now(self.clock_format));
        self.messages.push(msg);
    }

    pub fn auto_review_start(&mut self, tool_id: &str) {
        if let Some(msg) = self.find_tool_msg_mut(tool_id) {
            msg.review_status = Some("auto-review in progress…".to_string());
            self.rebuild_tool_segment(tool_id);
        }
    }

    pub fn auto_review_decision(
        &mut self,
        tool_id: &str,
        verdict: &str,
        risk: &str,
        rationale: &str,
    ) {
        if let Some(msg) = self.find_tool_msg_mut(tool_id) {
            let label = if verdict == "allow" {
                "allowed"
            } else {
                "denied"
            };
            msg.review_status = Some(format!("auto-review {label} (risk: {risk}): {rationale}"));
            self.rebuild_tool_segment(tool_id);
        }
    }

    pub fn tool_start(&mut self, event: ToolStartEvent) {
        if let Some(msg) = self.find_tool_msg_mut(&event.id) {
            if let DisplayRole::Tool(t) = &mut msg.role {
                t.name = Arc::clone(&event.tool);
            }
            msg.text = event.summary;
            msg.tool_input = event.input.map(Arc::new);
            msg.tool_output = event.output.map(Arc::new);
            msg.tool_raw_input = event.raw_input;
            msg.annotation = event.annotation;
            msg.render_header = event.render_header;
            self.rebuild_tool_segment(&event.id);
            return;
        }
        self.flush();
        let mut msg = DisplayMessage::new(
            DisplayRole::Tool(Box::new(ToolRole {
                id: event.id,
                status: ToolStatus::InProgress,
                name: Arc::clone(&event.tool),
            })),
            event.summary,
        );
        msg.tool_input = event.input.map(Arc::new);
        msg.tool_output = event.output.map(Arc::new);
        msg.tool_raw_input = event.raw_input;
        msg.annotation = event.annotation;
        msg.render_header = event.render_header;
        msg.timestamp = Some(format_timestamp_now(self.clock_format));
        self.messages.push(msg);
    }

    pub fn tool_output(&mut self, tool_id: &str, content: &str) {
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let tool_name = msg.role.tool_name().unwrap_or("");
        let hints = self.render_hints.get(tool_name);
        let limits = output_limits_from_hints(tool_name, hints, &self.tool_output_lines);
        truncate_to_header(&mut msg.text);
        let truncated = truncate_output(content, limits.max_lines, limits.keep);
        msg.truncated_lines = truncated.skipped;
        msg.text.push('\n');
        msg.text.push_str(&truncated.kept);
        msg.live_output = Some(content.to_owned());
        self.rebuild_tool_segment(tool_id);
    }

    pub fn tool_done(&mut self, event: ToolDoneEvent) {
        let live_entry = self.live_bufs.remove(&event.id);
        let had_live_buf = live_entry.is_some();
        if let Some(entry) = live_entry
            && let Some(lines) = entry.buf.read_if_dirty()
        {
            self.store_snapshot(&event.id, BufferSnapshot::from_arc(lines), None);
        }
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == event.id))
        else {
            return;
        };
        if let DisplayRole::Tool(t) = &mut msg.role {
            t.status = if event.is_error {
                ToolStatus::Error
            } else {
                ToolStatus::Success
            };
        }
        truncate_to_header(&mut msg.text);
        let hints = self.render_hints.get(&event.tool);
        let done_annotation = event
            .annotation
            .as_deref()
            .map(str::to_owned)
            .or_else(|| tool_output_annotation(&event.output));
        if let Some(suffix) = &done_annotation {
            append_annotation(&mut msg.annotation, suffix);
        }

        match &event.output {
            ToolOutput::Plain(text)
            | ToolOutput::Markdown(text)
            | ToolOutput::ReadDir { text, .. }
                if msg.render_snapshot.is_none() =>
            {
                if had_live_buf {
                    // The plugin streamed a body buf but no snapshot ever
                    // landed: this is the raw llm_output glitch users report.
                    warn!(
                        tool_id = %event.id,
                        tool = %event.tool,
                        is_error = event.is_error,
                        "live buf had no snapshot at tool_done; falling back to llm_output"
                    );
                }
                let limits = output_limits_from_hints(&event.tool, hints, &self.tool_output_lines);
                let tr = truncate_output(text, limits.max_lines, limits.keep);
                msg.truncated_lines = tr.skipped;
                if !tr.kept.is_empty() {
                    msg.text = format!("{}\n{}", msg.text, tr.kept);
                }
            }
            ToolOutput::GrepResult { entries } if entries.is_empty() => {
                msg.text = format!("{}\n{NO_FILES_FOUND}", msg.text);
            }
            ToolOutput::Batch { entries, .. } => {
                let failed = entries
                    .iter()
                    .filter(|e| e.status == BatchToolStatus::Error)
                    .count();
                if failed > 0 {
                    let total = entries.len();
                    msg.text = format!("{}/{total} tools succeeded", total - failed);
                }
            }
            _ => {}
        }
        if let ToolOutput::Batch {
            entries: new_entries,
            text,
            ..
        } = &event.output
            && let Some(arc) = &mut msg.tool_output
            && let ToolOutput::Batch {
                entries: existing,
                text: existing_text,
                ..
            } = Arc::make_mut(arc)
        {
            for (existing, new) in existing.iter_mut().zip(new_entries) {
                existing.status = new.status;
                existing.output = new.output.clone();
            }
            *existing_text = text.clone();
        } else {
            msg.tool_output = Some(Arc::new(event.output));
        }
        msg.live_output = None;
        self.rebuild_tool_segment(&event.id);
    }

    pub fn batch_progress(
        &mut self,
        batch_id: &str,
        index: usize,
        status: BatchToolStatus,
        output: Option<ToolOutput>,
        summary: Option<&str>,
    ) {
        let Some(msg) = self.find_tool_msg_mut(batch_id) else {
            return;
        };
        if let Some(arc) = &mut msg.tool_output
            && let ToolOutput::Batch { entries, .. } = Arc::make_mut(arc)
            && let Some(entry) = entries.get_mut(index)
        {
            entry.status = status;
            if output.is_some() {
                entry.output = output;
            }
            if let Some(s) = summary {
                entry.summary = s.to_owned();
            }
        }
        self.rebuild_tool_segment(batch_id);
    }

    pub fn update_tool_summary(&mut self, tool_id: &str, summary: &str) {
        self.update_tool(
            tool_id,
            |msg| msg.text = summary.to_owned(),
            |entry| entry.summary = summary.to_owned(),
        );
    }

    pub fn update_tool_model(&mut self, tool_id: &str, model: &str) {
        self.update_tool(
            tool_id,
            |msg| append_annotation(&mut msg.annotation, model),
            |entry| append_annotation(&mut entry.annotation, model),
        );
    }

    pub fn tool_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        self.store_snapshot(tool_id, snapshot, theme_gen);
    }

    pub fn tool_header_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        let theme_gen_val = theme_gen.unwrap_or(self.theme_generation);
        if let Some((batch_id, _)) = parse_batch_inner_id(tool_id) {
            let child = self.batch_children.entry(tool_id.to_owned()).or_default();
            child.header = Some(snapshot);
            child.snapshot_theme_gen = theme_gen_val;
            self.rebuild_tool_segment(batch_id);
        } else if let Some(msg) = self.find_tool_msg_mut(tool_id) {
            msg.text = snapshot.first_line_text();
            msg.render_header = Some(snapshot);
            msg.snapshot_theme_gen = theme_gen_val;
            self.rebuild_tool_segment(tool_id);
        } else {
            warn!(
                tool_id,
                is_header = true,
                "snapshot dropped: no tool message with this id"
            );
        }
    }

    /// A subagent stamps its own cumulative usage on the task header, and that
    /// header is usually the last tool of the turn, so an existing stamp wins.
    pub fn set_turn_usage_on_last_tool(&mut self, usage: String) {
        let last_tool = self.messages.iter().rev().find_map(|msg| match &msg.role {
            DisplayRole::Tool(tool) => Some((tool.id.clone(), msg.turn_usage.is_none())),
            _ => None,
        });
        if let Some((id, true)) = last_tool {
            self.set_tool_turn_usage(&id, usage);
        }
    }

    pub fn set_tool_turn_usage(&mut self, tool_id: &str, usage: String) {
        self.update_tool(tool_id, |msg| msg.turn_usage = Some(usage), |_| {});
    }

    fn upsert_instruction_segment(
        &mut self,
        parent_id: &str,
        blocks: &[InstructionBlock],
        parent_idx: usize,
        msg_index: Option<usize>,
    ) {
        if blocks.is_empty() {
            return;
        }
        let inst_id = segment::instruction_id(parent_id);
        let batch_index = parse_batch_inner_id(parent_id).map(|(_, idx)| idx + 1);
        let exp = self
            .expanded_tools
            .get(&inst_id)
            .copied()
            .unwrap_or_default();
        let tl = build_instructions_lines(blocks, self.viewport_width, exp.output, batch_index);

        if let Some(seg_idx) = self.cache.find_by_tool_id(&inst_id) {
            let seg = self.cache.get_mut(seg_idx).unwrap();
            seg.search_text = tl.search_text.clone();
            seg.update_with_reuse(tl, &self.hl_worker);
        } else {
            let mut seg = Segment::with_tool(inst_id, msg_index);
            seg.search_text = tl.search_text.clone();
            seg.apply_highlight(tl, &self.hl_worker);
            // Instructions and images arrive with the tool's output, so a
            // tool finishing beside slower siblings inserts above segments
            // that already exist. The scroll position names one by index, and
            // left alone it would quietly start naming a segment further down.
            let at = parent_idx + 1;
            if batch_index.is_some() {
                self.cache.insert(at, seg);
                if self.scroll.seg >= at {
                    self.scroll.seg += 1;
                }
            } else {
                self.cache.insert(at, Segment::spacer());
                self.cache.insert(at + 1, seg);
                if self.scroll.seg >= at {
                    self.scroll.seg += PAIR_SEGMENTS;
                }
            }
        }
    }

    const IMG_SUFFIX: &'static str = "__img";

    fn image_segment_id(parent_id: &str) -> String {
        format!("{parent_id}{}", Self::IMG_SUFFIX)
    }

    /// Insert or refresh a segment that renders an inline image for a
    /// `ToolOutput::Image`. The caption becomes the segment's single text
    /// line (linkable via OSC-8 if it contains a path); the image widget
    /// renders below it.
    fn upsert_image_segment(
        &mut self,
        parent_id: &str,
        output: Option<&ToolOutput>,
        msg_index: Option<usize>,
    ) {
        let Some(ToolOutput::Image { caption, source }) = output else {
            self.remove_image_segment(parent_id);
            return;
        };
        let img_id = Self::image_segment_id(parent_id);
        let render_state = self.image_picker.render_state(source, self.viewport_width);
        let mut hyperlink = None;
        if let Some(path) = crate::hyperlink::caption_path(caption) {
            hyperlink = crate::hyperlink::file_uri(path);
        }
        let caption_line = Line::from(Span::styled(caption.to_owned(), theme::current().tool_dim));

        if let Some(seg_idx) = self.cache.find_by_tool_id(&img_id) {
            let seg = self.cache.get_mut(seg_idx).unwrap();
            seg.search_text = caption.clone();
            seg.set_image(render_state.map(std::sync::Arc::new));
            seg.set_lines(vec![caption_line]);
            if let Some(uri) = hyperlink {
                seg.hyperlinks = vec![crate::hyperlink::Hyperlink::new(
                    0,
                    0,
                    caption.len() as u16,
                    uri,
                )];
            } else {
                seg.hyperlinks.clear();
            }
        } else if let Some(parent_idx) = self.cache.find_by_tool_id(parent_id) {
            let mut seg = Segment::with_tool(img_id, msg_index);
            seg.search_text = caption.clone();
            seg.set_image(render_state.map(std::sync::Arc::new));
            seg.set_lines(vec![caption_line]);
            if let Some(uri) = hyperlink {
                seg.hyperlinks = vec![crate::hyperlink::Hyperlink::new(
                    0,
                    0,
                    caption.len() as u16,
                    uri,
                )];
            }
            let at = parent_idx + 1;
            self.cache.insert(at, Segment::spacer());
            self.cache.insert(at + 1, seg);
            if self.scroll.seg >= at {
                self.scroll.seg += PAIR_SEGMENTS;
            }
        }
    }

    fn remove_image_segment(&mut self, parent_id: &str) {
        let img_id = Self::image_segment_id(parent_id);
        while let Some(idx) = self.cache.find_by_tool_id(&img_id) {
            let _ = self.cache.remove(idx);
        }
    }

    fn update_tool(
        &mut self,
        tool_id: &str,
        update_msg: impl FnOnce(&mut DisplayMessage),
        update_entry: impl FnOnce(&mut BatchToolEntry),
    ) {
        let rebuild_id;
        if let Some((batch_id, idx)) = parse_batch_inner_id(tool_id) {
            let Some(msg) = self.find_tool_msg_mut(batch_id) else {
                return;
            };
            if let Some(arc) = &mut msg.tool_output
                && let ToolOutput::Batch { entries, .. } = Arc::make_mut(arc)
                && let Some(entry) = entries.get_mut(idx)
            {
                update_entry(entry);
            }
            rebuild_id = batch_id.to_owned();
        } else {
            let Some(msg) = self.find_tool_msg_mut(tool_id) else {
                return;
            };
            update_msg(msg);
            rebuild_id = tool_id.to_owned();
        }
        self.rebuild_tool_segment(&rebuild_id);
    }

    pub fn stream_reset(&mut self) {
        self.streaming_thinking.clear();
        self.streaming_text.clear();
        self.thinking_collapsed = !self.show_thinking;
        self.cancel_in_progress();
    }

    pub fn fail_in_progress_with_message(&mut self, message: String) {
        self.fail_in_progress_except(message, &HashSet::new());
    }

    pub fn fail_in_progress_except(&mut self, message: String, excluded: &HashSet<String>) {
        let ids: Vec<(String, Arc<str>)> = self
            .messages
            .iter()
            .filter_map(|m| {
                if let DisplayRole::Tool(t) = &m.role
                    && t.status == ToolStatus::InProgress
                    && !excluded.contains(&t.id)
                {
                    Some((t.id.clone(), Arc::clone(&t.name)))
                } else {
                    None
                }
            })
            .collect();
        for (id, tool) in ids {
            self.tool_done(ToolDoneEvent {
                id,
                tool,
                output: ToolOutput::Plain(message.clone()),
                is_error: true,
                annotation: None,
                written_path: None,
            });
        }
    }

    pub fn cancel_in_progress(&mut self) {
        let affected_ids: Vec<String> = self
            .messages
            .iter_mut()
            .filter_map(|msg| {
                if let DisplayRole::Tool(t) = &mut msg.role
                    && t.status == ToolStatus::InProgress
                {
                    t.status = ToolStatus::Error;
                    if let Some(arc) = &mut msg.tool_output
                        && let ToolOutput::Batch { entries, .. } = Arc::make_mut(arc)
                    {
                        for entry in entries.iter_mut() {
                            if entry.status == BatchToolStatus::InProgress
                                || entry.status == BatchToolStatus::Pending
                            {
                                entry.status = BatchToolStatus::Error;
                            }
                        }
                    }
                    Some(t.id.clone())
                } else {
                    None
                }
            })
            .collect();

        for id in &affected_ids {
            self.rebuild_tool_segment(id);
        }
    }

    pub fn in_progress_count(&self) -> usize {
        self.messages
            .iter()
            .filter(
                |m| matches!(&m.role, DisplayRole::Tool(t) if t.status == ToolStatus::InProgress),
            )
            .count()
    }

    #[cfg(test)]
    pub fn toggle_expansion(&mut self, tool_id: &str) -> bool {
        let Some(seg) = self
            .cache
            .segments()
            .iter()
            .find(|s| s.tool_id.as_deref() == Some(tool_id))
        else {
            return false;
        };
        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        if !seg.truncation.any() && !exp.any() {
            return false;
        }
        let tool_id = tool_id.to_owned();
        let entry = self.expanded_tools.entry(tool_id.clone()).or_default();
        entry.script = !entry.script;
        entry.output = !entry.output;
        self.rebuild_expanded_tool(&tool_id);
        true
    }

    #[cfg(test)]
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    #[cfg(test)]
    pub fn message_at(&self, index: usize) -> Option<&DisplayMessage> {
        self.messages.get(index)
    }

    pub fn last_message_text(&self) -> &str {
        self.messages.last().map(|m| m.text.as_str()).unwrap_or("")
    }

    #[cfg(test)]
    pub fn last_message_is_plan(&self) -> bool {
        self.messages.last().is_some_and(|m| m.plan_path.is_some())
    }

    #[cfg(test)]
    pub fn last_message_role(&self) -> Option<&DisplayRole> {
        self.messages.last().map(|m| &m.role)
    }

    #[cfg(test)]
    pub fn streaming_text_is_empty(&self) -> bool {
        self.streaming_text.is_empty()
    }

    #[cfg(test)]
    pub fn streaming_thinking_is_empty(&self) -> bool {
        self.streaming_thinking.is_empty()
    }

    #[cfg(test)]
    pub fn tool_turn_usage(&self, tool_id: &str) -> Option<&str> {
        self.messages.iter().rev().find_map(|msg| match &msg.role {
            DisplayRole::Tool(tool) if tool.id == tool_id => msg.turn_usage.as_deref(),
            _ => None,
        })
    }

    pub fn set_prompt_progress(&mut self, progress: Option<PromptProgress>) {
        self.prompt_progress = progress;
    }

    pub fn clear_prompt_progress(&mut self) {
        self.prompt_progress = None;
    }

    pub fn flush(&mut self) {
        self.flush_thinking();
        self.prompt_progress = None;
        if !self.streaming_text.is_empty() {
            self.messages.push(DisplayMessage::new(
                DisplayRole::Assistant,
                self.streaming_text.take_all(),
            ));
        }
    }

    fn layout(&self) -> Layout<'_> {
        Layout::new(&self.cache, &self.tail, self.viewport_width)
    }

    /// Positive scrolls up. Clamping is immediate rather than deferred to the
    /// next `view`, so scrolling back up starts from the bottom row and not
    /// from wherever an overscroll left the position.
    pub fn scroll(&mut self, delta: i32) {
        let rows = delta.unsigned_abs();
        let layout = self.layout();
        let moved = if delta >= 0 {
            layout.retreat(self.scroll, rows)
        } else {
            layout.advance(self.scroll, rows)
        };
        let clamped = moved.min(layout.bottom(self.viewport_height));
        self.scroll_to(clamped);
    }

    /// Always unpins, and the next `view` re-pins if this lands on the
    /// bottom line.
    fn scroll_to(&mut self, pos: ScrollPos) {
        self.scroll = pos;
        self.auto_scroll = false;
    }

    pub fn auto_scroll(&self) -> bool {
        self.auto_scroll
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_to(ScrollPos::default());
    }

    pub fn enable_auto_scroll(&mut self) {
        self.auto_scroll = true;
    }

    pub fn scroll_to_segment(&mut self, segment_index: usize) {
        self.scroll_to(ScrollPos {
            seg: segment_index,
            row: 0,
        });
    }

    /// Backs `craft.fn.winrestview`, the one caller that still speaks in
    /// document rows.
    pub fn scroll_to_row(&mut self, doc_row: u32) {
        self.scroll_to(self.layout().at_row(doc_row));
    }

    pub fn restore_scroll(&mut self, scroll: ScrollPos, auto_scroll: bool) {
        self.scroll = scroll;
        self.auto_scroll = auto_scroll;
    }

    pub fn set_highlight_segment(&mut self, idx: Option<usize>) {
        self.highlight_segment = idx;
    }

    pub fn half_page(&self) -> i32 {
        self.viewport_height as i32 / 2
    }

    pub fn set_accent(&mut self, color: ratatui::style::Color) {
        self.accent.set(color);
    }

    pub fn handle_click(&mut self, row: u16, area: Rect) -> bool {
        if area.height == 0 {
            return false;
        }
        let pos = self
            .layout()
            .advance(self.scroll, u32::from(row.saturating_sub(area.y)));
        let width = self.viewport_width;
        // Both fallbacks toggle thinking: a position past the cached segments
        // belongs to the still-streaming indicator, and a segment without a
        // tool_id is a finished message's text.
        let Some(seg) = self.cache.get(pos.seg) else {
            return self.try_toggle_collapsed_thinking(pos);
        };
        let Some(tool_id) = seg.tool_id.as_deref() else {
            let msg_idx = seg.msg_index;
            return self.try_toggle_cached_thinking(msg_idx, width);
        };

        if self.has_snapshot(tool_id) {
            let expanded = !self.lua_expanded.contains(tool_id);
            if expanded {
                self.lua_expanded.insert(tool_id.to_owned());
            } else {
                self.lua_expanded.remove(tool_id);
            }
            if let Some(mut item) = self.lua_restore_item(tool_id) {
                item.expanded = expanded;
                if let Some(tx) = self.restore_event_tx.clone() {
                    self.lua_event_handle.request_restore(item, tx);
                }
            }
            return true;
        }

        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        if !seg.truncation.any() && !exp.any() {
            return false;
        }
        let tool_id = tool_id.to_owned();
        let truncation = seg.truncation;

        let entry = self.expanded_tools.entry(tool_id.clone()).or_default();
        if truncation.output || entry.output {
            entry.output = !entry.output;
        } else if truncation.script || entry.script {
            entry.script = !entry.script;
        }
        self.rebuild_expanded_tool(&tool_id);
        true
    }

    #[cfg(test)]
    pub fn toggle_expansion_at(&mut self, row: u16, area: Rect) -> bool {
        self.handle_click(row, area)
    }

    fn rebuild_expanded_tool(&mut self, tool_id: &str) {
        if segment::is_instruction_segment(tool_id) {
            if let Some(parent_id) = segment::instruction_parent(tool_id)
                && let Some(parent_idx) = self.cache.find_by_tool_id(parent_id)
                && let Some(blocks) = self.get_instructions_for_tool(parent_id)
            {
                self.upsert_instruction_segment(parent_id, &blocks, parent_idx, None);
            }
        } else {
            let rebuild_id =
                parse_batch_inner_id(tool_id).map_or(tool_id, |(batch_id, _)| batch_id);
            self.rebuild_tool_segment(rebuild_id);
        }
    }

    fn get_instructions_for_tool(&self, tool_id: &str) -> Option<Vec<InstructionBlock>> {
        let output = if let Some((batch_id, idx)) = parse_batch_inner_id(tool_id) {
            let msg = self
                .messages
                .iter()
                .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == batch_id))?;
            match msg.tool_output.as_deref()? {
                ToolOutput::Batch { entries, .. } => entries.get(idx)?.output.as_ref()?,
                _ => return None,
            }
        } else {
            let msg = self
                .messages
                .iter()
                .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))?;
            msg.tool_output.as_deref()?
        };
        output.owned_instructions()
    }

    /// Drains the highlight worker and every live tool buffer. These used to
    /// run inside [`Self::view`], which is why a running tool had to claim it
    /// was animating: it was the only way to keep them fed.
    pub fn tick(&mut self) -> Dirty {
        let mut dirty = self.drain_highlights() | self.poll_live_bufs();
        if self.show_idle_splash() {
            dirty |= self.idle_splash.poll_update(update::latest_version());
        }
        dirty
    }

    pub fn cadence(&self) -> Cadence {
        // Collapsed thinking draws a line count, not the text, so its
        // typewriter reveals nothing and never advances either, since only
        // `view` ticks it. Believing it would pin the loop at full frame rate
        // for the whole reasoning phase.
        let smooth = self.streaming_text.is_animating()
            || self.accent.is_animating()
            || (self.streaming_thinking.is_animating() && !self.streaming_thinking_collapsed());
        Cadence::any([
            // A running tool draws a spinner. Its output arriving is data, and
            // `tick` reports that separately.
            Cadence::when(self.in_progress_count() > 0, Cadence::SPINNER),
            Cadence::when(smooth, Cadence::SMOOTH),
            Cadence::when(self.show_idle_splash(), self.idle_splash.cadence()),
        ])
    }

    fn streaming_thinking_collapsed(&self) -> bool {
        self.thinking_collapsed && !self.streaming_thinking.is_empty()
    }

    fn show_idle_splash(&self) -> bool {
        self.messages.is_empty()
            && self.streaming_thinking.is_empty()
            && self.streaming_text.is_empty()
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, has_selection: bool) {
        self.viewport_height = area.height;
        let width = area.width.saturating_sub(1);
        let theme_gen = theme::generation();
        let width_changed = self.viewport_width != width || self.theme_generation != theme_gen;
        let theme_changed = self.theme_generation != theme_gen;
        if width_changed {
            self.viewport_width = width;
            self.theme_generation = theme_gen;
        }

        if self.show_idle_splash() {
            // Every other exit rebuilds the tail; this one has to drop it, or
            // `Layout` keeps answering with rows nothing draws any more.
            self.tail.clear();
            let accent = self.accent.resolve();
            self.idle_splash.render(area, frame.buffer_mut(), accent);
            return;
        }

        if width_changed {
            self.cache.mark_all_width_stale();
            let thinking = thinking_style();
            let assistant = assistant_style();
            self.streaming_thinking.set_style(
                thinking.prefix,
                thinking.text_style,
                thinking.prefix_style,
            );
            self.streaming_text.set_style(
                assistant.prefix,
                assistant.text_style,
                assistant.prefix_style,
            );
        }

        if theme_changed {
            self.collect_stale_snapshots(theme_gen);
        }
        self.rebuild_line_cache();
        if self.in_progress_count() > 0 {
            self.update_spinners();
        }

        let collapsed_thinking_lines = if self.streaming_thinking_collapsed() {
            self.build_streaming_collapsed_lines()
        } else {
            Vec::new()
        };
        self.tail = self.build_tail(width, &collapsed_thinking_lines);

        // The reflow window is picked from `scroll` and the bottom pin, and
        // the reflow changes the heights both are derived from: resolve
        // before to aim the window, and after to place the result.
        self.resolve_scroll(has_selection);
        self.reflow_viewport(width);
        self.resolve_scroll(has_selection);

        let viewport = Rect::new(area.x, area.y, width, area.height);
        let mut cursor = RenderCursor::new(self.scroll.row, viewport);

        for (i, seg) in self
            .cache
            .segments()
            .iter()
            .enumerate()
            .skip(self.scroll.seg)
        {
            if cursor.past_bottom() {
                break;
            }
            let h = seg.height(width);
            let highlight = self.highlight_segment == Some(i);
            if let Some(img) = &seg.image {
                cursor.render_image(img, seg.lines(), None, frame);
            } else {
                cursor.render(seg.lines(), h, None, highlight, &seg.hyperlinks, frame);
            }
        }

        let spacer_lines: [Line<'static>; 1] = [Line::default()];
        for &(part, h) in self
            .tail
            .iter()
            .skip(self.scroll.seg.saturating_sub(self.cache.len()))
        {
            if cursor.past_bottom() {
                break;
            }
            let lines = match part {
                TailPart::Spacer => &spacer_lines[..],
                TailPart::Thinking if !collapsed_thinking_lines.is_empty() => {
                    &collapsed_thinking_lines
                }
                TailPart::Thinking => self.streaming_thinking.cached_lines(),
                TailPart::Text => self.streaming_text.cached_lines(),
            };
            cursor.render(lines, h, None, false, &[], frame);
        }

        if let Some(pp) = self.prompt_progress
            && pp.total > 0
        {
            let ratio = pp.processed as f64 / pp.total as f64;
            let bar_width = (width as f64 * 0.1).round() as u16;
            let label = " Processing ";
            let label_width = label.len() as u16;
            let total_width = label_width + bar_width;
            let bar_x = area.x + width.saturating_sub(total_width);
            let bar_y = area.y + area.height.saturating_sub(1);
            let bar_area = Rect::new(bar_x, bar_y, total_width, 1);
            crate::components::progress_bar::render(
                frame,
                bar_area,
                &crate::components::progress_bar::ProgressBarConfig {
                    ratio,
                    style: theme::current().progress_bar,
                    cache_ratio: pp.cache as f64 / pp.total as f64,
                    cache_style: Style::new().fg(Color::Green),
                    label: Some(label),
                    label_style: Some(theme::current().tool_dim),
                    bar_width,
                },
            );
        }

        // Both walks are O(transcript) and a resize makes each one re-wrap
        // every segment it touches, so they stay behind the toggle that
        // decides whether anything is drawn from them.
        if scrollbar::is_enabled() {
            let layout = self.layout();
            let total_rows = layout.total_rows();
            if total_rows > u32::from(area.height) {
                render_vertical_scrollbar(frame, area, total_rows, layout.doc_row(self.scroll));
            }
        }
    }

    /// The streaming tail, in the same order and under the same spacer rule
    /// `rebuild_line_cache` uses when the turn flushes. A [`ScrollPos`] in the
    /// tail keeps pointing at the same content across that flush only while
    /// the two agree, so anything added here needs its segment there.
    fn build_tail(
        &mut self,
        width: u16,
        collapsed_thinking: &[Line<'static>],
    ) -> Vec<(TailPart, u16)> {
        let has_cached = self.cache.len() > 0;
        let mut tail: Vec<(TailPart, u16)> = Vec::new();
        // Mirrors `SegmentCache::push_spacer_if_needed`: a part is separated
        // from whatever precedes it in the document.
        let mut push = |part, height| {
            if has_cached || !tail.is_empty() {
                tail.push((TailPart::Spacer, 1));
            }
            tail.push((part, height));
        };

        if self.streaming_thinking_collapsed() {
            push(TailPart::Thinking, collapsed_thinking.len() as u16);
        } else if !self.streaming_thinking.is_empty() {
            let h = wrap::total_rows(self.streaming_thinking.render_lines(width), width);
            push(TailPart::Thinking, h);
        }
        if !self.streaming_text.is_empty() {
            let h = wrap::total_rows(self.streaming_text.render_lines(width), width);
            push(TailPart::Text, h);
        }
        tail
    }

    pub fn scroll_pos(&self) -> ScrollPos {
        self.scroll
    }

    /// Selections still count rows from the top of the document, so they need
    /// this bridge until they are anchored to segments too.
    pub fn scroll_doc_row(&self) -> u32 {
        self.layout().doc_row(self.scroll)
    }

    /// Backs `craft.fn.winsaveview`. The clamp matters: a pinned or restored
    /// scroll position can sit past the end until the next `view` resolves it
    /// against the current line count.
    pub fn win_view(&self) -> WinView {
        let layout = self.layout();
        let bottom = layout.bottom(self.viewport_height);
        WinView {
            scroll_top: layout.doc_row(layout.clamp(self.scroll.min(bottom))),
            line_count: layout.total_rows(),
            height: self.viewport_height,
            auto_scroll: self.auto_scroll,
        }
    }

    #[cfg(test)]
    pub fn segment_heights(&self) -> Vec<u16> {
        let width = self.viewport_width;
        self.cache
            .segments()
            .iter()
            .map(|s| s.height(width))
            .collect()
    }

    pub fn segment_search_texts(&self) -> Vec<&str> {
        self.cache.search_texts()
    }

    pub fn extract_selection_text(&self, sel: &Selection, msg_area: Rect) -> String {
        selection::extract_selection_text(&self.cache, self.viewport_width, sel, msg_area)
    }

    fn has_snapshot(&self, tool_id: &str) -> bool {
        self.batch_children
            .get(tool_id)
            .is_some_and(|c| c.snapshot.is_some())
            || self
                .messages
                .iter()
                .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
                .is_some_and(|m| m.render_snapshot.is_some())
    }

    fn lua_restore_item(&self, tool_id: &str) -> Option<RestoreItem> {
        let tol = self.tool_output_lines;
        if let Some((batch_id, idx)) = parse_batch_inner_id(tool_id) {
            let msg = self
                .messages
                .iter()
                .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == batch_id))?;
            let entries = match msg.tool_output.as_deref()? {
                ToolOutput::Batch { entries, .. } => entries,
                _ => return None,
            };
            let entry = entries.get(idx)?;
            let output_text = entry
                .output
                .as_ref()
                .map(|o| o.as_text())
                .unwrap_or_default();
            Some(RestoreItem {
                tool: Arc::from(entry.tool.as_str()),
                tool_use_id: tool_id.to_owned(),
                output: output_text,
                input: entry.raw_input.clone().unwrap_or(Value::Null),
                is_error: entry.status == BatchToolStatus::Error,
                tool_output_lines: tol,
                theme_gen: Some(self.theme_generation),
                expanded: false,
            })
        } else {
            let msg = self
                .messages
                .iter()
                .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))?;
            let DisplayRole::Tool(t) = &msg.role else {
                return None;
            };
            let output_text = msg
                .tool_output
                .as_deref()
                .map(|o| o.as_text())
                .unwrap_or_default();
            Some(RestoreItem {
                tool: Arc::clone(&t.name),
                tool_use_id: t.id.clone(),
                output: output_text,
                input: msg.tool_raw_input.clone().unwrap_or(Value::Null),
                is_error: t.status == ToolStatus::Error,
                tool_output_lines: tol,
                theme_gen: Some(self.theme_generation),
                expanded: false,
            })
        }
    }

    fn store_snapshot(&mut self, tool_id: &str, snapshot: BufferSnapshot, theme_gen: Option<u64>) {
        let theme_gen_val = theme_gen.unwrap_or(self.theme_generation);
        if let Some((batch_id, _)) = parse_batch_inner_id(tool_id) {
            let child = self.batch_children.entry(tool_id.to_owned()).or_default();
            child.snapshot = Some(snapshot);
            child.snapshot_theme_gen = theme_gen_val;
            self.rebuild_tool_segment(batch_id);
        } else if let Some(msg) = self.find_tool_msg_mut(tool_id) {
            msg.render_snapshot = Some(snapshot);
            msg.snapshot_theme_gen = theme_gen_val;
            self.rebuild_tool_segment(tool_id);
        } else {
            warn!(
                tool_id,
                is_header = false,
                "snapshot dropped: no tool message with this id"
            );
        }
    }

    fn find_tool_msg_mut(&mut self, tool_id: &str) -> Option<&mut DisplayMessage> {
        self.messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
    }

    fn rctx(&self) -> RenderCtx<'_> {
        RenderCtx {
            started_at: self.started_at,
            width: self.viewport_width,
            tool_output_lines: &self.tool_output_lines,
            registry: &self.render_hints,
        }
    }

    fn collect_stale_snapshots(&mut self, current_gen: u64) {
        for msg in &self.messages {
            let DisplayRole::Tool(t) = &msg.role else {
                continue;
            };
            let has_snapshot = msg.render_snapshot.is_some() || msg.render_header.is_some();
            if has_snapshot && msg.snapshot_theme_gen != current_gen {
                let output_text = msg
                    .tool_output
                    .as_deref()
                    .map(|o| o.as_text())
                    .unwrap_or_default();
                self.pending_restores.push(RestoreItem {
                    tool: Arc::clone(&t.name),
                    tool_use_id: t.id.clone(),
                    output: output_text,
                    input: msg.tool_raw_input.clone().unwrap_or(Value::Null),
                    is_error: t.status == ToolStatus::Error,
                    tool_output_lines: self.tool_output_lines,
                    theme_gen: Some(current_gen),
                    expanded: self.lua_expanded.contains(&t.id),
                });
            }
        }
        for (child_id, child) in &self.batch_children {
            if child.snapshot_is_stale(current_gen) {
                let Some(mut item) = self.lua_restore_item(child_id) else {
                    continue;
                };
                item.theme_gen = Some(current_gen);
                item.expanded = self.lua_expanded.contains(child_id);
                self.pending_restores.push(item);
            }
        }
    }

    pub fn drain_pending_restores(&mut self) -> Vec<RestoreItem> {
        std::mem::take(&mut self.pending_restores)
    }

    pub fn register_live_buf(&mut self, id: String, body: Arc<SharedBuf>) {
        self.live_bufs.insert(
            id,
            LiveBufEntry {
                buf: body,
                dirty_seen: false,
            },
        );
    }

    fn poll_live_bufs(&mut self) -> Dirty {
        let mut updated = Vec::new();
        let mut stale = Vec::new();
        for (id, entry) in &mut self.live_bufs {
            if let Some(lines) = entry.buf.read_if_dirty() {
                entry.dirty_seen = true;
                updated.push((id.clone(), lines));
            } else if entry.dirty_seen {
                stale.push(id.clone());
            }
        }
        let dirty = Dirty::from(!updated.is_empty());
        for (tool_id, lines) in updated {
            self.store_snapshot(&tool_id, BufferSnapshot::from_arc(lines), None);
        }
        for id in stale {
            self.live_bufs.remove(&id);
        }
        dirty
    }

    fn build_tool_segment_lines(
        msg: &DisplayMessage,
        status: ToolStatus,
        rctx: &RenderCtx,
        exp: SectionFlags,
    ) -> ToolLines {
        let mut tl = build_tool_lines(msg, status, rctx, exp);
        if let Some(ts) = &msg.timestamp
            && !tl.lines.is_empty()
        {
            append_right_info(
                &mut tl.lines[0],
                msg.turn_usage.as_deref(),
                Some(ts),
                rctx.width,
            );
        }
        if let Some(review) = &msg.review_status {
            tl.lines.push(Line::from(vec![
                Span::styled("  ↳ ", theme::current().tool_annotation),
                Span::styled(review.clone(), theme::current().tool_annotation),
            ]));
        }
        tl
    }

    fn flush_thinking(&mut self) {
        if self.streaming_thinking.is_empty() {
            return;
        }
        let mut msg =
            DisplayMessage::new(DisplayRole::Thinking, self.streaming_thinking.take_all());
        msg.thinking_collapsed = self.thinking_collapsed;
        self.thinking_collapsed = !self.show_thinking;
        self.messages.push(msg);
    }

    fn build_streaming_collapsed_lines(&self) -> Vec<Line<'static>> {
        thinking_indicator(self.streaming_thinking.line_count())
    }

    fn build_cached_thinking_indicator(&self, text: &str) -> Vec<Line<'static>> {
        thinking_indicator(logical_line_count(text))
    }

    /// `pos` is past the cached segments, so it names a tail part: the click
    /// toggles only when that part is the collapsed thinking indicator.
    fn try_toggle_collapsed_thinking(&mut self, pos: ScrollPos) -> bool {
        let part = pos
            .seg
            .checked_sub(self.cache.len())
            .and_then(|i| self.tail.get(i))
            .map(|&(p, _)| p);
        if part != Some(TailPart::Thinking) || !self.streaming_thinking_collapsed() {
            return false;
        }
        self.thinking_collapsed = false;
        true
    }

    fn try_toggle_cached_thinking(&mut self, msg_idx: Option<usize>, width: u16) -> bool {
        if self.show_thinking {
            return false;
        }
        let Some(idx) = msg_idx else { return false };
        let Some(msg) = self.messages.get_mut(idx) else {
            return false;
        };
        if !matches!(msg.role, DisplayRole::Thinking) {
            return false;
        }
        msg.thinking_collapsed = !msg.thinking_collapsed;
        self.rebuild_thinking_segment(idx, width);
        true
    }

    fn rebuild_thinking_segment(&mut self, msg_idx: usize, width: u16) {
        let Some((text, collapsed)) = self
            .messages
            .get(msg_idx)
            .map(|m| (m.text.clone(), m.thinking_collapsed))
        else {
            return;
        };
        let lines = if collapsed {
            self.build_cached_thinking_indicator(&text)
        } else {
            let style = thinking_style();
            text_to_lines(
                &text,
                style.prefix,
                style.text_style,
                style.prefix_style,
                width,
                None,
            )
        };
        let search_text = format!("thinking> {text}");
        let seg_idx = self
            .cache
            .segments()
            .iter()
            .position(|s| s.msg_index == Some(msg_idx) && s.tool_id.is_none());
        let Some(seg_idx) = seg_idx else { return };
        if let Some(seg) = self.cache.get_mut(seg_idx) {
            seg.set_lines(lines);
            seg.search_text = search_text;
        }
    }

    fn update_spinners(&mut self) {
        let spinner_span = Span::styled(
            spinner_str(self.started_at.elapsed().as_millis()),
            theme::current().spinner,
        );
        for seg in self.cache.segments_mut() {
            let is_child = seg
                .tool_id
                .as_deref()
                .is_some_and(segment::is_child_segment);
            for &line_idx in &seg.spinner_lines.clone() {
                let span_idx = if line_idx == 0 && (!is_child || seg.has_bar) {
                    0
                } else {
                    1
                };
                seg.update_spinner(line_idx, span_idx, spinner_span.clone());
            }
        }
    }

    fn drain_highlights(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        while let Some(result) = self.hl_worker.try_recv() {
            if let Some(seg) = self
                .cache
                .segments_mut()
                .iter_mut()
                .find(|s| s.matches_pending_highlight(result.id))
            {
                seg.apply_highlight_result(result.lines);
                dirty = Dirty::YES;
            }
        }
        dirty
    }

    fn rebuild_tool_segment(&mut self, tool_id: &str) {
        let Some(msg) = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let DisplayRole::Tool(t) = &msg.role else {
            unreachable!()
        };
        let status = t.status;
        let Some(seg_idx) = self.cache.find_by_tool_id(tool_id) else {
            return;
        };

        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        let rctx = self.rctx();
        let tl = Self::build_tool_segment_lines(msg, status, &rctx, exp);

        let instructions = msg
            .tool_output
            .as_deref()
            .and_then(|o| o.owned_instructions());
        let image_output = msg.tool_output.as_deref().and_then(|o| match o {
            ToolOutput::Image { .. } => Some(o.clone()),
            _ => None,
        });

        let seg = self.cache.get_mut(seg_idx).unwrap();
        seg.search_text = tl.search_text.clone();
        seg.update_with_reuse(tl, &self.hl_worker);

        self.build_and_upsert_batch_children(seg_idx, tool_id);

        if let Some(blocks) = instructions {
            self.upsert_instruction_segment(tool_id, &blocks, seg_idx, None);
        }
        self.upsert_image_segment(tool_id, image_output.as_ref(), None);
    }

    fn build_and_upsert_batch_children(&mut self, parent_idx: usize, tool_id: &str) {
        let Some(msg) = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let Some(ToolOutput::Batch { entries, .. }) = msg.tool_output.as_deref() else {
            return;
        };
        let rctx = self.rctx();
        let children: Vec<_> = entries
            .iter()
            .enumerate()
            .map(|(j, entry)| {
                let child_id = format!("{tool_id}__{j}");
                let child_exp = self
                    .expanded_tools
                    .get(&child_id)
                    .copied()
                    .unwrap_or_default();
                let tl = build_batch_entry_lines(
                    entry,
                    j,
                    &rctx,
                    child_exp,
                    self.batch_children.get(&child_id),
                );
                let search = tl.search_text.clone();
                let instructions = entry.output.as_ref().and_then(|o| o.owned_instructions());
                (child_id, search, tl, instructions)
            })
            .collect();
        let child_prefix = format!("{tool_id}__");
        let msg_index = self.cache.get(parent_idx).and_then(|s| s.msg_index);
        for (child_id, search, tl, instructions) in children {
            let child_seg_idx = if let Some(cseg_idx) = self.cache.find_by_tool_id(&child_id) {
                let cseg = self.cache.get_mut(cseg_idx).unwrap();
                cseg.search_text = search;
                cseg.update_with_reuse(tl, &self.hl_worker);
                cseg_idx
            } else {
                let mut seg = Segment::with_tool(child_id.clone(), msg_index);
                seg.search_text = search;
                seg.apply_highlight(tl, &self.hl_worker);
                let insert_pos = self
                    .cache
                    .segments()
                    .iter()
                    .rposition(|s| {
                        s.tool_id
                            .as_deref()
                            .is_some_and(|id| id == tool_id || id.starts_with(&child_prefix))
                    })
                    .map_or(parent_idx + 1, |p| p + 1);
                self.cache.insert(insert_pos, seg);
                insert_pos
            };
            if let Some(blocks) = instructions {
                self.upsert_instruction_segment(&child_id, &blocks, child_seg_idx, msg_index);
            }
        }
    }

    fn rebuild_line_cache(&mut self) {
        if !self.cache.needs_rebuild(self.messages.len()) {
            return;
        }
        for i in self.cache.msg_count()..self.messages.len() {
            let msg = &self.messages[i];

            if let DisplayRole::Tool(t) = &msg.role {
                let exp = self.expanded_tools.get(&t.id).copied().unwrap_or_default();
                let status = t.status;
                let tl = Self::build_tool_segment_lines(msg, status, &self.rctx(), exp);
                let id = t.id.clone();
                let search_text = tl.search_text.clone();
                self.cache.push_spacer_if_needed();
                let mut seg = Segment::with_tool(id.clone(), Some(i));
                seg.search_text = search_text;
                seg.apply_highlight(tl, &self.hl_worker);
                self.cache.push(seg);

                if let Some(ToolOutput::Batch { entries, .. }) = msg.tool_output.as_deref() {
                    let inst_data: Vec<_> = entries
                        .iter()
                        .enumerate()
                        .map(|(j, entry)| {
                            let child_id = format!("{id}__{j}");
                            let child_exp = self
                                .expanded_tools
                                .get(&child_id)
                                .copied()
                                .unwrap_or_default();
                            let tl = build_batch_entry_lines(
                                entry,
                                j,
                                &self.rctx(),
                                child_exp,
                                self.batch_children.get(&child_id),
                            );
                            let blocks = entry.output.as_ref().and_then(|o| o.owned_instructions());
                            (child_id, tl, blocks)
                        })
                        .collect();
                    for (child_id, tl, blocks) in inst_data {
                        let mut seg = Segment::with_tool(child_id.clone(), Some(i));
                        seg.search_text = tl.search_text.clone();
                        seg.apply_highlight(tl, &self.hl_worker);
                        self.cache.push(seg);
                        if let Some(blocks) = blocks {
                            let last_idx = self.cache.len().saturating_sub(1);
                            self.upsert_instruction_segment(&child_id, &blocks, last_idx, Some(i));
                        }
                    }
                } else {
                    let blocks = msg
                        .tool_output
                        .as_deref()
                        .and_then(|o| o.owned_instructions());
                    let image_output = msg.tool_output.as_deref().and_then(|o| match o {
                        ToolOutput::Image { .. } => Some(o.clone()),
                        _ => None,
                    });
                    if let Some(blocks) = blocks {
                        let last_idx = self.cache.len().saturating_sub(1);
                        self.upsert_instruction_segment(&id, &blocks, last_idx, Some(i));
                    }
                    self.upsert_image_segment(&id, image_output.as_ref(), Some(i));
                }
            } else {
                if matches!(&msg.role, DisplayRole::Thinking) && msg.thinking_collapsed {
                    let text = msg.text.clone();
                    let lines = self.build_cached_thinking_indicator(&text);
                    let search_text = format!("thinking> {text}");
                    self.cache.push_spacer_if_needed();
                    self.cache
                        .push(Segment::with_lines(lines, search_text, Some(i)));
                    continue;
                }
                let (lines, search_text) = build_message_lines(msg, self.viewport_width);
                self.cache.push_spacer_if_needed();
                self.cache
                    .push(Segment::with_lines(lines, search_text, Some(i)));
            }
        }
        self.cache.mark_built(self.messages.len());
    }

    /// Clamps the scroll position against the document end and applies the
    /// bottom pin.
    fn resolve_scroll(&mut self, has_selection: bool) {
        let bottom = self.layout().bottom(self.viewport_height);
        self.scroll = self.layout().clamp(self.scroll.min(bottom));
        if !has_selection {
            if self.scroll >= bottom {
                self.auto_scroll = true;
            }
            if self.auto_scroll {
                self.scroll = bottom;
            }
        }
    }

    /// Re-lays out the stale segments the viewport plus its margin reaches.
    /// The scroll position names a segment, so reflowing cannot slide the
    /// content the reader is looking at; the window only has to cover what is
    /// on screen.
    ///
    /// Heights are counted after each segment is reflowed, so the window is
    /// right the first time.
    fn reflow_viewport(&mut self, width: u16) {
        let viewport = u32::from(self.viewport_height);
        let margin = viewport.saturating_mul(REFLOW_MARGIN_VIEWPORTS);
        // The first visible row sits `scroll.row` rows into the starting
        // segment, so the downward window has to clear those before it starts
        // covering the viewport.
        let below = viewport
            .saturating_add(margin)
            .saturating_add(u32::from(self.scroll.row));
        // A scroll position in the streaming tail has no segment to start
        // from; the last cached one is the closest thing above it.
        let start = self.scroll.seg.min(self.cache.len().saturating_sub(1));

        self.reflow_run(start..self.cache.len(), below, width);
        self.reflow_run((0..start).rev(), margin, width);
    }

    fn reflow_run(&mut self, indices: impl Iterator<Item = usize>, row_budget: u32, width: u16) {
        let mut rows = 0;
        for i in indices {
            if rows >= row_budget {
                return;
            }
            rows += self.reflowed_height(i, width);
        }
    }

    /// Reflows `seg_idx` if it is stale, then reports the height it draws at.
    fn reflowed_height(&mut self, seg_idx: usize, width: u16) -> u32 {
        // A tool segment and its instruction segment both map back to the
        // same parent, and one `rebuild_tool_segment` clears both flags.
        // Re-check so the parent is not rebuilt twice.
        if self.cache.get(seg_idx).is_some_and(|s| s.stale) {
            self.reflow_segment(seg_idx, width);
        }
        self.cache
            .get(seg_idx)
            .map_or(0, |s| s.height(width) as u32)
    }

    fn reflow_segment(&mut self, seg_idx: usize, width: u16) {
        // Clear up front so a reflow that bails early (message gone, empty
        // instructions) costs one frame of old-width lines instead of
        // retrying forever.
        let Some(seg) = self.cache.get_mut(seg_idx) else {
            return;
        };
        seg.stale = false;
        let (tool_id, msg_idx) = (seg.tool_id.clone(), seg.msg_index);

        if let Some(tid) = tool_id {
            let parent = segment::instruction_parent(&tid)
                .map(str::to_string)
                .unwrap_or(tid);
            self.rebuild_tool_segment(&parent);
            return;
        }

        let Some(msg_idx) = msg_idx else {
            return;
        };

        let collapsed = self
            .messages
            .get(msg_idx)
            .is_some_and(|m| matches!(m.role, DisplayRole::Thinking) && m.thinking_collapsed);
        if collapsed {
            // Geometry is width-independent, but `width_changed` also fires on
            // theme changes; rebuild so spans pick up the new palette.
            self.rebuild_thinking_segment(msg_idx, width);
        } else {
            self.reflow_text_segment(seg_idx, width);
        }
    }

    fn reflow_text_segment(&mut self, seg_idx: usize, width: u16) {
        let Some(msg_idx) = self.cache.get(seg_idx).and_then(|s| s.msg_index) else {
            return;
        };
        let Some(msg) = self.messages.get(msg_idx) else {
            return;
        };
        let (lines, search_text) = build_message_lines(msg, width);
        let Some(seg) = self.cache.get_mut(seg_idx) else {
            return;
        };
        seg.set_lines(lines);
        seg.search_text = search_text;
    }
}

fn thinking_indicator(line_count: usize) -> Vec<Line<'static>> {
    let theme = theme::current();
    vec![
        Line::from(Span::styled(THINKING_HIDDEN_HEADER, theme.thinking)),
        Line::from(vec![
            Span::styled(format!("({line_count} lines) "), theme.tool_dim),
            Span::styled("(click to expand)", theme.thinking),
        ]),
    ]
}

fn logical_line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.bytes().filter(|&b| b == b'\n').count() + 1
    }
}

/// Builds ratatui lines for a non-Tool, non-collapsed-Thinking message at the
/// given width, returning the lines and search text. Shared by
/// `rebuild_line_cache` (new messages) and `reflow_text_segment` (stale-on-resize
/// messages) so both paths produce identical segments.
fn build_message_lines(msg: &DisplayMessage, width: u16) -> (Vec<Line<'static>>, String) {
    let style = match &msg.role {
        DisplayRole::User => user_style(),
        DisplayRole::Assistant => assistant_style(),
        DisplayRole::Thinking => thinking_style(),
        DisplayRole::Error => error_style(),
        DisplayRole::Done => done_style(),
        DisplayRole::Tool(_) => unreachable!(),
    };
    let prefix = if msg.plan_path.is_some() {
        ""
    } else {
        style.prefix
    };
    let mut lines = if style.use_markdown {
        text_to_lines(
            &msg.text,
            prefix,
            style.text_style,
            style.prefix_style,
            width,
            None,
        )
    } else {
        plain_lines(&msg.text, prefix, style.text_style, style.prefix_style)
    };
    if let Some(pp) = &msg.plan_path {
        if !msg.text.is_empty() {
            let rule = hr_line(width, theme::current().plan_rule);
            lines.insert(0, rule.clone());
            lines.push(rule);
        } else {
            lines.clear();
        }
        if !msg.text.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            pp.to_owned(),
            theme::current().plan_path,
        )));
        lines.push(Line::from(Span::styled(
            format!(
                "{} to open in editor ($VISUAL / $EDITOR)",
                key::OPEN_EDITOR.label
            ),
            theme::current().tool_dim,
        )));
    }
    let search_text = format!("{prefix}{}", msg.text);
    (lines, search_text)
}
