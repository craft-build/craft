//! App-wide UI state: signals bundled into [`AppState`], the ACP-derived data
//! model ([`Tab`], [`ChatItem`]), and the reducers that fold backend events
//! into them. Ports the old React store (`store.tsx` + `App.tsx`).

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use dioxus::prelude::*;
use serde_json::Value;

use crate::backend::{Backend, Event, SessionSummary};

static ITEM_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    ITEM_SEQ.fetch_add(1, Ordering::Relaxed)
}

fn new_tab_id() -> String {
    craft_storage::id::CraftId::generate().to_string()
}

// ------------------------------------------------------------------ UI theme

/// Applies the named theme (persist + syntax swap) and records it in state.
pub fn set_theme(mut s: AppState, name: &str) {
    crate::theme::apply_theme(name);
    s.theme.set(name.to_string());
}

pub const SUGGESTIONS: [(&str, &str); 3] = [
    (
        "Explain this repo",
        "Give me a tour of this repo — the entry points and where the agent loop lives.",
    ),
    (
        "Fix a failing test",
        "A test is failing on CI but passes locally. Find out why.",
    ),
    (
        "Review the last commit",
        "Review the last commit on main and flag anything risky.",
    ),
];

/// Modes offered before the server reports `availableModes`.
pub const DEFAULT_MODES: [&str; 3] = ["build", "plan", "flow"];

pub fn mode_label(mode: &str) -> &str {
    match mode {
        "plan" => "Plan",
        "flow" => "Flow",
        _ => "Build",
    }
}

// -------------------------------------------------------------- data model

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    /// The "Start a task" landing screen.
    New,
    /// An active session transcript.
    Session,
    /// The Skills section: browse and manage on-disk skills.
    Skills,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Popover {
    None,
    Theme,
    Mode,
    Model,
    Perm,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl ToolStatus {
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            ToolStatus::Pending => "pending",
            ToolStatus::InProgress => "running",
            ToolStatus::Completed => "done",
            ToolStatus::Failed => "failed",
        }
    }
    #[allow(dead_code)]
    pub fn pill_class(self) -> &'static str {
        match self {
            ToolStatus::Pending => "pill-waiting",
            ToolStatus::InProgress => "pill-running",
            ToolStatus::Completed => "pill-done",
            ToolStatus::Failed => "pill-failed",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "in_progress" => ToolStatus::InProgress,
            "completed" => ToolStatus::Completed,
            "failed" => ToolStatus::Failed,
            _ => ToolStatus::Pending,
        }
    }
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct ToolDiff {
    pub path: String,
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Clone, PartialEq, Debug)]
pub enum ToolContent {
    Text(String),
    Diff(ToolDiff),
}

#[derive(Clone, PartialEq, Debug)]
pub struct ToolCall {
    pub tool_call_id: String,
    pub title: String,
    pub kind: String,
    pub status: ToolStatus,
    pub content: Vec<ToolContent>,
    pub locations: Vec<String>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Clone, PartialEq, Debug)]
pub struct PermissionItem {
    pub request_id: Value,
    pub title: String,
    pub options: Vec<PermissionOption>,
    pub resolved: bool,
    pub chosen_option_id: Option<String>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct QuestionSpec {
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
}

#[derive(Clone, PartialEq, Debug)]
pub struct QuestionItem {
    pub request_id: Value,
    pub questions: Vec<QuestionSpec>,
    pub resolved: bool,
}

/// One transcript item, folded from the `SessionUpdate` stream. Mirrors the
/// old `ChatItem` union in `types.ts`.
#[derive(Clone, PartialEq, Debug)]
pub enum ChatItem {
    User { id: u64, text: String },
    Assistant { id: u64, text: String },
    Thinking { id: u64, text: String },
    Tool { id: u64, call: ToolCall },
    Permission { id: u64, item: PermissionItem },
    Question { id: u64, item: QuestionItem },
}

impl ChatItem {
    pub fn id(&self) -> u64 {
        match self {
            ChatItem::User { id, .. }
            | ChatItem::Assistant { id, .. }
            | ChatItem::Thinking { id, .. }
            | ChatItem::Tool { id, .. }
            | ChatItem::Permission { id, .. }
            | ChatItem::Question { id, .. } => *id,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanState {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, PartialEq, Debug)]
pub struct PlanEntry {
    pub content: String,
    pub status: PlanState,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Clone, PartialEq, Debug)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
    pub parent: Option<String>,
}

/// Flattens the todo tree by parent links (unmatched parents surface at top).
pub fn flatten_todos(items: &[TodoItem]) -> Vec<(usize, TodoItem)> {
    let ids: HashSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
    let mut visited = vec![false; items.len()];
    let mut out = Vec::new();

    fn visit(
        items: &[TodoItem],
        ids: &HashSet<&str>,
        visited: &mut Vec<bool>,
        out: &mut Vec<(usize, TodoItem)>,
        parent: Option<&str>,
        depth: usize,
    ) {
        for (i, item) in items.iter().enumerate() {
            if visited[i] {
                continue;
            }
            let belongs = match parent {
                None => item.parent.as_deref().is_none_or(|p| !ids.contains(p)),
                Some(p) => item.parent.as_deref() == Some(p),
            };
            if !belongs {
                continue;
            }
            visited[i] = true;
            out.push((depth, item.clone()));
            if !item.id.is_empty() {
                visit(items, ids, visited, out, Some(&item.id), depth + 1);
            }
        }
    }

    visit(items, &ids, &mut visited, &mut out, None, 0);
    for (i, item) in items.iter().enumerate() {
        if !visited[i] {
            out.push((0, item.clone()));
        }
    }
    out
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct ModeOption {
    pub id: String,
    pub name: String,
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    pub current_value: String,
    pub options: Vec<(String, String)>,
}

/// Per-session state. Mirrors the old `TabState`.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Tab {
    pub id: String,
    pub session_id: Option<String>,
    pub cwd: String,
    pub title: String,
    pub mode: String,
    pub modes: Vec<ModeOption>,
    pub config_options: Vec<ConfigOption>,
    pub items: Vec<ChatItem>,
    pub plan: Vec<PlanEntry>,
    pub todos: Vec<TodoItem>,
    pub pending: bool,
    pub connection_error: Option<String>,
    pub context_used: u64,
    pub context_size: u64,
    pub session_cost: f64,
    pub commands: Option<Value>,
}

impl Tab {
    pub fn config(&self, id: &str) -> Option<&ConfigOption> {
        self.config_options.iter().find(|c| c.id == id)
    }

    pub fn mode_label(&self) -> &str {
        self.modes
            .iter()
            .find(|m| m.id == self.mode)
            .map_or_else(|| mode_label(&self.mode), |m| m.name.as_str())
    }

    pub fn project(&self) -> String {
        self.cwd
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(&self.cwd)
            .to_string()
    }

    /// Every diff carried by this tab's tool calls, for the changes panel.
    pub fn diffs(&self) -> Vec<ToolDiff> {
        self.items
            .iter()
            .filter_map(|i| match i {
                ChatItem::Tool { call, .. } => Some(call),
                _ => None,
            })
            .flat_map(|c| {
                c.content
                    .iter()
                    .filter_map(|t| match t {
                        ToolContent::Diff(d) => Some(d.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn status_dot(&self) -> &'static str {
        if self
            .items
            .iter()
            .any(|i| matches!(i, ChatItem::Permission { item, .. } if !item.resolved))
        {
            return "var(--status-warning)";
        }
        if self.pending {
            return "var(--status-info)";
        }
        "var(--status-success)"
    }
}

// ------------------------------------------------------------- app signals

#[derive(Clone, Copy)]
pub struct AppState {
    pub tabs: Signal<Vec<Tab>>,
    pub active_id: Signal<String>,
    pub view: Signal<View>,
    pub panel_open: Signal<bool>,
    pub plan_open: Signal<bool>,
    pub todo_open: Signal<bool>,
    pub palette_open: Signal<bool>,
    pub palette_selected: Signal<usize>,
    pub query: Signal<String>,
    /// Project dir the "Start a task" screen will start the session in.
    pub new_task_cwd: Signal<Option<String>>,
    pub popover: Signal<Popover>,
    pub theme: Signal<String>,
    /// Default mode for sessions started before the server reports modes.
    pub mode: Signal<String>,
    /// Permission policy for new sessions: yolo / auto-review toggles.
    pub yolo: Signal<bool>,
    pub auto_review: Signal<bool>,
    pub draft: Signal<String>,
    pub focused: Signal<bool>,
    pub open_files: Signal<HashSet<String>>,
    pub expanded_tools: Signal<HashSet<u64>>,
    /// Sidebar "Recent" persisted sessions.
    pub history: Signal<Vec<SessionSummary>>,
    pub history_open: Signal<bool>,
    /// Onboarding / start-session state.
    pub start_error: Signal<Option<String>>,
    pub starting: Signal<bool>,
    /// Question card selections, keyed by item id.
    pub selections: Signal<Vec<(u64, Vec<Vec<usize>>)>>,
    /// Skills section: skills visible from the current project context.
    pub skills: Signal<Vec<crate::skills::Skill>>,
    /// Skills section: last list/save/delete error to surface to the user.
    pub skills_error: Signal<Option<String>>,
    /// Skills section: open editor form state, `None` when browsing.
    pub skill_editor: Signal<Option<crate::skills::SkillDraft>>,
    /// Skills section: skill queued for deletion until the user confirms.
    pub skill_delete: Signal<Option<crate::skills::Skill>>,
    /// Skills section: the create-with-AI panel is open.
    pub skill_ai_open: Signal<bool>,
}

pub fn provide_state() -> AppState {
    let state = AppState {
        tabs: use_signal(Vec::new),
        active_id: use_signal(String::new),
        view: use_signal(|| View::New),
        panel_open: use_signal(|| false),
        plan_open: use_signal(|| true),
        todo_open: use_signal(|| false),
        palette_open: use_signal(|| false),
        palette_selected: use_signal(|| 0usize),
        query: use_signal(String::new),
        new_task_cwd: use_signal(|| None),
        popover: use_signal(|| Popover::None),
        theme: use_signal(crate::theme::current_theme_name),
        mode: use_signal(|| "build".to_string()),
        yolo: use_signal(|| false),
        auto_review: use_signal(|| false),
        draft: use_signal(String::new),
        focused: use_signal(|| false),
        open_files: use_signal(HashSet::new),
        expanded_tools: use_signal(HashSet::new),
        history: use_signal(Vec::new),
        history_open: use_signal(|| false),
        start_error: use_signal(|| None),
        starting: use_signal(|| false),
        selections: use_signal(Vec::new),
        skills: use_signal(Vec::new),
        skills_error: use_signal(|| None),
        skill_editor: use_signal(|| None),
        skill_delete: use_signal(|| None),
        skill_ai_open: use_signal(|| false),
    };
    use_context_provider(|| state);
    state
}

pub fn active_tab(s: AppState) -> Option<Tab> {
    let id = s.active_id.read().clone();
    let tabs = s.tabs.read();
    tabs.iter()
        .find(|t| t.id == id)
        .or_else(|| tabs.first())
        .cloned()
}

fn with_tab<R>(s: AppState, tab_id: &str, f: impl FnOnce(&mut Tab) -> R) -> Option<R> {
    let mut tabs = s.tabs.write_unchecked();
    tabs.iter_mut().find(|t| t.id == tab_id).map(f)
}

// ------------------------------------------------------------------ events

/// Folds one backend event into the signals. Drives the entire session UI.
pub fn apply_event(mut s: AppState, backend: &Backend, event: Event) {
    match event {
        Event::SessionUpdate { tab_id, update } => {
            with_tab(s, &tab_id, |t| apply_session_update(t, &update));
            scroll_transcript_down();
        }
        Event::Permission {
            tab_id,
            request_id,
            params,
        } => {
            let title = params
                .pointer("/toolCall/title")
                .and_then(Value::as_str)
                .unwrap_or("Permission requested")
                .to_string();
            let options = params
                .get("options")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|o| {
                            Some(PermissionOption {
                                option_id: o.get("optionId")?.as_str()?.to_string(),
                                name: o
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                kind: o
                                    .get("kind")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            with_tab(s, &tab_id, |t| {
                t.items.push(ChatItem::Permission {
                    id: next_id(),
                    item: PermissionItem {
                        request_id,
                        title,
                        options,
                        resolved: false,
                        chosen_option_id: None,
                    },
                });
            });
            scroll_transcript_down();
        }
        Event::Question {
            tab_id,
            request_id,
            params,
        } => {
            let questions = schema_to_questions(params.pointer("/requestedSchema/properties"));
            with_tab(s, &tab_id, |t| {
                t.items.push(ChatItem::Question {
                    id: next_id(),
                    item: QuestionItem {
                        request_id,
                        questions,
                        resolved: false,
                    },
                });
            });
            scroll_transcript_down();
        }
        Event::Todos { tab_id, todos } => {
            let parsed = todos
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|t| {
                            Some(TodoItem {
                                id: t.get("id").and_then(Value::as_str)?.to_string(),
                                content: t
                                    .get("content")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                status: match t.get("status").and_then(Value::as_str) {
                                    Some("in_progress") => TodoStatus::InProgress,
                                    Some("completed") => TodoStatus::Completed,
                                    Some("cancelled") => TodoStatus::Cancelled,
                                    _ => TodoStatus::Pending,
                                },
                                parent: t.get("parent").and_then(Value::as_str).map(str::to_string),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            with_tab(s, &tab_id, |t| t.todos = parsed);
        }
        Event::PromptDone { tab_id, ok, error } => {
            with_tab(s, &tab_id, |t| {
                t.pending = false;
                if !ok {
                    t.connection_error = Some(error.unwrap_or_else(|| "Prompt failed".to_string()));
                }
            });
        }
        Event::Closed { tab_id } => {
            with_tab(s, &tab_id, |t| {
                t.pending = false;
                if t.connection_error.is_none() {
                    t.connection_error = Some("Session ended unexpectedly".to_string());
                }
            });
        }
        Event::SessionStarted {
            tab_id,
            cwd,
            result,
        } => {
            s.starting.set(false);
            match result {
                Ok(resp) => {
                    let modes = resp
                        .pointer("/modes/availableModes")
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter()
                                .filter_map(|m| {
                                    Some(ModeOption {
                                        id: m.get("id")?.as_str()?.to_string(),
                                        name: m
                                            .get("name")
                                            .and_then(Value::as_str)
                                            .unwrap_or_default()
                                            .to_string(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let current_mode = resp
                        .pointer("/modes/currentModeId")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let config_options = parse_config_options(resp.get("configOptions"));
                    let session_id = resp
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let existing = s.tabs.read().iter().any(|t| t.id == tab_id);
                    let title = resp
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| basename(&cwd));
                    if existing {
                        with_tab(s, &tab_id, |t| {
                            t.session_id = Some(session_id);
                            t.modes = modes;
                            if let Some(mode) = current_mode {
                                t.mode = mode;
                            }
                            t.config_options = config_options;
                            t.cwd = cwd;
                            t.title = title;
                        });
                    } else {
                        s.tabs.write_unchecked().push(Tab {
                            id: tab_id.clone(),
                            session_id: Some(session_id),
                            mode: current_mode.unwrap_or_default(),
                            cwd,
                            title,
                            modes,
                            config_options,
                            ..Default::default()
                        });
                        s.active_id.set(tab_id.clone());
                        s.view.set(View::Session);
                    }
                    backend.list_commands(tab_id);
                    scroll_transcript_down();
                }
                Err(e) => {
                    s.tabs.with_mut(|tabs| tabs.retain(|t| t.id != tab_id));
                    s.start_error.set(Some(e));
                }
            }
        }
        Event::SessionsListed { sessions } => s.history.set(sessions),
        Event::CommandsListed { tab_id, commands } => {
            with_tab(s, &tab_id, |t| t.commands = Some(commands));
        }
    }
}

fn basename(path: &str) -> String {
    path.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn parse_config_options(value: Option<&Value>) -> Vec<ConfigOption> {
    let Some(list) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|c| {
            Some(ConfigOption {
                id: c.get("id")?.as_str()?.to_string(),
                name: c
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                current_value: c
                    .get("currentValue")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                options: c
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|opts| {
                        // Menu items are keyed by value; drop duplicate values
                        // the server may send.
                        let mut seen = std::collections::HashSet::new();
                        opts.iter()
                            .filter_map(|o| {
                                let value = o.get("value")?.as_str()?;
                                seen.insert(value.to_string()).then(|| {
                                    (
                                        value.to_string(),
                                        o.get("name")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                    )
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Rebuilds a question card's list from an elicitation form schema. The
/// server maps questions to positional keys (`q1`, `q2`, ...) with
/// string/array properties; enum option titles carry the description as
/// `"label - description"`, so it is split back out.
fn schema_to_questions(props: Option<&Value>) -> Vec<QuestionSpec> {
    let Some(map) = props.and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort_by_key(|k| {
        k.chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u64>()
            .unwrap_or(0)
    });
    keys.iter()
        .map(|k| {
            let p = &map[k.as_str()];
            let one_of = p
                .get("oneOf")
                .and_then(Value::as_array)
                .map(|v| v.as_slice())
                .or_else(|| {
                    p.pointer("/items/anyOf")
                        .and_then(Value::as_array)
                        .map(|v| v.as_slice())
                })
                .unwrap_or(&[]);
            QuestionSpec {
                question: p
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                options: one_of
                    .iter()
                    .filter_map(|o| {
                        let label = o.get("const")?.as_str()?.to_string();
                        let description = o
                            .get("title")
                            .and_then(Value::as_str)
                            .and_then(|t| t.strip_prefix(&format!("{label} - ")))
                            .map(str::to_string);
                        Some(QuestionOption { label, description })
                    })
                    .collect(),
                multi_select: p.get("type").and_then(Value::as_str) == Some("array"),
            }
        })
        .collect()
}

fn content_text(content: &Value) -> String {
    match content.get("type").and_then(Value::as_str) {
        Some("text") => content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn parse_tool_content(value: Option<&Value>) -> Vec<ToolContent> {
    let Some(list) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|c| match c.get("type").and_then(Value::as_str) {
            Some("diff") => Some(ToolContent::Diff(ToolDiff {
                path: c
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                old_text: c.get("oldText").and_then(Value::as_str).map(str::to_string),
                new_text: c
                    .get("newText")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })),
            Some("content") => {
                let text = content_text(c.get("content").unwrap_or(&Value::Null));
                (!text.is_empty()).then_some(ToolContent::Text(text))
            }
            _ => None,
        })
        .collect()
}

fn parse_locations(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|l| l.get("path").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Ports `applySessionUpdate` from the old store: folds one
/// `session/update` notification into the tab's item stream.
pub fn apply_session_update(tab: &mut Tab, update: &Value) {
    let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
        return;
    };
    let items = &mut tab.items;
    match kind {
        "user_message_chunk" => {
            let text = content_text(update.get("content").unwrap_or(&Value::Null));
            match items.last_mut() {
                Some(ChatItem::User { text: t, .. }) => t.push_str(&text),
                _ => items.push(ChatItem::User {
                    id: next_id(),
                    text,
                }),
            }
        }
        "agent_message_chunk" => {
            let text = content_text(update.get("content").unwrap_or(&Value::Null));
            match items.last_mut() {
                Some(ChatItem::Assistant { text: t, .. }) => t.push_str(&text),
                _ => items.push(ChatItem::Assistant {
                    id: next_id(),
                    text,
                }),
            }
        }
        "agent_thought_chunk" => {
            let text = content_text(update.get("content").unwrap_or(&Value::Null));
            match items.last_mut() {
                Some(ChatItem::Thinking { text: t, .. }) => t.push_str(&text),
                _ => items.push(ChatItem::Thinking {
                    id: next_id(),
                    text,
                }),
            }
        }
        "tool_call" => {
            items.push(ChatItem::Tool {
                id: next_id(),
                call: ToolCall {
                    tool_call_id: update
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    title: update
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| {
                            update
                                .get("toolCallId")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                        })
                        .to_string(),
                    kind: update
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("other")
                        .to_string(),
                    status: update
                        .get("status")
                        .and_then(Value::as_str)
                        .map(ToolStatus::parse)
                        .unwrap_or(ToolStatus::Pending),
                    content: parse_tool_content(update.get("content")),
                    locations: parse_locations(update.get("locations")),
                },
            });
        }
        "tool_call_update" => {
            let tool_call_id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let existing = items.iter_mut().find_map(|i| match i {
                ChatItem::Tool { call, .. } if call.tool_call_id == tool_call_id => Some(call),
                _ => None,
            });
            match existing {
                Some(call) => {
                    if let Some(title) = update
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                    {
                        call.title = title.to_string();
                    }
                    if let Some(status) = update.get("status").and_then(Value::as_str) {
                        call.status = ToolStatus::parse(status);
                    }
                    let content = parse_tool_content(update.get("content"));
                    if !content.is_empty() {
                        call.content = content;
                    }
                    let locations = parse_locations(update.get("locations"));
                    if !locations.is_empty() {
                        call.locations = locations;
                    }
                }
                None => items.push(ChatItem::Tool {
                    id: next_id(),
                    call: ToolCall {
                        tool_call_id: tool_call_id.to_string(),
                        title: update
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or(tool_call_id)
                            .to_string(),
                        kind: update
                            .get("kind")
                            .and_then(Value::as_str)
                            .unwrap_or("other")
                            .to_string(),
                        status: ToolStatus::InProgress,
                        content: parse_tool_content(update.get("content")),
                        locations: parse_locations(update.get("locations")),
                    },
                }),
            }
        }
        "plan" => {
            tab.plan = update
                .get("entries")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|e| {
                            Some(PlanEntry {
                                content: e.get("content").and_then(Value::as_str)?.to_string(),
                                status: match e.get("status").and_then(Value::as_str) {
                                    Some("in_progress") => PlanState::InProgress,
                                    Some("completed") => PlanState::Completed,
                                    _ => PlanState::Pending,
                                },
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        "current_mode_update" => {
            if let Some(mode) = update.get("currentModeId").and_then(Value::as_str) {
                tab.mode = mode.to_string();
            }
        }
        "config_option_update" => {
            tab.config_options = parse_config_options(update.get("configOptions"));
        }
        "session_info_update" => {
            if let Some(title) = update
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                tab.title = title.to_string();
            }
        }
        "usage_update" => {
            tab.context_used = update
                .get("used")
                .and_then(Value::as_u64)
                .unwrap_or(tab.context_used);
            tab.context_size = update
                .get("size")
                .and_then(Value::as_u64)
                .unwrap_or(tab.context_size);
            if let Some(cost) = update.pointer("/cost/amount").and_then(Value::as_f64) {
                tab.session_cost = cost;
            }
        }
        _ => {}
    }
}

// ------------------------------------------------------------------ actions

pub fn open_new_task(s: AppState) {
    let cwd = active_tab(s).map(|t| t.cwd);
    show_new_task(s, cwd);
}

/// New-task composer pinned to a specific project (sidebar project "+").
pub fn open_new_task_in(s: AppState, cwd: String) {
    show_new_task(s, Some(cwd));
}

fn show_new_task(mut s: AppState, cwd: Option<String>) {
    s.new_task_cwd.set(cwd);
    s.view.set(View::New);
    s.panel_open.set(false);
    s.palette_open.set(false);
    s.draft.set(String::new());
}

pub fn select_task(mut s: AppState, id: String) {
    s.view.set(View::Session);
    s.active_id.set(id);
    s.palette_open.set(false);
}

pub fn toggle_popover(mut s: AppState, p: Popover) {
    let cur = *s.popover.read();
    s.popover.set(if cur == p { Popover::None } else { p });
}

pub fn toggle_file(mut s: AppState, path: &str) {
    let key = path.to_string();
    s.open_files.with_mut(|set| {
        if set.contains(&key) {
            set.remove(&key);
        } else {
            set.insert(key);
        }
    });
}

pub fn toggle_tool(mut s: AppState, id: u64) {
    s.expanded_tools.with_mut(|set| {
        if set.contains(&id) {
            set.remove(&id);
        } else {
            set.insert(id);
        }
    });
}

/// Resolve a permission request (or cancel it with `None`).
pub fn respond_permission(
    s: AppState,
    backend: &Backend,
    tab: &str,
    item_id: u64,
    option_id: Option<String>,
) {
    let (request_id, chosen) = with_tab(s, tab, |t| {
        if let Some(ChatItem::Permission { item, .. }) = t
            .items
            .iter_mut()
            .find(|i| i.id() == item_id && matches!(i, ChatItem::Permission { .. }))
        {
            let rid = item.request_id.clone();
            item.resolved = true;
            item.chosen_option_id = option_id.clone();
            (rid, option_id.clone())
        } else {
            (Value::Null, None)
        }
    })
    .unwrap_or((Value::Null, None));
    backend.respond_permission(tab.to_string(), request_id, chosen);
}

/// Submit (or dismiss) a question card.
pub fn respond_question(s: AppState, backend: &Backend, tab: &str, item_id: u64, accept: bool) {
    let result = with_tab(s, tab, |t| {
        if let Some(ChatItem::Question { item, .. }) = t
            .items
            .iter_mut()
            .find(|i| i.id() == item_id && matches!(i, ChatItem::Question { .. }))
        {
            let request_id = item.request_id.clone();
            if !accept {
                item.resolved = true;
                return Some((request_id, serde_json::json!({ "action": "cancel" })));
            }
            let selections = s.selections.read();
            let sel = selections
                .iter()
                .find(|(id, _)| *id == item_id)
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let mut content = serde_json::Map::new();
            for (qi, q) in item.questions.iter().enumerate() {
                let labels: Vec<&str> = sel
                    .get(qi)
                    .map(|v| {
                        v.iter()
                            .filter_map(|&oi| q.options.get(oi).map(|o| o.label.as_str()))
                            .collect()
                    })
                    .unwrap_or_default();
                if labels.is_empty() {
                    continue;
                }
                if q.multi_select {
                    content.insert(
                        format!("q{}", qi + 1),
                        Value::Array(
                            labels
                                .into_iter()
                                .map(|l| Value::String(l.to_string()))
                                .collect(),
                        ),
                    );
                } else {
                    content.insert(format!("q{}", qi + 1), Value::String(labels[0].to_string()));
                }
            }
            item.resolved = true;
            Some((
                request_id,
                serde_json::json!({ "action": "accept", "content": content }),
            ))
        } else {
            None
        }
    });
    if let Some((request_id, result)) = result.flatten() {
        backend.respond_elicitation(tab.to_string(), request_id, result);
        s.selections
            .write_unchecked()
            .retain(|(id, _)| *id != item_id);
    }
}

/// Toggle a question option; updates the per-card selection list.
pub fn toggle_question_option(
    mut s: AppState,
    item_id: u64,
    question: usize,
    option: usize,
    multi: bool,
) {
    s.selections.with_mut(
        |list| match list.iter().position(|(id, _)| *id == item_id) {
            Some(i) => {
                let v = &mut list[i].1;
                while v.len() <= question {
                    v.push(Vec::new());
                }
                let cur = &mut v[question];
                if multi {
                    match cur.iter().position(|&o| o == option) {
                        Some(pos) => {
                            cur.remove(pos);
                        }
                        None => cur.push(option),
                    }
                } else if cur.first() == Some(&option) {
                    cur.clear();
                } else {
                    *cur = vec![option];
                }
            }
            None => {
                let mut v = vec![Vec::new(); question];
                v.push(vec![option]);
                list.push((item_id, v));
            }
        },
    );
}

pub fn send_message(mut s: AppState, backend: &Backend) {
    let text = s.draft.read().trim().to_string();
    if text.is_empty() || *s.starting.read() {
        return;
    }

    if *s.view.read() == View::Session {
        let Some(tab) = active_tab(s) else { return };
        let Some(session_id) = tab.session_id.clone() else {
            return;
        };
        if tab.pending {
            return;
        }
        s.draft.set(String::new());
        with_tab(s, &tab.id, |t| {
            t.pending = true;
        });
        backend.send_prompt(tab.id, session_id, text);
        scroll_transcript_down();
    } else {
        // New task: reuse the composer target (or home) and start a session
        // whose first prompt is the draft.
        let cwd = s
            .new_task_cwd
            .read()
            .clone()
            .or_else(|| s.tabs.read().last().map(|t| t.cwd.clone()))
            .or_else(|| craft_storage::paths::home().map(|p| p.to_string_lossy().into_owned()))
            .unwrap_or_else(|| ".".to_string());
        let mode = s.mode.read().clone();
        let yolo = *s.yolo.read();
        let auto_review = *s.auto_review.read();
        s.draft.set(String::new());
        s.new_task_cwd.set(None);
        s.starting.set(true);
        backend.start_session(
            new_tab_id(),
            None,
            crate::backend::StartOptions {
                cwd,
                yolo,
                ssh: None,
                mode: (mode != "build").then_some(mode),
                auto_review,
                initial_prompt: Some(text),
            },
        );
    }
}

pub fn cancel_prompt(s: AppState, backend: &Backend) {
    if let Some(tab) = active_tab(s)
        && let Some(session_id) = tab.session_id.clone()
    {
        backend.cancel_prompt(tab.id, session_id);
    }
}

/// Start a session from the onboarding screen.
pub fn start_from_onboarding(
    mut s: AppState,
    backend: &Backend,
    cwd: String,
    yolo: bool,
    ssh: Option<crate::backend::SshTarget>,
    mode: Option<String>,
    auto_review: bool,
) {
    if *s.starting.read() {
        return;
    }
    s.starting.set(true);
    s.start_error.set(None);
    backend.start_session(
        new_tab_id(),
        None,
        crate::backend::StartOptions {
            cwd,
            yolo,
            ssh,
            mode,
            auto_review,
            initial_prompt: None,
        },
    );
}

/// Resume a persisted session from the sidebar history group.
pub fn load_session(mut s: AppState, backend: &Backend, summary: SessionSummary) {
    let cwd = summary
        .cwd
        .clone()
        .or_else(|| s.tabs.read().last().map(|t| t.cwd.clone()))
        .or_else(|| craft_storage::paths::home().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_else(|| ".".to_string());
    let title = summary
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| summary.session_id.chars().take(8).collect());
    let tab_id = new_tab_id();
    s.tabs.write_unchecked().push(Tab {
        id: tab_id.clone(),
        cwd: cwd.clone(),
        title,
        ..Default::default()
    });
    s.active_id.set(tab_id.clone());
    s.view.set(View::Session);
    s.history_open.set(false);
    backend.start_session(
        tab_id,
        Some(summary.session_id),
        crate::backend::StartOptions {
            cwd,
            yolo: false,
            ssh: None,
            mode: None,
            auto_review: false,
            initial_prompt: None,
        },
    );
}

/// Open a project directory as a fresh session tab.
pub fn open_project(mut s: AppState, backend: &Backend, cwd: String) {
    if *s.starting.read() {
        return;
    }
    s.starting.set(true);
    s.start_error.set(None);
    backend.start_session(
        new_tab_id(),
        None,
        crate::backend::StartOptions {
            cwd,
            yolo: false,
            ssh: None,
            mode: None,
            auto_review: false,
            initial_prompt: None,
        },
    );
}

pub fn close_tab(mut s: AppState, backend: &Backend, id: String) {
    backend.close_tab(&id);
    let mut tabs = s.tabs.write_unchecked();
    tabs.retain(|t| t.id != id);
    if *s.active_id.read() == id {
        let next = tabs.first().map(|t| t.id.clone()).unwrap_or_default();
        s.active_id.set(next);
        if tabs.is_empty() {
            s.view.set(View::New);
        }
    }
}

pub fn set_mode(mut s: AppState, backend: &Backend, mode: String) {
    s.mode.set(mode.clone());
    if let Some(tab) = active_tab(s)
        && let Some(session_id) = tab.session_id.clone()
    {
        with_tab(s, &tab.id, |t| t.mode = mode.clone());
        backend.set_mode(tab.id, session_id, mode);
    }
}

/// Sets the permission policy (ask / yolo / auto-review). Updates the
/// onboarding defaults and, when a session is live, its config options.
pub fn set_perm(mut s: AppState, backend: &Backend, yolo: bool, auto_review: bool) {
    s.yolo.set(yolo);
    s.auto_review.set(auto_review);
    if let Some(tab) = active_tab(s) {
        // The composer reads perm state from the live tab's config options;
        // update them optimistically instead of waiting for a server echo.
        with_tab(s, &tab.id, |t| {
            for (id, value) in [("yolo", yolo), ("auto_review", auto_review)] {
                if let Some(c) = t.config_options.iter_mut().find(|c| c.id == id) {
                    c.current_value = value.to_string();
                }
            }
        });
        if let Some(session_id) = tab.session_id.clone() {
            backend.set_config_option(
                tab.id.clone(),
                session_id.clone(),
                "yolo".into(),
                yolo.to_string(),
            );
            backend.set_config_option(
                tab.id,
                session_id,
                "auto_review".into(),
                auto_review.to_string(),
            );
        }
    }
}

pub fn set_config_option(s: AppState, backend: &Backend, config_id: String, value: String) {
    if let Some(tab) = active_tab(s)
        && let Some(session_id) = tab.session_id.clone()
    {
        with_tab(s, &tab.id, |t| {
            if let Some(c) = t.config_options.iter_mut().find(|c| c.id == config_id) {
                c.current_value = value.clone();
            }
        });
        backend.set_config_option(tab.id, session_id, config_id, value);
    }
}

// -------------------------------------------------------------------- skills

/// Open the Skills section and load the skills visible from the current
/// project context (active tab cwd, else last tab cwd, else home).
pub fn open_skills(mut s: AppState) {
    s.view.set(View::Skills);
    s.panel_open.set(false);
    s.palette_open.set(false);
    refresh_skills(s);
}

/// The project context the Skills section lists from, mirroring the
/// "current project" rule used by the sidebar.
pub fn skills_cwd(s: AppState) -> Option<String> {
    active_tab(s)
        .map(|t| t.cwd.clone())
        .or_else(|| s.tabs.read().last().map(|t| t.cwd.clone()))
        .or_else(|| craft_storage::paths::home().map(|p| p.to_string_lossy().into_owned()))
}

/// Reload the skill list from disk. IO problems surface via `skills_error`.
pub fn refresh_skills(mut s: AppState) {
    let Some(cwd) = skills_cwd(s) else {
        s.skills.set(Vec::new());
        s.skills_error
            .set(Some("no project context to list skills from".into()));
        return;
    };
    s.skills
        .set(crate::skills::list(std::path::Path::new(&cwd)));
    s.skills_error.set(None);
}

/// Open the editor for a new skill (defaults to project scope).
pub fn begin_new_skill(mut s: AppState) {
    s.skill_editor.set(Some(crate::skills::SkillDraft::new()));
}

/// Open the editor pre-filled from an existing skill; the name is fixed.
pub fn begin_edit_skill(mut s: AppState, skill: crate::skills::Skill) {
    s.skill_editor
        .set(Some(crate::skills::SkillDraft::from_skill(&skill)));
}

pub fn cancel_skill_editor(mut s: AppState) {
    s.skill_editor.set(None);
}

/// Persist the current editor draft to disk and close it on success.
pub fn save_skill_editor(mut s: AppState) {
    let Some(draft) = s.skill_editor.read().clone() else {
        return;
    };
    if draft.description.trim().is_empty() {
        s.skills_error.set(Some("description is required".into()));
        return;
    }
    let result = match &draft.target {
        Some(path) => crate::skills::update(path, &draft),
        None => {
            let dir = match draft.scope {
                crate::skills::SkillScope::Project => {
                    let Some(cwd) = skills_cwd(s) else {
                        s.skills_error
                            .set(Some("no project context to create a skill in".into()));
                        return;
                    };
                    crate::skills::project_write_dir(std::path::Path::new(&cwd))
                }
                crate::skills::SkillScope::Global => match crate::skills::global_write_dir() {
                    Ok(d) => d,
                    Err(e) => {
                        s.skills_error.set(Some(e));
                        return;
                    }
                },
            };
            crate::skills::create(&dir, &draft).map(|_| ())
        }
    };
    match result {
        Ok(()) => {
            s.skill_editor.set(None);
            refresh_skills(s);
        }
        Err(e) => s.skills_error.set(Some(e)),
    }
}

/// Delete a skill after the UI confirm; back into browsing state either way.
pub fn delete_skill(mut s: AppState, skill: crate::skills::Skill) {
    s.skill_delete.set(None);
    match crate::skills::delete(&skill) {
        Ok(()) => refresh_skills(s),
        Err(e) => s.skills_error.set(Some(e)),
    }
}

/// Spawn a session seeded with instructions to author a SKILL.md from the
/// user's description. The session tab takes over the view when it starts.
pub fn create_skill_with_ai(
    mut s: AppState,
    backend: &Backend,
    description: String,
    scope: crate::skills::SkillScope,
) {
    let description = description.trim().to_string();
    if description.is_empty() || *s.starting.read() {
        return;
    }
    let Some(cwd) = skills_cwd(s) else {
        s.skills_error
            .set(Some("no project context to create a skill in".into()));
        return;
    };
    let target_dir = match scope {
        crate::skills::SkillScope::Project => {
            crate::skills::project_write_dir(std::path::Path::new(&cwd))
        }
        crate::skills::SkillScope::Global => match crate::skills::global_write_dir() {
            Ok(d) => d,
            Err(e) => {
                s.skills_error.set(Some(e));
                return;
            }
        },
    };
    let prompt = format!(
        "Create one craft skill implementing the user's description below.\n\
         Write a single file at `{dir}/<name>/SKILL.md` where `<name>` is a \
         kebab-case slug you choose. The file must start with YAML frontmatter \
         fenced by `---` lines containing `name:`, `description:`, and \
         `when_to_use:` (one line each), followed by a markdown body with \
         step-by-step instructions a future agent could follow. Create the \
         directory as needed. Do not create anything else.\n\n\
         Skill description: {description}",
        dir = target_dir.display(),
    );
    s.skill_ai_open.set(false);
    s.starting.set(true);
    s.start_error.set(None);
    backend.start_session(
        new_tab_id(),
        None,
        crate::backend::StartOptions {
            cwd,
            yolo: false,
            ssh: None,
            mode: None,
            auto_review: false,
            initial_prompt: Some(prompt),
        },
    );
}

/// Keep the transcript pinned to the newest item. Double rAF so the DOM has
/// applied the new content before we measure.
pub fn scroll_transcript_down() {
    document::eval(
        r#"requestAnimationFrame(()=>requestAnimationFrame(()=>{
            const el = document.getElementById('craft-transcript');
            if (el) el.scrollTop = el.scrollHeight;
        }));"#,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tab() -> Tab {
        Tab {
            id: "t".into(),
            session_id: Some("s".into()),
            ..Default::default()
        }
    }

    #[test]
    fn chunks_merge_into_last_item() {
        let mut t = tab();
        apply_session_update(
            &mut t,
            &json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "he" } }),
        );
        apply_session_update(
            &mut t,
            &json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "llo" } }),
        );
        match &t.items[..] {
            [ChatItem::Assistant { text, .. }] => assert_eq!(text, "hello"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_call_update_merges() {
        let mut t = tab();
        apply_session_update(
            &mut t,
            &json!({ "sessionUpdate": "tool_call", "toolCallId": "c1", "title": "read file", "kind": "read" }),
        );
        apply_session_update(
            &mut t,
            &json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "c1",
                "status": "completed",
                "content": [ { "type": "diff", "path": "a.rs", "newText": "x" } ]
            }),
        );
        match &t.items[..] {
            [ChatItem::Tool { call, .. }] => {
                assert_eq!(call.status, ToolStatus::Completed);
                assert_eq!(call.content.len(), 1);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(t.diffs().len(), 1);
    }

    #[test]
    fn usage_and_mode_update() {
        let mut t = tab();
        apply_session_update(
            &mut t,
            &json!({ "sessionUpdate": "usage_update", "used": 10, "size": 100, "cost": { "amount": 0.5 } }),
        );
        apply_session_update(
            &mut t,
            &json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" }),
        );
        assert_eq!((t.context_used, t.context_size), (10, 100));
        assert_eq!(t.session_cost, 0.5);
        assert_eq!(t.mode, "plan");
    }

    #[test]
    fn flatten_todos_nests_by_parent() {
        let items = vec![
            TodoItem {
                id: "1".into(),
                content: "a".into(),
                status: TodoStatus::Pending,
                parent: None,
            },
            TodoItem {
                id: "2".into(),
                content: "b".into(),
                status: TodoStatus::Pending,
                parent: Some("1".into()),
            },
        ];
        let flat = flatten_todos(&items);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].0, 0);
        assert_eq!(flat[1].0, 1);
    }

    #[test]
    fn schema_to_questions_orders_and_splits_descriptions() {
        let schema = json!({
            "q2": { "type": "array", "title": "second", "items": { "anyOf": [ { "const": "a", "title": "a - desc" } ] } },
            "q1": { "type": "string", "title": "first", "oneOf": [ { "const": "x" } ] }
        });
        let qs = schema_to_questions(Some(&schema));
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].question, "first");
        assert!(!qs[0].multi_select);
        assert!(qs[1].multi_select);
        assert_eq!(qs[1].options[0].description.as_deref(), Some("desc"));
    }
}
