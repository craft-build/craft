use gpui::{Context, Entity, Window, prelude::*};

use crate::state::{Comment, Screen};
use crate::text_input::TextInput;

use super::App;

#[derive(Clone)]
pub struct CommentDraft {
    pub input: Entity<TextInput>,
    pub label: String,
    pub selections: Vec<String>,
    pub scroll_anchor: Option<gpui::ScrollAnchor>,
}

impl CommentDraft {
    pub fn reference_label(&self) -> String {
        comment_reference_label(&self.label, &self.selections)
    }
}

pub(crate) fn comment_reference_label(label: &str, selections: &[String]) -> String {
    let mut reference = label.to_string();
    for selection in selections {
        reference.push_str("\nSelected text:\n");
        for line in selection.split('\n') {
            reference.push_str("> ");
            reference.push_str(line);
            reference.push('\n');
        }
    }
    reference
}

pub struct PendingComment {
    pub key: String,
    pub idx: usize,
    pub text: String,
    pub label: String,
}

pub(crate) fn comment_scope_prefix(session_id: Option<&str>) -> String {
    format!("session:{}:", session_id.unwrap_or("<none>"))
}

pub(crate) fn scoped_comment_key(session_id: Option<&str>, anchor: &str) -> String {
    format!("{}{anchor}", comment_scope_prefix(session_id))
}

impl App {
    pub fn comment_key(&self, anchor: &str) -> String {
        scoped_comment_key(self.active_session_id.as_deref(), anchor)
    }

    pub fn open_comment_box(&mut self, key: String, label: String, cx: &mut Context<Self>) {
        if self.comment_drafts.remove(&key).is_some() {
            cx.notify();
            return;
        }
        self.ensure_comment_box(key, label, cx);
    }

    fn ensure_comment_box(&mut self, key: String, label: String, cx: &mut Context<Self>) {
        if self.comment_drafts.contains_key(&key) {
            return;
        }
        let weak = cx.weak_entity();
        let submit_key = key.clone();
        let cancel_owner = weak.clone();
        let cancel_key = key.clone();
        let input = cx.new(|cx| {
            TextInput::new(cx, "Comment...")
                .soft_wrap()
                .on_cancel(move |window, cx| {
                    cancel_owner
                        .update(cx, |app, cx| {
                            app.comment_drafts.remove(&cancel_key);
                            window.focus(&app.selection_focus);
                            cx.notify();
                        })
                        .ok();
                })
                .on_submit(move |text, _window, cx| {
                    let text = text.to_string();
                    let key = submit_key.clone();
                    weak.update(cx, |app, cx| app.submit_comment(key, text, cx))
                        .ok();
                })
        });
        self.comment_drafts.insert(
            key,
            CommentDraft {
                input,
                label,
                selections: Vec::new(),
                scroll_anchor: None,
            },
        );
        cx.notify();
    }

    pub(crate) fn start_selection_comment(
        &mut self,
        target: crate::selectable_text::CommentTarget,
        selection: String,
        typed: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A selection from another session must never create a comment here.
        if self.screen != Screen::Workspace
            || !target
                .key
                .starts_with(&comment_scope_prefix(self.active_session_id.as_deref()))
        {
            return;
        }
        self.ensure_comment_box(target.key.clone(), target.label, cx);
        let draft = self.comment_drafts.get_mut(&target.key).unwrap();
        // Reuse an open draft without discarding text or earlier references.
        if !draft.selections.contains(&selection) {
            draft.selections.push(selection);
        }
        draft.input.update(cx, |input, cx| {
            input.set_content(format!("{}{typed}", input.content));
            window.focus(&input.focus_handle);
            cx.notify();
        });
        let anchor = gpui::ScrollAnchor::for_handle(target.scroll_handle);
        anchor.scroll_to(window, cx);
        draft.scroll_anchor = Some(anchor);
        cx.notify();
    }

    pub fn submit_comment(&mut self, key: String, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(draft) = self.comment_drafts.get(&key) else {
            return;
        };
        let label = draft.reference_label();
        self.comments.entry(key.clone()).or_default().push(Comment {
            author: "you".into(),
            text,
            pending: true,
            label,
        });
        self.comment_drafts.remove(&key);
        self.persist_state();
        cx.notify();
    }

    pub fn pending_comments(&self) -> Vec<PendingComment> {
        let mut out = vec![];
        let scope = comment_scope_prefix(self.active_session_id.as_deref());
        for (key, list) in self
            .comments
            .iter()
            .filter(|(key, _)| key.starts_with(&scope))
        {
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
        if let Some(list) = self.comments.get_mut(key)
            && idx < list.len()
        {
            list.remove(idx);
        }
        self.persist_state();
        cx.notify();
    }
}
