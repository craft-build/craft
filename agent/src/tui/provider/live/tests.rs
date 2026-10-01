//! Command-loop and session tests: queueing behind a running turn,
//! bang-mode result draining/persistence, model selection, and session
//! load/resume.

use std::collections::VecDeque;

use super::bootstrap::selection_for_spec;
use super::commands::{
    PendingMessage, drain_shell_results, handle_clear, handle_reset, handle_select_model,
    handle_send_message, maybe_send_next,
};
use super::resume::{load_session, persist_on_exit, resume_latest, transcript};
use super::*;
use crate::history::Message;
use crate::permissions::PermissionsConfig;
use crate::tui::provider::{LoadedMessage, Tone};

/// `selection_for_spec` resolves a remembered `provider/model` against
/// the discovered catalog, keeps a slash-bearing model id intact, and
/// rejects a spec whose provider or model has since gone away.
#[test]
fn selection_for_spec_resolves_against_the_catalog() {
    let catalogs: BTreeMap<String, Vec<CatalogModel>> = BTreeMap::from([
        (
            "zai".to_string(),
            vec![CatalogModel {
                id: "glm-5.3".into(),
                name: Some("GLM-5.3".into()),
                description: None,
                context_length: Some(200_000),
                max_output_tokens: None,
            }],
        ),
        (
            "openrouter".to_string(),
            vec![CatalogModel {
                id: "anthropic/claude-sonnet-4".into(),
                name: None,
                description: None,
                context_length: None,
                max_output_tokens: None,
            }],
        ),
    ]);

    let picked = selection_for_spec(&catalogs, "zai/glm-5.3").unwrap();
    assert_eq!(picked.provider, "zai");
    assert_eq!(picked.model, "glm-5.3");
    assert_eq!(picked.context_length, Some(200_000));
    // Only the provider alias is split off; a slash-bearing model id
    // (OpenRouter style) stays whole.
    assert_eq!(
        selection_for_spec(&catalogs, "openrouter/anthropic/claude-sonnet-4")
            .unwrap()
            .model,
        "anthropic/claude-sonnet-4"
    );
    assert!(selection_for_spec(&catalogs, "gone/model").is_none());
    assert!(selection_for_spec(&catalogs, "zai/retired-model").is_none());
}

/// Bang-mode visible-run results queue and are drained into the session
/// history exactly once, at the next `SendMessage`.
#[tokio::test]
async fn shell_results_drain_into_history_once() {
    let state = Arc::new(Mutex::new(SessionState::linked()));
    let pending: Arc<std::sync::Mutex<crate::tui::shell::ShellState>> = Arc::default();
    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_result(Message::user("I ran: $ ls\n\nOutput:\nsrc"));

    drain_shell_results(&pending, &state).await;
    assert_eq!(state.lock().await.history.len(), 1);
    assert_eq!(
        state.lock().await.history[0].text(),
        "I ran: $ ls\n\nOutput:\nsrc"
    );
    assert!(
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain_results()
            .is_empty()
    );

    // A second drain with nothing queued is a no-op.
    drain_shell_results(&pending, &state).await;
    assert_eq!(state.lock().await.history.len(), 1);
}

/// Quitting with queued bang-mode results persists them into the
/// session file, instead of losing them until a next `SendMessage`.
#[tokio::test]
async fn bang_results_persist_on_loop_exit() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = crate::storage::StateDir::from_path(dir.path().to_path_buf());
    let state = Arc::new(Mutex::new(SessionState::linked().with_store(
        Some(&state_dir),
        "/cwd",
        "mock/model",
    )));
    let ctx = test_ctx(state.clone());
    ctx.shell
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_result(Message::user("I ran: $ ls\n\nOutput:\nsrc"));

    persist_on_exit(
        &ctx,
        &Selection {
            provider: "mock".into(),
            model: "model".into(),
            context_length: None,
        },
    )
    .await;

    assert_eq!(state.lock().await.history.len(), 1);
    // The store mints its own session id; discover it via the cwd list.
    let summaries = crate::headless::StoredSession::list(Some("/cwd"), &state_dir).unwrap();
    let found = summaries.first().expect("the session lists for this cwd");
    let reloaded = crate::headless::StoredSession::load(found.id.id().clone(), &state_dir).unwrap();
    assert!(
        reloaded
            .messages()
            .iter()
            .any(|m| m.text().starts_with("I ran: $ ls")),
        "the bang-mode result reached the session file"
    );
}

/// Submitting while a turn is alive queues the message instead of
/// aborting the turn; the queue preserves submission order.
#[tokio::test]
async fn submit_mid_turn_queues_without_aborting() {
    let state = Arc::new(Mutex::new(SessionState::linked()));
    let ctx = LoopCtx {
        shell: Arc::default(),
        ..test_ctx(state)
    };
    let selection = Selection {
        provider: String::new(),
        model: String::new(),
        context_length: None,
    };
    // A long-running "turn": alive well past the test body.
    let mut current_turn = Some(
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        })
        .abort_handle(),
    );
    let mut pending: VecDeque<PendingMessage> = VecDeque::new();
    for text in ["first", "second"] {
        handle_send_message(
            &ctx,
            &selection,
            &mut current_turn,
            &mut pending,
            text.to_string(),
            crate::run::AgentMode::Build,
            Vec::new(),
        )
        .await;
    }
    assert_eq!(pending.len(), 2, "both messages queue behind the turn");
    assert_eq!(pending[0].0, "first");
    assert_eq!(pending[1].0, "second");
    assert!(
        !current_turn.as_ref().unwrap().is_finished(),
        "the in-flight turn was not aborted"
    );

    // A settled turn with nothing queued is a no-op.
    let aborted = current_turn.take().unwrap();
    aborted.abort();
    while !aborted.is_finished() {
        tokio::task::yield_now().await;
    }
    current_turn = Some(aborted);
    maybe_send_next(&ctx, &selection, &mut current_turn, &mut pending).await;
    assert_eq!(pending.len(), 1, "the first queued message was sent");
    assert_eq!(pending[0].0, "second");
    assert!(
        current_turn.is_some(),
        "the queued message spawned the next turn"
    );
}

/// Switching the model writes it into the session header right away, so
/// a launch with no follow-up turn still reuses the choice.
#[tokio::test]
async fn selecting_a_model_persists_it_to_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = crate::storage::StateDir::from_path(dir.path().to_path_buf());
    let session_ref = crate::id::SessionRef::generate();
    let store = crate::headless::SessionStore::open_in(
        state_dir.clone(),
        session_ref.clone(),
        "/cwd",
        "zai/glm-5.3",
    )
    .unwrap();
    let mut state = SessionState::linked();
    state.store = Some(store);
    let mut ctx = test_ctx(Arc::new(Mutex::new(state)));
    ctx.catalogs = BTreeMap::from([(
        "anthropic".to_string(),
        vec![CatalogModel {
            id: "claude-sonnet-4".into(),
            name: None,
            description: None,
            context_length: Some(200_000),
            max_output_tokens: None,
        }],
    )]);
    let mut selection = Selection {
        provider: "zai".into(),
        model: "glm-5.3".into(),
        context_length: None,
    };

    handle_select_model(
        &ctx,
        &mut selection,
        "anthropic".into(),
        "claude-sonnet-4".into(),
    )
    .await;

    assert_eq!(selection.provider, "anthropic");
    let loaded = crate::headless::StoredSession::load(session_ref.id(), &state_dir).unwrap();
    assert_eq!(loaded.model, "anthropic/claude-sonnet-4");
}

/// A minimal LoopCtx for the queue/persist tests: only `state`,
/// ``shell`, and `wake` are exercised.
fn test_ctx(state: Arc<Mutex<SessionState>>) -> LoopCtx {
    LoopCtx {
        state,
        files: Files::default(),
        cancel_flag: run::cancel_channel().0,
        subagent_cancels: Arc::new(run::cancel::CancelMap::new()),
        shell: Arc::default(),
        permissions: Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            std::path::PathBuf::new(),
        )),
        config: Arc::new(Config::default()),
        workspace: Workspace::new(std::env::temp_dir()).unwrap(),
        instructions_text: String::new(),
        catalogs: BTreeMap::new(),
        snapshots: crate::snapshot::SnapshotManager::new(std::env::temp_dir()),
        state_dir: None,
        cwd: "/cwd".into(),
        evt_tx: mpsc::unbounded_channel().0,
        wake: Arc::new(Notify::new()),
    }
}

/// W10: a persisted session is listed by `/sessions`, and loading it
/// repopulates the history plus the conversation view.
#[tokio::test]
async fn loading_a_persisted_session_repopulates_history() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = crate::storage::StateDir::from_path(dir.path().to_path_buf());
    // Persist a session the way a committed turn does.
    let session_ref = crate::id::SessionRef::generate();
    let mut store = crate::headless::SessionStore::open_in(
        state_dir.clone(),
        session_ref.clone(),
        "/cwd",
        "mock/model",
    )
    .unwrap();
    let history = vec![
        Message::user("hello there"),
        Message::assistant("hi — how can I help?"),
    ];
    store.checkpoint_draft("unsent draft");
    store.checkpoint_now();
    store.record_turn(&history, "mock/model".into());

    // The record exists in the state dir and lists for this cwd.
    let summaries = crate::headless::StoredSession::list(Some("/cwd"), &state_dir).unwrap();
    assert!(summaries.iter().any(|s| s.id == session_ref));

    // Loading it repopulates the history and emits the rebuilt transcript.
    let state = Arc::new(Mutex::new(SessionState::default()));
    let files = Files::default();
    let (tx, mut rx) = mpsc::unbounded_channel();
    load_session(
        &state,
        &files,
        session_ref.as_str(),
        Some(&state_dir),
        "/cwd",
        "mock/model",
        &tx,
    )
    .await;

    assert_eq!(state.lock().await.history, history);
    // The store rebinds to the loaded id so future turns resume it.
    assert!(state.lock().await.store.is_some());
    let mut loaded = None;
    let mut loaded_draft = String::new();
    let mut resumed = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::SessionLoaded { messages, draft } => {
                loaded = Some(messages);
                loaded_draft = draft;
            }
            AgentEvent::Notice { tone, text } => {
                assert_eq!(tone, Tone::Success);
                resumed = text.starts_with("resumed session");
            }
            _ => {}
        }
    }
    let loaded = loaded.expect("SessionLoaded event");
    assert!(matches!(&loaded[0], LoadedMessage::User(t) if t == "hello there"));
    assert!(matches!(&loaded[1], LoadedMessage::Assistant(t) if t == "hi — how can I help?"));
    assert_eq!(
        loaded_draft, "unsent draft",
        "the preserved draft rides the load"
    );
    assert!(resumed);
}

/// F.3 resume-latest-by-cwd: the newest session for this directory is
/// loaded; nothing persisted keeps the fresh session untouched.
#[tokio::test]
async fn resume_latest_picks_the_newest_session_for_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = crate::storage::StateDir::from_path(dir.path().to_path_buf());
    let persist = |text: &str| {
        let session_ref = crate::id::SessionRef::generate();
        let mut store = crate::headless::SessionStore::open_in(
            state_dir.clone(),
            session_ref,
            "/cwd",
            "mock/model",
        )
        .unwrap();
        store.record_turn(&[Message::user(text)], "mock/model".into());
    };
    persist("older session");
    std::thread::sleep(std::time::Duration::from_millis(1100)); // distinct updated_at seconds
    persist("newer session");

    let state = Arc::new(Mutex::new(SessionState::default()));
    let files = Files::default();
    let (tx, mut rx) = mpsc::unbounded_channel();
    resume_latest(&state, &files, Some(&state_dir), "/cwd", "mock/model", &tx).await;

    assert_eq!(state.lock().await.history.len(), 1);
    assert_eq!(state.lock().await.history[0].text(), "newer session");
    let mut saw_load = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, AgentEvent::SessionLoaded { .. }) {
            saw_load = true;
        }
    }
    assert!(saw_load, "the resumed session announces a SessionLoaded");

    // No prior session in this cwd: a notice, not a crash, and the
    // current session history stays put.
    let state = Arc::new(Mutex::new(SessionState::default()));
    state.lock().await.history = vec![Message::user("keep me")];
    let (tx, mut rx) = mpsc::unbounded_channel();
    resume_latest(
        &state,
        &files,
        Some(&state_dir),
        "/other-cwd",
        "mock/model",
        &tx,
    )
    .await;
    assert_eq!(state.lock().await.history.len(), 1);
    assert!(
        matches!(
            rx.try_recv(),
            Ok(AgentEvent::Notice {
                tone: Tone::Neutral,
                ..
            })
        ),
        "no-session resume is a neutral notice"
    );
}

/// Tool calls, results, and system blocks don't render as user/agent
/// text in the rebuilt transcript; a system block reads as agent text.
#[test]
fn transcript_skips_tool_blocks() {
    let messages = vec![
        Message::user("do it"),
        Message::User {
            content: vec![crate::history::UserContent::ToolResult(
                crate::history::ToolResult {
                    call: "t1".into(),
                    name: "read".into(),
                    content: vec![crate::history::ToolResultContent::text("file body")],
                    is_error: false,
                },
            )],
        },
        Message::system("compacted summary"),
    ];
    let rendered = transcript(&messages);
    assert_eq!(rendered.len(), 2);
    assert!(matches!(&rendered[0], LoadedMessage::User(t) if t == "do it"));
    assert!(matches!(&rendered[1], LoadedMessage::Assistant(t) if t == "compacted summary"));
}

/// An unparseable id keeps the current session and warns.
#[tokio::test]
async fn load_with_unknown_id_keeps_the_session_and_warns() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = crate::storage::StateDir::from_path(dir.path().to_path_buf());
    let state = Arc::new(Mutex::new(SessionState::default()));
    state.lock().await.history = vec![Message::user("keep me")];
    let files = Files::default();
    let (tx, mut rx) = mpsc::unbounded_channel();
    load_session(
        &state,
        &files,
        "bogus",
        Some(&state_dir),
        "/cwd",
        "mock/model",
        &tx,
    )
    .await;
    assert_eq!(state.lock().await.history.len(), 1);
    let event = rx.try_recv().expect("a warning notice");
    assert!(
        matches!(event, AgentEvent::Notice { tone: Tone::Warning, ref text }
            if text.contains("unknown session id")),
        "{event:?}"
    );
}

/// `/clear` and `/new` both zero the composer's context counter: the
/// fresh session carries no context, so the provider must emit a `0.0K`
/// label or the old usage lingers until the next turn reports fresh.
#[tokio::test]
async fn clear_and_reset_zero_the_context_counter() {
    for clear in [true, false] {
        let state = Arc::new(Mutex::new(SessionState::linked()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ctx = LoopCtx {
            evt_tx: tx,
            ..test_ctx(state)
        };
        let selection = Selection {
            provider: "mock".into(),
            model: "model".into(),
            context_length: None,
        };
        let mut current_turn = None;
        if clear {
            handle_clear(&ctx, &selection, &mut current_turn).await;
        } else {
            handle_reset(&ctx, &selection, &mut current_turn).await;
        }

        let mut zeroed = false;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::TokenUsage(label) = event {
                zeroed = label == "0.0K";
            }
        }
        assert!(
            zeroed,
            "clear={clear}: the reset must zero the context counter"
        );
    }
}
