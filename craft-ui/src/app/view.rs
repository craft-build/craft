use crate::components::Overlay;
#[cfg(test)]
use crate::components::keybindings::KeybindContext;
use crate::components::queue_panel;
use crate::components::split_layout::{MIN_CHAT_ROWS, SplitLayout, carve};
use crate::components::status_bar::{self, StatusBarContext, UsageStats};
use crate::selection::{self, SelectableZone, SelectionZone, ZoneRegistry};
use crate::theme;
use craft_lua::Split;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};

use super::{App, Mode, Status};

/// Horizontal/top inset applied to the message transcript so text doesn't
/// sit flush against the terminal edge, matching the prototype's layout.
const MSG_PAD_X: u16 = 2;
const MSG_PAD_TOP: u16 = 1;

struct ViewLayout {
    msg_area: Rect,
    bottom_area: Rect,
    status_area: Rect,
    queue_area: Rect,
    panel_windows: Vec<(usize, Rect)>,
    model_row_area: Rect,
    input_area: Rect,
    splits: SplitLayout,
    content_area: Rect,
    bottom_takeover: bool,
}

impl App {
    pub fn view(&mut self, frame: &mut Frame) {
        self.status_bar.clear_expired_hint();

        self.sync_flow_snapshot();
        let form_visible = self.permission_prompt.is_open()
            || self.plan_form_active()
            || self.flow_goal_form_active();
        let layout = self.compute_layout(frame, form_visible);
        let render_chat = self.resolve_render_chat();

        self.render_background(frame);
        self.render_messages(frame, &layout, render_chat);
        self.render_bottom_panel(frame, &layout);
        self.render_splits(frame, &layout);
        let mut overlay_rect = self.render_picker_overlays(frame, &layout);
        self.render_status_bar(frame, layout.status_area, render_chat);
        overlay_rect = self.render_top_modals(frame, overlay_rect);
        self.register_zones(&layout, overlay_rect);
        self.apply_selection(frame, render_chat);
    }

    fn compute_layout(&self, frame: &Frame, form_visible: bool) -> ViewLayout {
        self.compute_layout_raw(frame.area(), form_visible)
    }

    fn compute_layout_raw(&self, area: Rect, form_visible: bool) -> ViewLayout {
        let permission_open = self.permission_prompt.is_open();

        let [content, status_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

        let reqs: Vec<_> = self
            .float_mgr
            .split_reqs(content)
            .into_iter()
            .filter(|r| !(permission_open && r.split == Split::Below))
            .collect();
        let splits = carve(content, &reqs);
        let inner = splits.inner;

        let below_active = splits.rect(Split::Below).is_some();
        let bottom_takeover = form_visible || below_active;
        let show_model_row = !permission_open && !bottom_takeover && self.is_main_chat();
        let max_bottom = inner.height.saturating_sub(MIN_CHAT_ROWS);
        let bottom_height = if permission_open {
            self.permission_prompt.height(inner.width).min(max_bottom)
        } else if below_active {
            0
        } else if form_visible {
            let form_h = if self.plan_form_active() {
                self.plan_form.height()
            } else if self.flow_goal_form_active() {
                self.flow_goal_form.height()
            } else {
                0
            };
            form_h.min(max_bottom)
        } else if self.is_main_chat() {
            let panel_h: u16 = self
                .float_mgr
                .panel_reqs(content)
                .iter()
                .map(|(_, h)| *h)
                .sum();
            queue_panel::height(self.queue.panel_len())
                + panel_h
                + status_bar::MODEL_ROW_HEIGHT
                + self.input_box.height(inner.width).min(max_bottom)
        } else {
            let panel_h: u16 = self
                .float_mgr
                .panel_reqs(content)
                .iter()
                .map(|(_, h)| *h)
                .sum();
            if panel_h > 0 { panel_h + 1 } else { 1 }
        };

        let [msg_area, bottom_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(bottom_height)]).areas(inner);
        let msg_area = Rect {
            x: msg_area.x + MSG_PAD_X,
            y: msg_area.y + MSG_PAD_TOP,
            width: msg_area.width.saturating_sub(MSG_PAD_X * 2),
            height: msg_area.height.saturating_sub(MSG_PAD_TOP),
        };

        let panel_reqs = if bottom_takeover {
            Vec::new()
        } else {
            self.float_mgr.panel_reqs(content)
        };

        let (queue_height, _input_height) = if bottom_takeover || !self.is_main_chat() {
            (0, 0)
        } else {
            let qh = queue_panel::height(self.queue.panel_len());
            let panel_h: u16 = panel_reqs.iter().map(|(_, h)| *h).sum();
            let input_height = bottom_area.height.saturating_sub(qh + panel_h);
            (qh, input_height)
        };

        let mut constraints = vec![Constraint::Length(queue_height)];
        for &(_, h) in &panel_reqs {
            constraints.push(Constraint::Length(h));
        }
        if show_model_row {
            constraints.push(Constraint::Length(status_bar::MODEL_ROW_HEIGHT));
        }
        constraints.push(Constraint::Min(1));

        let areas = Layout::vertical(constraints).split(bottom_area);
        let queue_area = areas[0];
        let panel_windows: Vec<(usize, Rect)> = panel_reqs
            .iter()
            .enumerate()
            .map(|(i, &(idx, _))| (idx, areas[1 + i]))
            .collect();
        let model_row_area = if show_model_row {
            areas[areas.len() - 2]
        } else {
            Rect::default()
        };
        let input_area = areas[areas.len() - 1];

        ViewLayout {
            msg_area,
            bottom_area,
            status_area,
            queue_area,
            panel_windows,
            model_row_area,
            input_area,
            splits,
            content_area: content,
            bottom_takeover,
        }
    }

    fn resolve_render_chat(&self) -> usize {
        if self.task_picker.is_open() {
            self.task_picker
                .selected_index()
                .unwrap_or(self.active_chat)
        } else {
            self.active_chat
        }
    }

    fn render_background(&self, frame: &mut Frame) {
        let bg =
            Block::default().style(ratatui::style::Style::new().bg(theme::current().background));
        bg.render(frame.area(), frame.buffer_mut());
    }

    fn render_messages(&mut self, frame: &mut Frame, layout: &ViewLayout, render_chat: usize) {
        let accent = self.effective_mode_color();
        self.chats[render_chat].set_accent(accent);
        self.chats[render_chat].view(frame, layout.msg_area, self.selection_state.is_some());
    }

    fn render_bottom_panel(&mut self, frame: &mut Frame, layout: &ViewLayout) {
        if self.permission_prompt.is_open() {
            self.permission_prompt.view(frame, layout.bottom_area);
        } else if !self.is_main_chat() {
            let panel_reqs = self.float_mgr.panel_reqs(layout.content_area);
            let panel_h: u16 = panel_reqs.iter().map(|(_, h)| *h).sum();
            let (panel_areas, sep_area) = if panel_h > 0 {
                let [panels, s] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)])
                    .areas(layout.bottom_area);
                let constraints: Vec<_> = panel_reqs
                    .iter()
                    .map(|&(_, h)| Constraint::Length(h))
                    .collect();
                let sub = Layout::vertical(constraints).split(panels);
                let areas: Vec<(usize, Rect)> = panel_reqs
                    .iter()
                    .enumerate()
                    .map(|(i, &(idx, _))| (idx, sub[i]))
                    .collect();
                (Some(areas), s)
            } else {
                (None, layout.bottom_area)
            };
            if let Some(areas) = panel_areas {
                for (idx, rect) in areas {
                    self.float_mgr.view_panel(frame, idx, rect);
                }
            }
            let sep = Block::default()
                .borders(Borders::TOP)
                .border_style(self.separator_style());
            frame.render_widget(sep, sep_area);
        } else if self.plan_form_active() {
            self.plan_form.view(frame, layout.bottom_area);
        } else if self.flow_goal_form_active() {
            self.flow_goal_form.view(frame, layout.bottom_area);
        } else if layout.bottom_area.height > 0 {
            let queue_entries = self.queue.panel_entries();
            queue_panel::view(frame, layout.queue_area, &queue_entries, self.queue.focus());
            for &(idx, rect) in &layout.panel_windows {
                self.float_mgr.view_panel(frame, idx, rect);
            }
            frame.render_widget(
                Block::default().style(Style::new().bg(theme::current().layer01)),
                layout.input_area,
            );
            let render_chat = self.resolve_render_chat();
            let ctx = self.status_bar_context(render_chat);
            status_bar::view_model_row(frame, layout.model_row_area, &ctx);
            let streaming = self.status == Status::Streaming;
            let panel_hint = (self.state.mode == Mode::Plan)
                .then(|| self.plan_form.hint_line())
                .flatten()
                .or_else(|| self.flow_goal_form.hint_line())
                .or_else(|| self.flow_status_line())
                .or_else(|| self.lua_hint_line());
            self.input_box.view(
                frame,
                layout.input_area,
                streaming,
                !self.any_overlay_open(),
                panel_hint,
            );
            self.command_palette.view(frame, layout.input_area);
        }
    }

    fn render_splits(&mut self, frame: &mut Frame, layout: &ViewLayout) {
        for dir in Split::ALL {
            if let Some(rect) = layout.splits.rect(dir) {
                self.float_mgr.view_split(frame, dir, rect);
            }
        }
    }

    fn render_picker_overlays(&mut self, frame: &mut Frame, layout: &ViewLayout) -> Rect {
        let mut overlay_rect = Rect::default();
        let full = frame.area();

        if self.search_modal.is_open() {
            overlay_rect = self.search_modal.view(frame, layout.msg_area);
        }

        if self.task_picker.is_open() {
            overlay_rect = self.task_picker.view(frame, full);
        }

        if self.file_picker.is_open() {
            if let Some(flash) = self.file_picker.tick() {
                self.status_bar.flash(flash);
            }
            overlay_rect = self.file_picker.view(frame, full);
        }

        macro_rules! render_if_open {
            ($overlay:expr) => {
                if $overlay.is_open() {
                    overlay_rect = $overlay.view(frame, full);
                }
            };
        }

        render_if_open!(self.rewind_picker);
        render_if_open!(self.theme_picker);
        render_if_open!(self.model_picker);
        render_if_open!(self.login_picker);
        render_if_open!(self.mcp_picker);
        render_if_open!(self.recipe_picker);

        overlay_rect
    }

    fn render_top_modals(&mut self, frame: &mut Frame, mut overlay_rect: Rect) -> Rect {
        let full = frame.area();
        let r = self.btw_modal.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        let r = self.help_modal.view(frame, full, &self.keybindings);
        if r.width > 0 {
            overlay_rect = r;
        }
        if self.usage_modal.is_open() {
            let quota = self.usage_slot.load();
            let ctx = crate::components::usage_modal::UsageModalContext {
                total: &self.state.token_usage,
                by_model: self.state.session.usage_by_model(),
                model: &self.state.model,
                fast: self.state.fast,
                quota: quota.as_deref(),
            };
            let r = self.usage_modal.view(frame, full, &ctx);
            if r.width > 0 {
                overlay_rect = r;
            }
        }
        let r = self.stats_modal.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        let r = self.float_mgr.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        overlay_rect
    }

    fn status_bar_context(&self, render_chat: usize) -> StatusBarContext<'_> {
        let chat = &self.chats[render_chat];
        let chat_name = (self.chats.len() > 1).then_some(chat.name.as_str());
        let (mode_label, mode_style) = self.mode_label();
        StatusBarContext {
            status: &self.status,
            mode_label,
            mode_style,
            model_id: chat
                .model_id
                .as_deref()
                .unwrap_or(&self.state.session.model),
            stats: UsageStats {
                context_size: chat.context_size,
                cost: chat.cost,
                global_cost: self.state.cost,
                context_window: self.state.model.context_window,
                show_global: self.chats.len() > 1,
            },
            auto_scroll: chat.auto_scroll(),
            chat_name,
            retry_info: self.retry_info.as_ref(),
            thinking_label: self.state.thinking.status_label(),
            fast: self.state.fast,
            restoring: self.restoring.load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    fn render_status_bar(&mut self, frame: &mut Frame, status_area: Rect, render_chat: usize) {
        let ctx = self.status_bar_context(render_chat);
        self.status_bar.view(frame, status_area, &ctx);
    }

    fn register_zones(&mut self, layout: &ViewLayout, overlay_rect: Rect) {
        // Push order = z-order. zone_at() walks in reverse, so later entries win.
        self.zones = ZoneRegistry::new();

        self.zones.push(SelectableZone {
            area: layout.msg_area,
            zone: SelectionZone::Messages,
        });

        if layout.input_area.height > 0 && !layout.bottom_takeover && self.is_main_chat() {
            let input_inner = Rect::new(
                layout.input_area.x,
                layout.input_area.y + 1,
                layout.input_area.width,
                layout.input_area.height.saturating_sub(2),
            );
            self.zones.push(SelectableZone {
                area: input_inner,
                zone: SelectionZone::Input,
            });
        }

        self.zones.push_overlay(layout.status_area);

        if self.permission_prompt.is_open()
            || self.plan_form_active()
            || self.flow_goal_form_active()
        {
            self.zones.push_overlay(layout.bottom_area);
        }

        for &(_, rect) in &layout.panel_windows {
            self.zones.push_overlay(selection::inset_border(rect));
        }

        if !self.is_main_chat() && layout.bottom_area.height > 0 {
            self.zones.push_overlay(layout.bottom_area);
        }

        if layout.queue_area.height > 0 && !layout.bottom_takeover {
            self.zones.push_overlay(layout.queue_area);
        }

        for dir in Split::ALL {
            if let Some(rect) = layout.splits.rect(dir) {
                self.zones.push_overlay(selection::inset_border(rect));
            }
        }

        if overlay_rect.width > 0 {
            self.zones
                .push_overlay(selection::inset_border(overlay_rect));
        }

        // Overlay zone was removed (e.g. dialog closed), drop the dangling selection
        if let Some(ref state) = self.selection_state
            && state.sel().zone == SelectionZone::Overlay
            && self.zones.find_area(state.sel().area).is_none()
        {
            self.selection_state = None;
        }
    }

    fn apply_selection(&mut self, frame: &mut Frame, render_chat: usize) {
        let Some(ref state) = self.selection_state else {
            return;
        };

        let sel = state.sel();
        let scroll = self.scroll_offset(sel.zone);
        if let Some(screen_sel) = sel.to_screen(scroll) {
            selection::apply_highlight(frame.buffer_mut(), sel.highlight_area(), &screen_sel);
        }
        if state.is_pending_copy() {
            let sel = *sel;
            self.copy_selection(frame.buffer_mut(), &sel, render_chat);
        }
    }

    /// Compact Flow status line for the input-box hint slot. Renders as
    /// `flow · <stage>` (plus ` · <active>/<total>` when child threads exist),
    /// or `flow · awaiting goal` at the goal-approval gate. Returns `None`
    /// outside Flow mode.
    pub(super) fn flow_status_line(&self) -> Option<Line<'static>> {
        if self.state.mode != Mode::Flow {
            return None;
        }
        let t = theme::current();
        let mut spans = vec![Span::styled("flow", t.tool_dim), Span::raw(" · ")];
        if self.flow_awaiting_approval {
            spans.push(Span::styled("awaiting goal", t.active));
        } else {
            let stage = self
                .flow_panel
                .snapshot
                .stage
                .map(|s| s.as_str())
                .unwrap_or("general");
            spans.push(Span::styled(stage, t.active));
            let total = self.flow_panel.total_threads;
            if total > 0 {
                spans.push(Span::raw(" · "));
                spans.push(Span::styled(
                    format!("{}/{}", self.flow_panel.live_threads, total),
                    t.tool_dim,
                ));
            }
        }
        Some(Line::from(spans))
    }

    fn lua_hint_line(&self) -> Option<Line<'static>> {
        let snap = self.hint_reader.load();
        if snap.entries.is_empty() {
            return None;
        }
        let mut spans = Vec::new();
        for (_, pairs) in &snap.entries {
            for (text, style_name) in pairs {
                let style = theme::style_by_name(style_name);
                spans.push(Span::styled(text.clone(), style));
            }
        }
        Some(Line::from(spans))
    }

    #[cfg(test)]
    pub(super) fn active_keybind_contexts(&self) -> Vec<KeybindContext> {
        let mut contexts = vec![KeybindContext::General];
        if self.plan_form_active() || self.flow_goal_form_active() {
            contexts.push(KeybindContext::FormInput);
        } else if self.queue.focus().is_some() {
            contexts.push(KeybindContext::QueueFocus);
        } else if self.rewind_picker.is_open() {
            contexts.push(KeybindContext::RewindPicker);
        } else if self.task_picker.is_open() {
            contexts.push(KeybindContext::TaskPicker);
        } else if self.theme_picker.is_open() {
            contexts.push(KeybindContext::ThemePicker);
        } else if self.model_picker.is_open() {
            contexts.push(KeybindContext::ModelPicker);
        } else if self.command_palette.is_active() {
            contexts.push(KeybindContext::CommandPalette);
        } else if self.search_modal.is_open() {
            contexts.push(KeybindContext::Search);
        } else if self.file_picker.is_open() {
            contexts.push(KeybindContext::FilePicker);
        } else {
            if self.status == Status::Streaming {
                contexts.push(KeybindContext::Streaming);
            }
            contexts.push(KeybindContext::Editing);
        }
        contexts
    }

    #[cfg(test)]
    pub(super) fn layout_geometry(&self, area: Rect) -> (Rect, Rect, Rect, Rect, SplitLayout) {
        let form_visible = self.permission_prompt.is_open()
            || self.plan_form_active()
            || self.flow_goal_form_active();
        let layout = self.compute_layout_raw(area, form_visible);
        (
            layout.msg_area,
            layout.bottom_area,
            layout.status_area,
            layout.input_area,
            layout.splits,
        )
    }
}
