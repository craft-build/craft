//! Command-loop and session tests: queueing behind a running turn,
//! bang-mode result draining/persistence, model selection, and session
//! load/resume.

use std::collections::VecDeque;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::{Mutex, Notify, mpsc};

use super::SessionKit;
use super::bootstrap::{open_session, selection_for_spec};
use super::commands::{
    PendingMessage, close_aborted_turn, drain_shell_results, handle_clear, handle_interrupt,
    handle_reset, handle_select_model, handle_send_message, maybe_send_next,
};
use super::resume::{handle_set_mode, load_session, persist_on_exit, resume_latest, transcript};
use super::turn::{TurnCtx, run_turn};
use super::*;
use crate::history::ext;
use crate::history::marker;
use crate::permissions::PermissionsConfig;
use crate::tui::provider::{LoadedMessage, Tone};

/// A config with one offline-resolvable provider (openai-compatible at a
/// dead port, manual model table): session builds resolve without touching
/// the network.
fn offline_config() -> Config {
    Config::parse(
        r#"
        provider "mock" {
          kind = "openai-compatible"
          base_url = "http://127.0.0.1:9/v1"
          model "model" { name = "Mock" }
        }
    "#,
    )
    .unwrap()
}

fn mock_selection() -> Selection {
    Selection {
        provider: "mock".into(),
        model: "model".into(),
        context_length: None,
    }
}

/// The active texts of a session's conversation history, in order.
async fn history_texts(state: &Arc<Mutex<SessionState>>) -> Vec<String> {
    state
        .lock()
        .await
        .conversation
        .history()
        .snapshot()
        .await
        .entries()
        .iter()
        .filter_map(animist::history::HistoryEntry::current)
        .map(ext::message_text)
        .collect()
}

/// A session state backed by a real persister over `dir`, with the scripted
/// model standing in for the (offline-resolvable) mock provider.
async fn persisted_state(
    dir: &crate::storage::StateDir,
    cwd: &str,
) -> (Arc<Mutex<SessionState>>, crate::id::SessionRef) {
    let session_ref = crate::id::SessionRef::generate();
    let persister =
        crate::harness::Persister::open_in(dir.clone(), session_ref.clone(), cwd, "mock/model")
            .await
            .expect("persister opens");
    let workspace = Workspace::new(std::env::temp_dir()).expect("workspace");
    let (sink, _stream) = crate::harness::EventSink::new();
    let info = crate::testing::model_info_with("main", Some(128_000), None);
    let model = crate::provider::ModelHandle::scripted(crate::testing::scripted_model(
        &crate::testing::ScriptedProvider::new(vec![info.clone()]),
        info,
    ));
    let history = persister.history();
    let conversation = crate::harness::ConversationBuilder::new(
        &Config::parse("").expect("default config"),
        cwd,
        "",
        model,
        workspace.register(),
        sink,
    )
    .history(history)
    .persist(Some(Arc::new(Mutex::new(persister))))
    .build()
    .expect("conversation builds");
    let usage =
        super::usage_recorder::UsageLedger::default().with_session_id(session_ref.id().to_string());
    (
        Arc::new(Mutex::new(SessionState {
            thinking: crate::thinking::ThinkingConfig::default(),
            conversation,
            signals: super::turn::RunSignals::new(),
            usage,
        })),
        session_ref,
    )
}

/// `selection_for_spec` resolves a remembered `provider/model` against
/// the discovered catalog, keeps a slash-bearing model id intact, and
/// rejects a spec whose provider or model has since gone away.
#[test]
fn selection_for_spec_resolves_against_the_catalog() {
    let catalogs: BTreeMap<String, Vec<animist::provider::ModelInfo>> = BTreeMap::from([
        (
            "zai".to_string(),
            vec![crate::testing::model_info_with(
                "glm-5.3",
                Some(200_000),
                None,
            )],
        ),
        (
            "openrouter".to_string(),
            vec![crate::testing::model_info_with(
                "anthropic/claude-sonnet-4",
                None,
                None,
            )],
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

/// Bang-mode visible-run results queue and are committed into the
/// conversation history exactly once, at the next `SendMessage`.
#[tokio::test]
async fn shell_results_drain_into_history_once() {
    let state = Arc::new(Mutex::new(SessionState::for_tests()));
    let pending: Arc<std::sync::Mutex<crate::tui::shell::ShellState>> = Arc::default();
    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_result(ext::stored_user_text("I ran: $ ls\n\nOutput:\nsrc"));

    drain_shell_results(&pending, &state).await;
    assert_eq!(
        history_texts(&state).await,
        vec!["I ran: $ ls\n\nOutput:\nsrc".to_string()]
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
    assert_eq!(history_texts(&state).await.len(), 1);
}

/// Quitting with queued bang-mode results persists them into the
/// session file, instead of losing them until a next `SendMessage`.
#[tokio::test]
async fn bang_results_persist_on_loop_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
    let (state, _session_ref) = persisted_state(&state_dir, "/cwd").await;
    let ctx = test_ctx(state.clone()).await;
    ctx.shell
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_result(ext::stored_user_text("I ran: $ ls\n\nOutput:\nsrc"));

    persist_on_exit(&ctx).await;

    assert_eq!(history_texts(&ctx.state).await.len(), 1);
    let summaries =
        crate::storage::sessions::StoredSession::list(Some("/cwd"), &state_dir).unwrap();
    let found = summaries.first().expect("the session lists for this cwd");
    let reloaded =
        crate::storage::sessions::StoredSession::load(found.id.id(), &state_dir).unwrap();
    assert!(
        reloaded
            .messages()
            .iter()
            .filter_map(animist::history::HistoryEntry::current)
            .any(|m| ext::message_text(m).starts_with("I ran: $ ls")),
        "the bang-mode result reached the session file"
    );
}

/// Submitting while a turn is alive queues the message instead of
/// aborting the turn; the queue preserves submission order.
#[tokio::test]
async fn submit_mid_turn_queues_without_aborting() {
    let state = Arc::new(Mutex::new(SessionState::for_tests()));
    let ctx = test_ctx(state).await;
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
    maybe_send_next(&ctx, &mut current_turn, &mut pending).await;
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
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
    let (state, session_ref) = persisted_state(&state_dir, "/cwd").await;
    let mut ctx = test_ctx(state).await;
    ctx.catalogs = BTreeMap::from([(
        "anthropic".to_string(),
        vec![crate::testing::model_info_with(
            "claude-sonnet-4",
            Some(200_000),
            None,
        )],
    )]);

    handle_select_model(&ctx, "anthropic".into(), "claude-sonnet-4".into()).await;

    let selection = ctx.kit.selection.load_full();
    assert_eq!(selection.provider, "anthropic");
    let reloaded =
        crate::storage::sessions::StoredSession::load(session_ref.id(), &state_dir).unwrap();
    assert_eq!(reloaded.model, "anthropic/claude-sonnet-4");
}

/// A minimal LoopCtx for the queue/persist tests: only `state`, `shell`,
/// and `wake` are exercised; the kit's offline-resolvable provider lets
/// session rebuilds succeed where tests trigger them.
async fn test_ctx(state: Arc<Mutex<SessionState>>) -> LoopCtx {
    LoopCtx {
        state,
        kit: SessionKit {
            config: Arc::new(offline_config()),
            workspace: Workspace::new(std::env::temp_dir()).unwrap(),
            instructions_text: String::new(),
            permissions: Arc::new(PermissionManager::new(
                PermissionsConfig::default(),
                std::path::PathBuf::new(),
            )),
            cancel_cell: Arc::new(ArcSwap::from_pointee(
                animist::agent::Cancellation::default(),
            )),
            subagent_cancels: Arc::new(run::cancel::CancelMap::new()),
            park: Arc::new(Mutex::new(ParkSlots::default())),
            evt_tx: mpsc::unbounded_channel().0,
            files: Files::default(),
            selection: Arc::new(ArcSwap::from_pointee(mock_selection())),
            cwd: "/cwd".into(),
            state_dir: None,
        },
        shell: Arc::default(),
        catalogs: BTreeMap::new(),
        catalog: Arc::new(animist::catalog::Catalog::default()),
        wake: Arc::new(Notify::new()),
    }
}

/// The kit a session (re)build resolves against: offline provider config
/// plus its catalog row, and an optional state dir to persist through.
fn offline_kit(
    config: Config,
    state_dir: Option<crate::storage::StateDir>,
    evt_tx: mpsc::UnboundedSender<AgentEvent>,
) -> SessionKit {
    SessionKit {
        config: Arc::new(config),
        workspace: Workspace::new(std::env::temp_dir()).unwrap(),
        instructions_text: String::new(),
        permissions: Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            std::path::PathBuf::new(),
        )),
        cancel_cell: Arc::new(ArcSwap::from_pointee(
            animist::agent::Cancellation::default(),
        )),
        subagent_cancels: Arc::new(run::cancel::CancelMap::new()),
        park: Arc::new(Mutex::new(ParkSlots::default())),
        evt_tx,
        files: Files::default(),
        selection: Arc::new(ArcSwap::from_pointee(mock_selection())),
        cwd: "/cwd".into(),
        state_dir,
    }
}

/// Thinking is session state: both clear and a brand-new session carry the
/// user's last level forward instead of dropping back to the config default.
#[tokio::test]
async fn thinking_changes_persist_across_clear_and_new_session() {
    use crate::thinking::ThinkingConfig;
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let kit = offline_kit(offline_config(), Some(state_dir.clone()), tx);
    let state = open_session(&kit, ThinkingConfig::default(), None)
        .await
        .expect("session builds offline");
    let ctx = LoopCtx {
        state: state.clone(),
        kit,
        shell: Arc::default(),
        catalogs: BTreeMap::from([("mock".to_owned(), {
            let mut info = crate::testing::model_info_with("model", None, None);
            info.features.thinking.supported = true;
            vec![info]
        })]),
        catalog: Arc::new(animist::catalog::Catalog::default()),
        wake: Arc::new(Notify::new()),
    };

    super::commands::handle_set_thinking(&ctx, ThinkingConfig::Budget(4096)).await;
    assert_eq!(state.lock().await.thinking, ThinkingConfig::Budget(4096));
    async fn stored(state: &Arc<Mutex<SessionState>>) -> Option<Option<ThinkingConfig>> {
        match state.lock().await.conversation.persister() {
            Some(persister) => Some(persister.lock().await.store().thinking()),
            None => None,
        }
    }
    assert_eq!(
        stored(&state).await,
        Some(Some(ThinkingConfig::Budget(4096)))
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(AgentEvent::ThinkingChanged(ThinkingConfig::Budget(4096)))
    ));

    let mut current_turn = None;
    super::commands::handle_clear(&ctx, &mut current_turn).await;
    assert_eq!(state.lock().await.thinking, ThinkingConfig::Budget(4096));
    assert_eq!(
        stored(&state).await,
        Some(Some(ThinkingConfig::Budget(4096)))
    );
    super::commands::handle_reset(&ctx, &mut current_turn).await;
    assert_eq!(state.lock().await.thinking, ThinkingConfig::Budget(4096));
    assert_eq!(
        stored(&state).await,
        Some(Some(ThinkingConfig::Budget(4096)))
    );
}

#[tokio::test]
async fn unsupported_thinking_is_reconciled_in_session_and_badge() {
    use crate::thinking::{Effort, ThinkingConfig};
    let state = Arc::new(Mutex::new(SessionState::for_tests()));
    let ctx = test_ctx(state.clone()).await;
    let mut ctx = ctx;
    // The catalogs row as discovery + the manual-table merge produce it.
    ctx.catalogs = BTreeMap::from([("mock".to_owned(), {
        let mut info = crate::testing::model_info_with("model", None, None);
        info.features.thinking.supported = false;
        vec![info]
    })]);
    let (tx, mut rx) = mpsc::unbounded_channel();
    ctx.kit.evt_tx = tx;
    super::commands::handle_set_thinking(&ctx, ThinkingConfig::Effort(Effort::High)).await;
    assert_eq!(state.lock().await.thinking, ThinkingConfig::Off);
    assert!(matches!(
        rx.try_recv(),
        Ok(AgentEvent::Notice {
            tone: Tone::Warning,
            ..
        })
    ));
    assert!(matches!(
        rx.try_recv(),
        Ok(AgentEvent::ThinkingChanged(ThinkingConfig::Off))
    ));
}

/// W10: a persisted session is listed by `/sessions`, and loading it
/// repopulates the conversation plus the transcript view.
#[tokio::test]
async fn loading_a_persisted_session_repopulates_history() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
    // Persist a session the way a committed turn does.
    let session_ref = crate::id::SessionRef::generate();
    #[allow(unused_mut)]
    let mut persister = crate::harness::Persister::open_in(
        state_dir.clone(),
        session_ref.clone(),
        "/cwd",
        "mock/model",
    )
    .await
    .unwrap();
    {
        let history = persister.history();
        history
            .add_stored(ext::stored_user_text("hello there"))
            .await;
        history
            .add_stored(ext::stored(ext::assistant_text("hi — how can I help?")))
            .await;
        persister
            .store_mut()
            .set_thinking(crate::thinking::ThinkingConfig::Budget(4096));
        persister.store_mut().checkpoint_draft("unsent draft");
        persister.store_mut().set_mode(true);
        persister.store_mut().checkpoint_now();
        persister.checkpoint().await.unwrap();
    }

    // The record exists in the state dir and lists for this cwd.
    let summaries =
        crate::storage::sessions::StoredSession::list(Some("/cwd"), &state_dir).unwrap();
    assert!(summaries.iter().any(|s| s.id == session_ref));
    // Loading it repopulates the conversation and emits the rebuilt transcript.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let kit = offline_kit(offline_config(), Some(state_dir.clone()), tx.clone());
    let state = open_session(&kit, crate::thinking::ThinkingConfig::default(), None)
        .await
        .expect("session builds offline");
    let ctx = LoopCtx {
        state,
        kit,
        shell: Arc::default(),
        catalogs: BTreeMap::new(),
        catalog: Arc::new(animist::catalog::Catalog::default()),
        wake: Arc::new(Notify::new()),
    };
    load_session(&ctx, session_ref.as_str()).await;

    assert_eq!(
        history_texts(&ctx.state).await,
        vec![
            "hello there".to_string(),
            "hi — how can I help?".to_string()
        ]
    );
    assert_eq!(
        ctx.state.lock().await.thinking,
        crate::thinking::ThinkingConfig::Budget(4096)
    );
    let mut loaded = None;
    let mut loaded_draft = String::new();
    let mut loaded_mode = None;
    let mut resumed = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::SessionLoaded {
                messages,
                draft,
                mode,
            } => {
                loaded = Some(messages);
                loaded_draft = draft;
                loaded_mode = mode;
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
    assert_eq!(loaded_mode, Some(crate::tui::provider::SessionMode::Plan));
    assert!(resumed);
}

/// F.3 resume-latest-by-cwd: the newest session for this directory is
/// loaded; nothing persisted keeps the fresh session untouched.
#[tokio::test]
async fn resume_latest_picks_the_newest_session_for_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let kit = offline_kit(offline_config(), Some(state_dir.clone()), tx);
    let state = open_session(&kit, crate::thinking::ThinkingConfig::default(), None)
        .await
        .expect("session builds offline");
    let ctx = LoopCtx {
        state,
        kit,
        shell: Arc::default(),
        catalogs: BTreeMap::new(),
        catalog: Arc::new(animist::catalog::Catalog::default()),
        wake: Arc::new(Notify::new()),
    };
    // The loop's own session lands first so the fixtures below are the
    // newest for this cwd (resume-latest picks by updated_at).
    let persist = |text: &str| {
        let state_dir = state_dir.clone();
        let text = text.to_owned();
        async move {
            let session_ref = crate::id::SessionRef::generate();
            let mut persister =
                crate::harness::Persister::open_in(state_dir, session_ref, "/cwd", "mock/model")
                    .await
                    .unwrap();
            persister
                .history()
                .add_stored(ext::stored_user_text(&text))
                .await;
            persister.checkpoint().await.unwrap();
        }
    };
    persist("older session").await;
    std::thread::sleep(std::time::Duration::from_millis(1100)); // distinct updated_at seconds
    persist("newer session").await;

    resume_latest(&ctx).await;

    assert_eq!(
        history_texts(&ctx.state).await,
        vec!["newer session".to_string()]
    );
    let mut saw_load = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, AgentEvent::SessionLoaded { .. }) {
            saw_load = true;
        }
    }
    assert!(saw_load, "the resumed session announces a SessionLoaded");

    // No prior session in an untouched cwd: a notice, not a crash, and the
    // current conversation stays put. The kit reuses the same state (no new
    // session is built, which would itself become that cwd's latest).
    let (tx, mut rx) = mpsc::unbounded_channel();
    let ctx = LoopCtx {
        state: ctx.state.clone(),
        kit: {
            let mut kit = ctx.kit.clone();
            kit.cwd = "/other-cwd".into();
            kit.evt_tx = tx;
            kit
        },
        shell: Arc::default(),
        catalogs: BTreeMap::new(),
        catalog: Arc::new(animist::catalog::Catalog::default()),
        wake: Arc::new(Notify::new()),
    };
    resume_latest(&ctx).await;
    assert_eq!(
        history_texts(&ctx.state).await,
        vec!["newer session".to_string()],
        "the current conversation stays put"
    );
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
        ext::stored_user_text("do it"),
        ext::stored(ext::observation(vec![ext::tool_result(
            "t1",
            Some("read"),
            "file body",
            false,
        )])),
        ext::stored(ext::system_text("compacted summary")),
    ];
    let rendered = transcript(&messages);
    assert_eq!(rendered.len(), 2);
    assert!(matches!(&rendered[0], LoadedMessage::User(t) if t == "do it"));
    assert!(matches!(&rendered[1], LoadedMessage::Assistant(t) if t == "compacted summary"));
}

/// An unparseable id keeps the current session and warns.
#[tokio::test]
async fn load_with_unknown_id_keeps_the_session_and_warns() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let kit = offline_kit(offline_config(), Some(state_dir), tx);
    let state = open_session(&kit, crate::thinking::ThinkingConfig::default(), None)
        .await
        .expect("session builds offline");
    state
        .lock()
        .await
        .conversation
        .history()
        .add_stored(ext::stored_user_text("keep me"))
        .await;
    let ctx = LoopCtx {
        state: state.clone(),
        kit,
        shell: Arc::default(),
        catalogs: BTreeMap::new(),
        catalog: Arc::new(animist::catalog::Catalog::default()),
        wake: Arc::new(Notify::new()),
    };
    load_session(&ctx, "bogus").await;
    assert_eq!(history_texts(&state).await, vec!["keep me".to_string()]);
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
        let (tx, mut rx) = mpsc::unbounded_channel();
        let kit = offline_kit(offline_config(), None, tx);
        let state = open_session(&kit, crate::thinking::ThinkingConfig::default(), None)
            .await
            .expect("session builds offline");
        let ctx = LoopCtx {
            state,
            kit,
            shell: Arc::default(),
            catalogs: BTreeMap::new(),
            catalog: Arc::new(animist::catalog::Catalog::default()),
            wake: Arc::new(Notify::new()),
        };
        let mut current_turn = None;
        if clear {
            handle_clear(&ctx, &mut current_turn).await;
        } else {
            handle_reset(&ctx, &mut current_turn).await;
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

/// `/clear` and `/new` drop the previous session's todo plan: the fresh
/// session must not inherit the sidebar checklist or the workspace's todo
/// store, or the old plan lingers until the next `todo_write` overwrites it.
#[tokio::test]
async fn clear_and_reset_drop_the_previous_todo_plan() {
    for clear in [true, false] {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let kit = offline_kit(offline_config(), None, tx);
        let state = open_session(&kit, crate::thinking::ThinkingConfig::default(), None)
            .await
            .expect("session builds offline");
        let ctx = LoopCtx {
            state,
            kit,
            shell: Arc::default(),
            catalogs: BTreeMap::new(),
            catalog: Arc::new(animist::catalog::Catalog::default()),
            wake: Arc::new(Notify::new()),
        };
        // Seed the todo store the way a turn's `todo_write` call would.
        use rig_core::tool::PortableTool;
        crate::tools::TodoWrite(ctx.kit.workspace.clone())
            .call(crate::tools::TodoWriteArgs {
                todos: vec![crate::tools::Todo {
                    id: "T1".into(),
                    parent: None,
                    content: "stale plan".into(),
                    status: "pending".into(),
                    owner: None,
                }],
            })
            .await
            .unwrap();
        assert_eq!(ctx.kit.workspace.todos().len(), 1);

        let mut current_turn = None;
        if clear {
            handle_clear(&ctx, &mut current_turn).await;
        } else {
            handle_reset(&ctx, &mut current_turn).await;
        }

        let mut plan_cleared = false;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::PlanSet(plan) = event {
                plan_cleared = plan.is_empty();
            }
        }
        assert!(
            plan_cleared,
            "clear={clear}: the reset must clear the plan panel"
        );
        assert!(
            ctx.kit.workspace.todos().is_empty(),
            "clear={clear}: the reset must clear the todo store"
        );
    }
}

/// `Command::SetMode` checkpoints the session mode into the session file,
/// so a later load (resume) restores it — mirroring the draft checkpoint.
#[tokio::test]
async fn set_mode_checkpoints_the_session_meta() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
    let (state, _session_ref) = persisted_state(&state_dir, "/cwd").await;
    let ctx = test_ctx(state).await;

    handle_set_mode(&ctx, true).await;
    let id = crate::storage::sessions::StoredSession::list(Some("/cwd"), &state_dir)
        .unwrap()
        .pop()
        .expect("set_mode wrote a session record")
        .id
        .id();
    let loaded = crate::storage::sessions::StoredSession::load(id, &state_dir).unwrap();
    assert_eq!(loaded.meta.mode.as_deref(), Some("plan"));

    // Flipping back writes build, not a cleared field.
    handle_set_mode(&ctx, false).await;
    let loaded = crate::storage::sessions::StoredSession::load(id, &state_dir).unwrap();
    assert_eq!(loaded.meta.mode.as_deref(), Some("build"));
}

/// The interrupt abort fallback must not lose the turn: whatever the run
/// committed stays in the conversation, and the aborted turn is closed
/// with the same cancel marker a cooperative cancel would have added —
/// exactly once.
#[tokio::test(start_paused = true)]
async fn interrupt_grace_abort_keeps_the_committed_turn() {
    let state: Arc<Mutex<SessionState>> = Arc::new(Mutex::new(SessionState::for_tests()));
    let ctx = test_ctx(state.clone()).await;
    // A turn stuck on an await that never observes cancellation.
    let stuck = tokio::spawn(std::future::pending::<()>());
    let current_turn = Some(stuck.abort_handle());
    // What the aborted run had already committed.
    state
        .lock()
        .await
        .conversation
        .history()
        .add_stored(ext::stored_user_text("keep me"))
        .await;

    handle_interrupt(&ctx, &current_turn);
    // Paused time jumps to the grace deadline; the fallback aborts the
    // stuck task and closes the conversation.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(stuck.is_finished());
    let texts = history_texts(&state).await;
    assert_eq!(texts.len(), 2, "prompt + cancel marker, nothing dropped");
    assert_eq!(texts[0], "keep me");
    assert_eq!(texts[1], crate::harness::driver::CANCEL_MARKER);

    // Interrupting again on the already-closed turn adds no second marker.
    handle_interrupt(&ctx, &current_turn);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(history_texts(&state).await.len(), 2);
}

/// `close_aborted_turn` is idempotent at the history tail: an interrupted
/// session never accumulates marker spam.
#[tokio::test]
async fn close_aborted_turn_appends_one_marker() {
    let state: Arc<Mutex<SessionState>> = Arc::new(Mutex::new(SessionState::for_tests()));
    state
        .lock()
        .await
        .conversation
        .history()
        .add_stored(ext::stored_user_text("q"))
        .await;
    close_aborted_turn(&state).await;
    close_aborted_turn(&state).await;
    let conversation = state.lock().await.conversation.clone();
    let all = conversation.history().snapshot().await.entries().to_vec();
    assert_eq!(all.len(), 2);
    let last = all
        .last()
        .and_then(animist::history::HistoryEntry::current)
        .unwrap();
    assert!(marker::is(last, marker::CANCEL));
    assert_eq!(
        ext::message_text(last),
        crate::harness::driver::CANCEL_MARKER
    );
}

/// A one-shot local SSE server echoing OpenAI chat.completion chunks (the
/// capture pattern from `providers::tests::server`); returns the base URL
/// and a task yielding the captured request bytes.
fn capture_server(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no request arrived");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            let size = stream.read(&mut buffer).unwrap();
            assert_ne!(size, 0, "incomplete request");
            request.extend_from_slice(&buffer[..size]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .map(|value| value.parse::<usize>().unwrap())
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        );
        stream.write_all(response.as_bytes()).unwrap();
        String::from_utf8(request).unwrap()
    });
    (base, task)
}

/// Regression (docs/hardening/10): the TUI turn must send the preamble
/// exactly as `main` resolved it — one copy of the config baseline, one of
/// the appended marker — in the final model request.
#[tokio::test]
async fn run_turn_sends_the_resolved_preamble_exactly_once() {
    let sse = "data: {\"id\":\"t\",\"object\":\"chat.completion.chunk\",\"created\":0,\
               \"model\":\"manual\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\
               \"content\":\"hello\"},\"finish_reason\":null}]}\n\n\
               data: {\"id\":\"t\",\"object\":\"chat.completion.chunk\",\"created\":0,\
               \"model\":\"manual\",\"choices\":[{\"index\":0,\"delta\":{},\
               \"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\
               \"completion_tokens\":1,\"total_tokens\":2}}\n\n\
               data: [DONE]\n\n";
    let (base, server) = capture_server(sse);
    let mut config = Config::parse(&format!(
        "provider \"test\" {{\n  kind = \"openai-compatible\"\n  base_url = \"{base}/v1\"\n  \
         model \"manual\" {{ name = \"Manual model\" }}\n}}",
    ))
    .unwrap();
    // The resolved preamble, as `main`'s `apply_prompt_overrides` leaves it.
    config.agent.preamble = "BASELINE-PREAMBLE\n\nMARKER".into();

    let dir = tempfile::tempdir().unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut kit = offline_kit(config, None, tx);
    kit.selection = Arc::new(ArcSwap::from_pointee(Selection {
        provider: "test".into(),
        model: "manual".into(),
        context_length: None,
    }));
    kit.cwd = dir.path().display().to_string();
    let state = open_session(&kit, crate::thinking::ThinkingConfig::default(), None)
        .await
        .expect("session builds against the capture server");
    run_turn(
        TurnCtx {
            kit,
            state,
            mode: crate::run::AgentMode::Build,
        },
        "hi".into(),
        Vec::new(),
    )
    .await;

    let captured = server.join().unwrap();
    let (_, body) = captured.split_once("\r\n\r\n").unwrap();
    assert_eq!(body.matches("MARKER").count(), 1, "{body}");
    assert_eq!(body.matches("BASELINE-PREAMBLE").count(), 1, "{body}");
}
