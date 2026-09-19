//! Headless session API: the substrate non-TUI surfaces (ACP, print mode,
//! SDK-style embedding) share instead of each hand-rolling the run loop.
//!
//! Ported from the reference `craft-agent/src/headless.rs`, re-expressed on
//! this repo's seams: events ride [`crate::run::events`] (tokio mpsc, not
//! flume), turns are driven by [`crate::run::run`] rather than a
//! crate-owned agent loop, and persistence lands on the JSONL
//! [`Session`](crate::storage::sessions::Session) store behind
//! [`SessionStore`]. Params and handles carry only the inputs this repo's
//! run seam actually takes; MCP handles, Flow attachments, and plugin rule
//! stores from the reference have no counterpart here (yet).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::history::{self, Message};
use crate::id::SessionRef;
use crate::providers::DynamicModel;
use crate::run::ToolDispatch;
use crate::run::dispatch::BeforeExecute;
use crate::run::events::{SessionEvents, event_stream};
use crate::run::{Event, RunParams, cancel_channel};
use crate::storage::sessions::StoredTokenUsage;
use crate::storage::stats::{self, CostLedger, CostUsage};
use crate::storage::{StateDir, sessions};
use crate::tools::Workspace;

/// The persisted shape of a live session: our own message/usage/tool-result
/// domain types.
pub type StoredSession = sessions::Session<Message, StoredTokenUsage, history::ToolResult>;

/// Owns a session's persisted state for the lifetime of one headless run.
/// Save failures are warnings, never fatal: a headless run must finish even
/// when the disk is unhappy.
pub struct SessionStore {
    dir: StateDir,
    /// Cost ledger handle, opened once so per-turn appends don't re-resolve
    /// the path. `None` when the state dir cannot host it.
    ledger: Option<CostLedger>,
    session: StoredSession,
}

impl SessionStore {
    /// Open (or create) in the resolved state dir; `None` when the state dir
    /// is unavailable, meaning the session simply will not be persisted.
    pub fn open(session_ref: SessionRef, cwd: &str, model_spec: &str) -> Option<Self> {
        let dir = StateDir::resolve().ok()?;
        Some(Self::open_in(dir, session_ref, cwd, model_spec))
    }

    /// Open (or create) against an explicit dir; a fresh session is saved
    /// immediately so it is loadable before the first turn completes.
    pub fn open_in(dir: StateDir, session_ref: SessionRef, cwd: &str, model_spec: &str) -> Self {
        // Opened once and reused for every turn's cost append; a failure here
        // only means no cost records, the session itself still persists.
        let ledger = CostLedger::from_state_dir(&dir).ok();
        match StoredSession::load(session_ref.id(), &dir) {
            Ok(session) => Self {
                dir,
                ledger,
                session,
            },
            Err(_) => {
                let mut session = StoredSession::new(model_spec, cwd);
                session.id = session_ref;
                let mut store = Self {
                    dir,
                    ledger,
                    session,
                };
                store.save();
                store
            }
        }
    }

    fn save(&mut self) {
        if let Err(e) = self.session.save(&self.dir) {
            eprintln!("failed to persist session: {e}");
        }
    }

    /// Persist the post-turn history and the model that served it, deriving
    /// the title from the first user message when it is still the default.
    pub fn record_turn(&mut self, messages: &[Message], model_spec: String) {
        self.session.replace_messages(messages.to_vec());
        self.session.set_model(model_spec);
        self.session.update_title_if_default();
        self.save();
    }

    /// Fold a finished run's per-model usage into the session and append one
    /// cost-ledger record per model to `cost.jsonl`. Ledger failures are
    /// warnings, never fatal. Unpriced models record `cost_usd: 0.0` — the
    /// ledger's cost field is not optional, so the token counts carry the
    /// information and callers must not show "$0.00" as a bill.
    pub fn record_cost(&mut self, by_model: &HashMap<String, crate::usage::StoredTokenUsage>) {
        if by_model.is_empty() {
            return;
        }
        for (spec, usage) in by_model {
            let stored = sessions::StoredTokenUsage::from(*usage);
            self.session.add_model_usage(spec, stored);
            if let Some(ledger) = &self.ledger {
                let (provider, model) = spec.split_once('/').unwrap_or(("", spec.as_str()));
                let record = stats::make_record(
                    self.session.id.id().to_string(),
                    model,
                    provider,
                    CostUsage::from_stored(usage),
                    usage.cost.unwrap_or(0.0),
                    false,
                );
                if let Err(e) = ledger.append(&record) {
                    eprintln!("warning: failed to append cost record: {e}");
                }
            }
        }
        self.save();
    }
}

/// A ready-to-run one-shot turn: model, run parameters, tools workspace,
/// and the prompt to send.
pub struct HeadlessParams {
    pub model: DynamicModel,
    /// `provider/model` for pricing and the session header; `None` disables
    /// pricing and records the model as unknown.
    pub model_spec: Option<Arc<str>>,
    pub run: RunParams,
    pub workspace: Workspace,
    /// Optional interception point for every tool call (approval gate).
    pub before: Option<Arc<dyn BeforeExecute>>,
    pub prompt: String,
    pub initial_wd: PathBuf,
    /// Where the session is persisted; `None` disables persistence.
    pub state_dir: Option<StateDir>,
    pub session_id: Option<SessionRef>,
}

pub struct HeadlessHandle {
    /// The run's events; `None` (closed) once the task ends.
    pub events: SessionEvents,
    pub tool_names: Vec<String>,
    pub session_id: SessionRef,
    pub cwd: String,
    pub task: JoinHandle<()>,
}

/// Run one prompt to completion in the background. The event stream closes
/// when the task ends; provider/run failures surface as `Event::Error`
/// followed by a terminal `Done` (the run driver emits both itself).
pub fn spawn(params: HeadlessParams) -> HeadlessHandle {
    let cwd = params.initial_wd.to_string_lossy().into_owned();
    let session_ref = params.session_id.unwrap_or_else(SessionRef::generate);
    let model_spec: Option<String> = params
        .model_spec
        .as_ref()
        .map(|s| s.to_string())
        .or_else(|| params.model.label().map(str::to_owned));

    let (guard, events) = event_stream();
    let tool_names = params.workspace.register().names();

    let session_ref_task = session_ref.clone();
    let cwd_task = cwd.clone();
    let task = tokio::spawn(async move {
        let mut tools = params.workspace.register();
        if let Some(hook) = params.before {
            tools = attach_before(tools, hook);
        }
        let mut store = params.state_dir.map(|dir| {
            SessionStore::open_in(
                dir,
                session_ref_task.clone(),
                &cwd_task,
                model_spec.as_deref().unwrap_or("unknown"),
            )
        });
        let mut history = Vec::new();
        let (_flag, cancel) = cancel_channel();
        let mut run_params = params.run;
        run_params.model_spec = params.model_spec;
        let event_tx = guard.sender(0);
        // The terminal Done's per-model ledger, captured from the event
        // seam so the store can persist it after the run ends.
        let done_by_model = Arc::new(std::sync::Mutex::new(None));
        let sink = Arc::clone(&done_by_model);
        crate::run::run(
            &params.model,
            &run_params,
            &tools,
            &mut history,
            &params.prompt,
            &cancel,
            &|event| {
                if let Event::Done { by_model, .. } = &event {
                    *sink.lock().expect("usage sink") = Some(by_model.clone());
                }
                event_tx.send(event);
            },
        )
        .await;
        let by_model = done_by_model
            .lock()
            .expect("usage sink")
            .take()
            .unwrap_or_default();
        if let Some(store) = &mut store {
            store.record_cost(&by_model);
            store.record_turn(&history, model_spec.unwrap_or_else(|| "unknown".into()));
        }
        // `guard` drops here, closing the stream.
    });

    HeadlessHandle {
        events,
        tool_names,
        session_id: session_ref,
        cwd,
        task,
    }
}

/// A channel-driven interactive session: each input runs one multi-turn
/// run against the shared history, which is persisted after every turn.
pub struct InteractiveParams {
    pub model: DynamicModel,
    pub model_spec: Option<Arc<str>>,
    pub run: RunParams,
    pub workspace: Workspace,
    /// Optional interception point for every tool call (approval gate).
    pub before: Option<Arc<dyn BeforeExecute>>,
    pub initial_wd: PathBuf,
    /// Resume an existing session; omitted generates a fresh id.
    pub session_id: Option<SessionRef>,
    /// Seeded history (e.g. replayed from a prior session).
    pub initial_history: Vec<Message>,
    /// Where the session is persisted; `None` disables persistence.
    pub state_dir: Option<StateDir>,
}

pub struct InteractiveHandle {
    /// The session's events for its whole lifetime; `None` (closed) once
    /// the task ends.
    pub events: SessionEvents,
    pub tool_names: Vec<String>,
    /// Next prompt to run.
    pub input_tx: mpsc::UnboundedSender<String>,
    /// Cancel the in-flight turn (or any pending signal is drained before
    /// the next turn starts).
    pub cancel_tx: mpsc::UnboundedSender<()>,
    /// Swap the model before the next turn; the last value wins.
    pub model_tx: mpsc::UnboundedSender<(DynamicModel, Option<Arc<str>>)>,
    pub session_id: SessionRef,
    pub task: JoinHandle<()>,
}

pub fn spawn_interactive(params: InteractiveParams) -> InteractiveHandle {
    let cwd = params.initial_wd.to_string_lossy().into_owned();
    let session_ref = params
        .session_id
        .clone()
        .unwrap_or_else(SessionRef::generate);
    let model_spec: Option<String> = params
        .model_spec
        .as_ref()
        .map(|s| s.to_string())
        .or_else(|| params.model.label().map(str::to_owned));

    let (guard, events) = event_stream();
    let tools = params.workspace.register();
    let tool_names = tools.names();

    let (input_tx, input_rx) = mpsc::unbounded_channel::<String>();
    let (cancel_tx, cancel_rx) = mpsc::unbounded_channel::<()>();
    let (model_tx, model_rx) = mpsc::unbounded_channel::<(DynamicModel, Option<Arc<str>>)>();
    let cancel_rx = Arc::new(tokio::sync::Mutex::new(cancel_rx));

    let session_ref_task = session_ref.clone();
    let cwd_task = cwd.clone();
    let task = tokio::spawn(async move {
        let mut model = params.model;
        let mut model_spec = model_spec;
        let mut run_params = params.run;
        let mut history = params.initial_history;
        let mut store = params.state_dir.map(|dir| {
            SessionStore::open_in(
                dir,
                session_ref_task.clone(),
                &cwd_task,
                model_spec.as_deref().unwrap_or("unknown"),
            )
        });
        let mut input_rx = input_rx;
        let mut model_rx = model_rx;
        let mut run_id: u64 = 0;

        while let Some(prompt) = input_rx.recv().await {
            let event_tx = guard.sender(run_id);

            // Last model swap requested before this turn wins.
            if let Some((new_model, new_spec)) = model_rx.try_recv().ok().or_else(|| {
                let mut last = None;
                while let Ok(next) = model_rx.try_recv() {
                    last = Some(next);
                }
                last
            }) {
                model = new_model;
                model_spec = new_spec
                    .as_ref()
                    .map(|s| s.to_string())
                    .or_else(|| model.label().map(str::to_owned));
            }
            run_params.model_spec = model_spec.clone().map(Arc::from);

            let mut tools = params.workspace.register();
            if let Some(hook) = &params.before {
                tools = attach_before(tools, Arc::clone(hook));
            }

            // A cancel left over from a previous turn must not kill this
            // one, so drain before arming the watcher.
            drain_cancel(&cancel_rx).await;
            let (flag, cancel) = cancel_channel();
            let watcher = {
                let cancel_rx = Arc::clone(&cancel_rx);
                tokio::spawn(async move {
                    let mut guard = cancel_rx.lock().await;
                    if guard.recv().await.is_some() {
                        flag.set(true);
                    }
                })
            };

            // The terminal Done's per-model ledger for this turn.
            let done_by_model = Arc::new(std::sync::Mutex::new(None));
            let sink = Arc::clone(&done_by_model);
            crate::run::run(
                &model,
                &run_params,
                &tools,
                &mut history,
                &prompt,
                &cancel,
                &|event| {
                    if let Event::Done { by_model, .. } = &event {
                        *sink.lock().expect("usage sink") = Some(by_model.clone());
                    }
                    event_tx.send(event);
                },
            )
            .await;
            watcher.abort();

            if let Some(store) = &mut store {
                let by_model = done_by_model
                    .lock()
                    .expect("usage sink")
                    .take()
                    .unwrap_or_default();
                store.record_cost(&by_model);
                store.record_turn(
                    &history,
                    model_spec.clone().unwrap_or_else(|| "unknown".into()),
                );
            }
            run_id += 1;
        }
        // `guard` drops here, closing the stream.
    });

    InteractiveHandle {
        events,
        tool_names,
        input_tx,
        cancel_tx,
        model_tx,
        session_id: session_ref,
        task,
    }
}

fn attach_before(tools: ToolDispatch, hook: Arc<dyn BeforeExecute>) -> ToolDispatch {
    tools.with_before(hook)
}

async fn drain_cancel(cancel_rx: &Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<()>>>) {
    let mut guard = cancel_rx.lock().await;
    while guard.try_recv().is_ok() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::Event;

    use super::StoredSession;
    use crate::storage::sessions::generate_title;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

    const CWD: &str = "/project";
    const MODEL_SPEC: &str = "anthropic/claude-test";

    fn state_dir(tmp: &tempfile::TempDir) -> StateDir {
        StateDir::from_path(tmp.path().to_path_buf())
    }

    fn session_ref() -> SessionRef {
        SessionRef::from_id("01965087-4c71-7f00-8000-000000000000".parse().unwrap())
    }

    fn store_in(tmp: &tempfile::TempDir) -> SessionStore {
        SessionStore::open_in(state_dir(tmp), session_ref(), CWD, MODEL_SPEC)
    }

    fn load(tmp: &tempfile::TempDir) -> StoredSession {
        StoredSession::load(session_ref().id(), &state_dir(tmp)).unwrap()
    }

    fn mock(turns: Vec<Vec<MockStreamEvent>>) -> DynamicModel {
        crate::providers::DynamicModel::wrap(
            Some("mock"),
            MockCompletionModel::from_stream_turns(turns),
        )
    }

    #[test]
    fn new_session_is_loadable_before_first_turn() {
        let tmp = tempfile::tempdir().unwrap();
        store_in(&tmp);
        let loaded = load(&tmp);
        assert_eq!(loaded.id, session_ref());
        assert_eq!(loaded.cwd, CWD);
        assert_eq!(loaded.model, MODEL_SPEC);
        assert!(loaded.messages().is_empty());
    }

    #[test]
    fn record_turn_persists_messages_and_title() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(&tmp);
        let messages = vec![Message::user("fix the login bug")];
        store.record_turn(&messages, MODEL_SPEC.into());

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.title, generate_title(&messages));
    }

    #[test]
    fn record_turn_persists_tool_results() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(&tmp);
        store.record_turn(
            &[
                Message::user("fix the login bug"),
                Message::User {
                    content: vec![crate::history::UserContent::ToolResult(
                        history::ToolResult {
                            call: "t1".into(),
                            name: "read".into(),
                            content: vec![history::ToolResultContent::text("build failed")],
                            is_error: false,
                        },
                    )],
                },
            ],
            MODEL_SPEC.into(),
        );

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
    }

    #[test]
    fn reopening_resumes_existing_session() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(&tmp);
        store.record_turn(&[Message::user("first prompt")], MODEL_SPEC.into());
        drop(store);

        let mut store = store_in(&tmp);
        assert_eq!(store.session.messages().len(), 1);

        store.record_turn(
            &[
                Message::user("first prompt"),
                Message::user("second prompt"),
            ],
            "other/model".into(),
        );

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
        assert_eq!(loaded.model, "other/model");
    }

    fn workspace() -> Workspace {
        let root = std::env::temp_dir().join(format!("craft-headless-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        Workspace::new(&root).unwrap()
    }

    fn params(model: DynamicModel, prompt: &str, state: Option<StateDir>) -> HeadlessParams {
        HeadlessParams {
            model,
            model_spec: Some(Arc::from("mock/model")),
            run: RunParams::default(),
            workspace: workspace(),
            before: None,
            prompt: prompt.into(),
            initial_wd: std::env::temp_dir(),
            state_dir: state,
            session_id: None,
        }
    }

    async fn drain_until_done(events: &mut SessionEvents) -> Vec<Event> {
        let mut out = Vec::new();
        while let Some(envelope) = events.next().await {
            let is_done = matches!(envelope.event, Event::Done { .. });
            out.push(envelope.event);
            if is_done {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn spawn_streams_events_and_persists_the_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let model = mock(vec![vec![
            MockStreamEvent::text("all done"),
            MockStreamEvent::final_response_with_total_tokens(3),
        ]]);
        let mut handle = spawn(params(model, "hello", Some(state_dir(&tmp))));

        let events = drain_until_done(&mut handle.events).await;
        assert!(events.iter().any(|e| matches!(e, Event::Done { .. })));
        let _ = handle.task.await;

        let loaded = StoredSession::load(handle.session_id.id(), &state_dir(&tmp)).unwrap();
        // User prompt + assistant reply.
        assert_eq!(loaded.messages().len(), 2);
        assert!(loaded.messages()[1].text().contains("all done"));
    }

    #[tokio::test]
    async fn spawn_stream_closes_after_the_task_ends() {
        let model = mock(vec![vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let mut handle = spawn(params(model, "hello", None));
        let events = drain_until_done(&mut handle.events).await;
        assert!(events.iter().any(|e| matches!(e, Event::Done { .. })));
        let _ = handle.task.await;
        assert!(handle.events.next().await.is_none());
    }

    #[tokio::test]
    async fn interactive_runs_and_persists_multiple_turns() {
        let tmp = tempfile::tempdir().unwrap();
        let model = mock(vec![
            vec![
                MockStreamEvent::text("one"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("two"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let handle = spawn_interactive(InteractiveParams {
            model,
            model_spec: Some(Arc::from("mock/model")),
            run: RunParams::default(),
            workspace: workspace(),
            before: None,
            initial_wd: std::env::temp_dir(),
            session_id: None,
            initial_history: Vec::new(),
            state_dir: Some(state_dir(&tmp)),
        });
        let mut events = handle.events;

        handle.input_tx.send("first".into()).unwrap();
        let one = drain_until_done(&mut events).await;
        assert!(one.iter().any(|e| matches!(e, Event::Done { .. })));

        handle.input_tx.send("second".into()).unwrap();
        let two = drain_until_done(&mut events).await;
        assert!(two.iter().any(|e| matches!(e, Event::Done { .. })));

        drop(handle.input_tx);
        let _ = handle.task.await;

        let loaded = StoredSession::load(handle.session_id.id(), &state_dir(&tmp)).unwrap();
        // two user prompts + two assistant replies
        assert_eq!(loaded.messages().len(), 4);
    }

    #[tokio::test]
    async fn interactive_applies_model_swap_before_next_turn() {
        let first = mock(vec![vec![
            MockStreamEvent::text("from first"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let second = mock(vec![vec![
            MockStreamEvent::text("from second"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let handle = spawn_interactive(InteractiveParams {
            model: first,
            model_spec: Some(Arc::from("mock/first")),
            run: RunParams::default(),
            workspace: workspace(),
            before: None,
            initial_wd: std::env::temp_dir(),
            session_id: None,
            initial_history: Vec::new(),
            state_dir: None,
        });
        let mut events = handle.events;

        handle.input_tx.send("go".into()).unwrap();
        let one = drain_until_done(&mut events).await;
        assert!(
            one.iter()
                .any(|e| matches!(e, Event::TextDelta(t) if t.contains("from first")))
        );

        handle
            .model_tx
            .send((second, Some(Arc::from("mock/second"))))
            .unwrap();
        handle.input_tx.send("again".into()).unwrap();
        let two = drain_until_done(&mut events).await;
        assert!(
            two.iter()
                .any(|e| matches!(e, Event::TextDelta(t) if t.contains("from second")))
        );

        drop(handle.input_tx);
        let _ = handle.task.await;
    }

    #[tokio::test]
    async fn stale_cancel_is_drained_before_the_next_turn() {
        let model = mock(vec![
            vec![
                MockStreamEvent::text("one"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("two"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let handle = spawn_interactive(InteractiveParams {
            model,
            model_spec: None,
            run: RunParams::default(),
            workspace: workspace(),
            before: None,
            initial_wd: std::env::temp_dir(),
            session_id: None,
            initial_history: Vec::new(),
            state_dir: None,
        });
        let mut events = handle.events;

        handle.input_tx.send("first".into()).unwrap();
        let one = drain_until_done(&mut events).await;
        assert!(matches!(
            one.last(),
            Some(Event::Done {
                reason: crate::run::DoneReason::Stop,
                ..
            })
        ));

        // A cancel with no turn in flight must not poison the next turn.
        handle.cancel_tx.send(()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        handle.input_tx.send("second".into()).unwrap();
        let two = drain_until_done(&mut events).await;
        assert!(matches!(
            two.last(),
            Some(Event::Done {
                reason: crate::run::DoneReason::Stop,
                ..
            })
        ));

        drop(handle.input_tx);
        let _ = handle.task.await;
    }

    /// Blocks the run's only tool call until the test releases it, so the
    /// cancel lands while the turn is genuinely in flight.
    struct GatedHook(Arc<tokio::sync::Notify>);

    impl BeforeExecute for GatedHook {
        fn decide(
            &self,
            _call: history::ToolCall,
        ) -> crate::run::BoxFuture<crate::run::dispatch::Decision> {
            let release = Arc::clone(&self.0);
            Box::pin(async move {
                release.notified().await;
                crate::run::dispatch::Decision::Run
            })
        }
    }

    #[tokio::test]
    async fn interactive_cancel_mid_run_ends_the_turn_as_cancelled() {
        let model = mock(vec![
            vec![
                MockStreamEvent::tool_call("t1", "list", serde_json::json!({"path": "."})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("never reached"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let gate = Arc::new(tokio::sync::Notify::new());
        let handle = spawn_interactive(InteractiveParams {
            model,
            model_spec: None,
            run: RunParams::default(),
            workspace: workspace(),
            before: Some(Arc::new(GatedHook(Arc::clone(&gate)))),
            initial_wd: std::env::temp_dir(),
            session_id: None,
            initial_history: Vec::new(),
            state_dir: None,
        });
        let mut events = handle.events;

        handle.input_tx.send("go".into()).unwrap();
        // Wait until the tool call is actually in flight, then cancel and
        // let the gated call through; the next model call must not run.
        loop {
            let Some(envelope) = events.next().await else {
                panic!("stream closed")
            };
            if matches!(envelope.event, Event::ToolStart { .. }) {
                break;
            }
        }
        handle.cancel_tx.send(()).unwrap();
        gate.notify_one();

        let mut terminal = None;
        while let Some(envelope) = events.next().await {
            if let Event::Done { reason, .. } = envelope.event {
                terminal = Some(reason);
                break;
            }
        }
        assert_eq!(terminal, Some(crate::run::DoneReason::Cancelled));

        drop(handle.input_tx);
        let _ = handle.task.await;
    }
}
