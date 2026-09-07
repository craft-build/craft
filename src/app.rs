use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui::prelude::*;
use gpui::{Context, Entity, ScrollHandle, Window, div, px, rgb};

use crate::async_runtime;
use crate::screens;
use crate::state::*;
use crate::text_input::TextInput;
use crate::theme;

pub struct PendingComment {
    pub key: String,
    pub idx: usize,
    pub text: String,
    pub label: String,
}

pub struct App {
    pub screen: Screen,
    pub projects: Vec<Project>,
    pub sessions_by_project: HashMap<String, Vec<Session>>,
    pub active_project: Option<Project>,
    pub active_session_id: Option<String>,

    pub selected_model: String,
    pub model_menu_open: bool,

    pub sidebar_visible: bool,
    pub file_tree_visible: bool,
    pub collapsed_projects: HashSet<String>,

    pub active_diff_file: Option<String>,
    pub file_diffs: HashMap<String, Diff>,

    pub composer: Entity<TextInput>,
    pub context_chips: Vec<String>,
    pub thinking: bool,
    pub reply_generation: u64,
    pub sent_count: usize,

    pub show_checkpoints: bool,
    pub toast: Option<String>,
    pub toast_generation: u64,

    pub expanded_steps: HashSet<String>,
    pub open_comment_boxes: HashSet<String>,
    pub comment_inputs: HashMap<String, Entity<TextInput>>,
    pub comments: HashMap<String, Vec<Comment>>,

    pub thread_scroll: ScrollHandle,
}

impl App {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let weak = cx.weak_entity();
        let composer = cx.new(|cx| {
            TextInput::new(cx, "Message the agent... (Cmd+Enter to send)")
                .multiline()
                .on_submit(move |text, _window, cx| {
                    let text = text.to_string();
                    weak.update(cx, |app, cx| app.send_message(text, cx)).ok();
                })
        });

        App {
            screen: Screen::Onboarding,
            projects: seed_projects(),
            sessions_by_project: seed_sessions(),
            active_project: None,
            active_session_id: None,
            selected_model: MODEL_NAMES[0].to_string(),
            model_menu_open: false,
            sidebar_visible: true,
            file_tree_visible: true,
            collapsed_projects: HashSet::new(),
            active_diff_file: None,
            file_diffs: seed_file_diffs(),
            composer,
            context_chips: vec![],
            thinking: false,
            reply_generation: 0,
            sent_count: 0,
            show_checkpoints: false,
            toast: None,
            toast_generation: 0,
            expanded_steps: HashSet::new(),
            open_comment_boxes: HashSet::new(),
            comment_inputs: HashMap::new(),
            comments: seed_comments(),
            thread_scroll: ScrollHandle::new(),
        }
    }

    // ---- navigation ----

    pub fn go_projects(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::Projects;
        cx.notify();
    }

    pub fn go_settings(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::Settings;
        cx.notify();
    }

    pub fn go_back_from_settings(&mut self, cx: &mut Context<Self>) {
        self.screen = if self.active_project.is_some() {
            Screen::Workspace
        } else {
            Screen::Projects
        };
        cx.notify();
    }

    pub fn open_project(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(project) = self.projects.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        let session_id = self
            .sessions_by_project
            .get(id)
            .and_then(|s| s.first())
            .map(|s| s.id.clone());
        self.active_project = Some(project);
        self.active_session_id = session_id;
        self.show_checkpoints = false;
        self.context_chips.clear();
        self.screen = Screen::Workspace;
        cx.notify();
    }

    pub fn open_session(&mut self, project_id: &str, session_id: &str, cx: &mut Context<Self>) {
        let Some(project) = self.projects.iter().find(|p| p.id == project_id).cloned() else {
            return;
        };
        self.active_project = Some(project);
        self.active_session_id = Some(session_id.to_string());
        self.show_checkpoints = false;
        self.context_chips.clear();
        self.screen = Screen::Workspace;
        cx.notify();
    }

    pub fn add_session_to_project(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let Some(project) = self.projects.iter().find(|p| p.id == project_id).cloned() else {
            return;
        };
        let new_id = format!("s{}", now_ms());
        let sessions = self
            .sessions_by_project
            .entry(project_id.to_string())
            .or_default();
        sessions.push(Session {
            id: new_id.clone(),
            name: "New session".into(),
            messages: vec![],
        });
        self.active_project = Some(project);
        self.active_session_id = Some(new_id);
        self.show_checkpoints = false;
        self.context_chips.clear();
        self.screen = Screen::Workspace;
        cx.notify();
    }

    pub fn new_session(&mut self, cx: &mut Context<Self>) {
        let project = Project {
            id: "untitled".into(),
            name: "untitled-session".into(),
            path: "~/dev/untitled-session".into(),
            desc: String::new(),
            updated: String::new(),
            checkpoint_label: String::new(),
            model: self.selected_model.clone(),
        };
        self.sessions_by_project.insert(
            "untitled".to_string(),
            vec![Session {
                id: "s1".into(),
                name: "New session".into(),
                messages: vec![],
            }],
        );
        self.active_project = Some(project);
        self.active_session_id = Some("s1".into());
        self.show_checkpoints = false;
        self.context_chips.clear();
        self.screen = Screen::Workspace;
        cx.notify();
    }

    // ---- workspace chrome ----

    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        cx.notify();
    }

    pub fn toggle_file_tree(&mut self, cx: &mut Context<Self>) {
        self.file_tree_visible = !self.file_tree_visible;
        cx.notify();
    }

    pub fn toggle_project_collapse(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.collapsed_projects.insert(id.to_string()) {
            self.collapsed_projects.remove(id);
        }
        cx.notify();
    }

    pub fn toggle_model_menu(&mut self, cx: &mut Context<Self>) {
        self.model_menu_open = !self.model_menu_open;
        cx.notify();
    }

    pub fn select_model(&mut self, name: &str, cx: &mut Context<Self>) {
        self.selected_model = name.to_string();
        self.model_menu_open = false;
        cx.notify();
    }

    pub fn toggle_checkpoints(&mut self, cx: &mut Context<Self>) {
        self.show_checkpoints = !self.show_checkpoints;
        cx.notify();
    }

    pub fn toggle_diff_file(&mut self, path: &str, cx: &mut Context<Self>) {
        self.active_diff_file = if self.active_diff_file.as_deref() == Some(path) {
            None
        } else {
            Some(path.to_string())
        };
        cx.notify();
    }

    pub fn toggle_steps(&mut self, msg_id: &str, cx: &mut Context<Self>) {
        if !self.expanded_steps.insert(msg_id.to_string()) {
            self.expanded_steps.remove(msg_id);
        }
        cx.notify();
    }

    // ---- comments ----

    pub fn open_comment_box(&mut self, key: String, label: String, cx: &mut Context<Self>) {
        if !self.open_comment_boxes.insert(key.clone()) {
            self.open_comment_boxes.remove(&key);
            self.comment_inputs.remove(&key);
            cx.notify();
            return;
        }
        let weak = cx.weak_entity();
        let submit_key = key.clone();
        let input = cx.new(|cx| {
            TextInput::new(cx, "Comment...").on_submit(move |text, _window, cx| {
                let text = text.to_string();
                let key = submit_key.clone();
                let label = label.clone();
                weak.update(cx, |app, cx| app.submit_comment(key, text, label, cx))
                    .ok();
            })
        });
        self.comment_inputs.insert(key, input);
        cx.notify();
    }

    pub fn submit_comment(&mut self, key: String, text: String, label: String, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.comments.entry(key.clone()).or_default().push(Comment {
            author: "you".into(),
            text,
            pending: true,
            label,
        });
        self.open_comment_boxes.remove(&key);
        self.comment_inputs.remove(&key);
        cx.notify();
    }

    pub fn pending_comments(&self) -> Vec<PendingComment> {
        let mut out = vec![];
        for (key, list) in &self.comments {
            for (idx, c) in list.iter().enumerate() {
                if c.pending {
                    out.push(PendingComment {
                        key: key.clone(),
                        idx,
                        text: c.text.clone(),
                        label: c.label.clone(),
                    });
                }
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key).then(a.idx.cmp(&b.idx)));
        out
    }

    pub fn remove_pending_comment(&mut self, key: &str, idx: usize, cx: &mut Context<Self>) {
        if let Some(list) = self.comments.get_mut(key) {
            if idx < list.len() {
                list.remove(idx);
            }
        }
        cx.notify();
    }

    // ---- thread / messages ----

    fn current_thread_key(&self) -> String {
        self.active_project
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_else(|| "untitled".to_string())
    }

    pub fn active_messages(&self) -> Vec<Message> {
        let key = self.current_thread_key();
        let sessions = self.sessions_by_project.get(&key);
        let sess = sessions.and_then(|list| {
            list.iter()
                .find(|s| Some(&s.id) == self.active_session_id.as_ref())
                .or_else(|| list.first())
        });
        sess.map(|s| s.messages.clone()).unwrap_or_default()
    }

    fn update_active_messages(&mut self, new_messages: Vec<Message>) {
        let key = self.current_thread_key();
        if let Some(sessions) = self.sessions_by_project.get_mut(&key) {
            if let Some(sess) = sessions
                .iter_mut()
                .find(|s| Some(&s.id) == self.active_session_id.as_ref())
            {
                sess.messages = new_messages;
            }
        }
    }

    pub fn send_message(&mut self, text: String, cx: &mut Context<Self>) {
        let pending = self.pending_comments();
        let text = text.trim().to_string();
        if text.is_empty() && pending.is_empty() {
            return;
        }
        let mut thread = self.active_messages();
        let chips = self.context_chips.clone();
        let attached_comments = pending
            .iter()
            .map(|p| (p.label.clone(), p.text.clone()))
            .collect();
        let id = format!("u{}", now_ms());
        thread.push(Message {
            id,
            role: Role::User,
            text,
            time: None,
            context: chips,
            attached_comments,
            checkpoint_label: None,
            steps: None,
            diff: None,
            terminal: None,
        });
        let sent_count = self.sent_count;
        self.update_active_messages(thread);

        for p in &pending {
            if let Some(list) = self.comments.get_mut(&p.key) {
                if let Some(c) = list.get_mut(p.idx) {
                    c.pending = false;
                }
            }
        }

        self.context_chips.clear();
        self.thinking = true;
        self.sent_count += 1;
        self.reply_generation += 1;
        let generation = self.reply_generation;

        let rx = async_runtime::delay(Duration::from_millis(1200));
        cx.spawn(async move |this, cx| {
            let _ = rx.await;
            this.update(cx, |app, cx| app.apply_reply(generation, sent_count, cx))
                .ok();
        })
        .detach();

        cx.notify();
    }

    fn apply_reply(&mut self, generation: u64, sent_count: usize, cx: &mut Context<Self>) {
        if generation != self.reply_generation {
            return;
        }
        let canned = canned_replies();
        let reply = &canned[sent_count % canned.len()];
        let checkpoint_label = reply
            .diff
            .as_ref()
            .map(|d| format!("Checkpoint {} · {}", self.checkpoints_for().len() + 1, d.file));
        let id = format!("a{}", now_ms());
        let mut thread = self.active_messages();
        thread.push(Message {
            id,
            role: Role::Assistant,
            text: reply.text.to_string(),
            time: Some("Just now".into()),
            context: vec![],
            attached_comments: vec![],
            checkpoint_label,
            steps: reply.steps.clone(),
            diff: reply.diff.clone(),
            terminal: reply.terminal.clone(),
        });
        self.update_active_messages(thread);
        self.thinking = false;
        cx.notify();
    }

    /// Checkpoints in chronological order (oldest first) — matches the JS
    /// `checkpointsFor()`. Callers that want most-recent-first (the
    /// checkpoint history panel) reverse this themselves.
    pub fn checkpoints_for(&self) -> Vec<(String, String)> {
        self.active_messages()
            .into_iter()
            .filter(|m| matches!(m.role, Role::Assistant) && m.checkpoint_label.is_some())
            .map(|m| (m.checkpoint_label.unwrap(), m.time.unwrap_or_default()))
            .collect()
    }

    pub fn restore_checkpoint(&mut self, label: &str, cx: &mut Context<Self>) {
        self.toast = Some(format!("Restored to {label}"));
        self.show_checkpoints = false;
        self.toast_generation += 1;
        let generation = self.toast_generation;

        let rx = async_runtime::delay(Duration::from_millis(2200));
        cx.spawn(async move |this, cx| {
            let _ = rx.await;
            this.update(cx, |app, cx| {
                if generation == app.toast_generation {
                    app.toast = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();

        cx.notify();
    }

    // ---- diff / files ----

    pub fn changed_files(&self) -> &'static [FileEntry] {
        CHANGED_FILES
    }
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::TEXT_PRIMARY))
            .font_family(theme::FONT_FAMILY)
            .text_size(px(13.))
            .overflow_hidden()
            .child(match self.screen {
                Screen::Onboarding => {
                    screens::onboarding::render(self, window, cx).into_any_element()
                }
                Screen::Projects => screens::projects::render(self, window, cx).into_any_element(),
                Screen::Workspace => {
                    screens::workspace::render(self, window, cx).into_any_element()
                }
                Screen::Settings => screens::settings::render(self, window, cx).into_any_element(),
            })
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
