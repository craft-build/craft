//! Headless session API: the substrate non-TUI surfaces (ACP, print mode,
//! SDK-style embedding) share instead of each hand-rolling a run loop.
//!
//! Turns run on the shared [`crate::harness::Conversation`] seam;
//! persistence lands on the JSONL
//! [`Session`](crate::storage::sessions::Session) store behind the storage
//! layer's [`SessionStore`](crate::storage::sessions::SessionStore). MCP
//! handles, Flow attachments, and plugin rule stores from the reference
//! have no counterpart here (yet).

use crate::permissions::{PermissionError, ToolKey};
use crate::run::approval::{Approval, ApprovalAsk, ApprovalResponder};

/// The headless approval responder (`--print`, `craft term run`,
/// `craft recipe run`): where an interactive surface would prompt, headless
/// has no one to ask, so every ask fails closed with guidance naming the
/// deliberate opt-outs instead of hanging or allowing. The permission rules
/// and the auto-review reviewer live in the policy layer (07), which hands
/// this responder only the asks that reached it; the `question` tool
/// degrades to `DismissAsk` headlessly, so nothing can park on a user.
pub struct HeadlessGate;

impl ApprovalResponder for HeadlessGate {
    fn ask(&self, ask: ApprovalAsk<'_>) -> crate::run::BoxFuture<Approval> {
        let tool = ToolKey::parse(ask.name);
        let message = headless_deny_message(&tool, ask.scopes);
        Box::pin(async move {
            Approval::Deny {
                message,
                remember: None,
            }
        })
    }
}

/// What an unresolved ask becomes headlessly: a denial whose guidance says
/// why and lists every deliberate opt-out, instead of hanging or allowing.
fn headless_deny_message(tool: &ToolKey, scopes: &[String]) -> String {
    PermissionError::with_guidance(
        &tool.to_string(),
        scopes,
        "this headless run has no interactive approver, so the call was not run; \
         re-run with --yolo to bypass permission checks, with -A/--auto-review \
         to let a reviewer decide, or pre-approve the call in permissions.bml"
            .to_string(),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::SessionRef;
    use crate::storage::StateDir;
    use crate::storage::sessions::generate_title;
    use crate::storage::sessions::{SessionStore, StoredSession};
    use crate::tools::Workspace;

    const CWD: &str = "/project";
    const MODEL_SPEC: &str = "anthropic/claude-test";

    fn state_dir(tmp: &tempfile::TempDir) -> StateDir {
        StateDir::from_path(tmp.path().to_path_buf())
    }

    fn session_ref() -> SessionRef {
        SessionRef::from_id("01965087-4c71-7f00-8000-000000000000".parse().unwrap())
    }

    fn store_in(tmp: &tempfile::TempDir) -> SessionStore {
        SessionStore::open_in(state_dir(tmp), session_ref(), CWD, MODEL_SPEC).unwrap()
    }

    fn load(tmp: &tempfile::TempDir) -> StoredSession {
        StoredSession::load(session_ref().id(), &state_dir(tmp)).unwrap()
    }

    fn mock(turns: Vec<crate::testing::Attempt>) -> crate::provider::ModelHandle {
        let provider = crate::testing::ScriptedProvider::new(Vec::new());
        for turn in turns {
            provider.push(turn);
        }
        crate::provider::ModelHandle::scripted(crate::testing::scripted_model(
            &provider,
            crate::testing::model_info("mock", 200_000, 8_192, false, false),
        ))
    }

    /// One tool-call spec for `crate::testing::tool_turn`.
    fn tool_event(id: &str, name: &str, args: serde_json::Value) -> crate::testing::ToolCallSpec {
        (id.to_owned(), name.to_owned(), args)
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
        let messages = vec![crate::history::ext::stored_user_text("fix the login bug")];
        store.record_turn(&messages, MODEL_SPEC.into());

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(
            loaded.title,
            generate_title(&crate::history::ext::entries(messages.clone()))
        );
    }

    /// F.3 draft preservation: a draft-only change waits for its soft-write
    /// window on disk, `checkpoint_now` flushes it, and an emptied draft
    /// clears the stored copy.
    #[test]
    fn checkpoint_draft_soft_delays_then_flushes_and_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(&tmp);
        // The store just wrote its fresh header, so the first draft lands
        // inside the soft window: recorded, not yet on disk.
        store.checkpoint_draft("half-finished thought");
        assert!(
            load(&tmp).meta.input_draft.is_none(),
            "a draft inside the soft window must not hit disk yet"
        );
        let wait = store
            .soft_save_wait()
            .expect("a deferred draft owes a write window");
        assert!(wait <= crate::storage::sessions::SOFT_SAVE_DELAY);

        // Shutdown flush: the pending draft reaches disk.
        store.checkpoint_now();
        assert_eq!(
            load(&tmp).meta.input_draft.as_deref(),
            Some("half-finished thought")
        );

        // A changed draft within the window again defers; the next turn's
        // record_turn write carries it regardless of the delay.
        store.checkpoint_draft("edited");
        store.record_turn(
            &[crate::history::ext::stored_user_text("sent")],
            MODEL_SPEC.into(),
        );
        let loaded = load(&tmp);
        assert_eq!(loaded.meta.input_draft.as_deref(), Some("edited"));
        assert_eq!(loaded.messages().len(), 1);

        // Submitting empties the draft: the stored copy is cleared.
        store.checkpoint_draft("");
        store.checkpoint_now();
        assert!(load(&tmp).meta.input_draft.is_none());

        // An unchanged draft is a no-op (no revision bump, no write).
        let before = store.session().revision();
        store.checkpoint_draft("");
        assert_eq!(store.session().revision(), before);
        assert!(
            store.soft_save_wait().is_none(),
            "nothing pending, nothing owed"
        );
    }

    #[test]
    fn record_turn_persists_tool_results() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(&tmp);
        store.record_turn(
            &[
                crate::history::ext::stored_user_text("fix the login bug"),
                crate::history::ext::stored(crate::history::ext::observation(vec![
                    crate::history::ext::tool_result("t1", Some("read"), "build failed", false),
                ])),
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
        store.record_turn(
            &[crate::history::ext::stored_user_text("first prompt")],
            MODEL_SPEC.into(),
        );
        drop(store);

        let mut store = store_in(&tmp);
        assert_eq!(store.session().messages().len(), 1);

        store.record_turn(
            &[
                crate::history::ext::stored_user_text("first prompt"),
                crate::history::ext::stored_user_text("second prompt"),
            ],
            "other/model".into(),
        );

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
        assert_eq!(loaded.model, "other/model");
    }

    // --- HeadlessGate --------------------------------------------------------

    use crate::permissions::{Effect, PermissionManager, PermissionRule, PermissionsConfig};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn manager(root: &std::path::Path) -> Arc<PermissionManager> {
        Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            root.to_path_buf(),
        ))
    }

    fn deny(permissions: &PermissionManager, tool: ToolKey) {
        permissions.add_session_rule(PermissionRule {
            tool,
            scope: None,
            effect: Effect::Deny,
        });
    }

    fn allow_reviewer() -> (crate::auto_review::Reviewer, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let reviewer: crate::auto_review::Reviewer = Arc::new(move |_tool, _scopes| {
            seen.fetch_add(1, Ordering::Relaxed);
            let decision = crate::auto_review::Decision {
                verdict: crate::auto_review::Verdict::Allow,
                risk: crate::auto_review::Risk::Low,
                rationale: "local read-only build".into(),
            };
            Box::pin(async move { Ok(decision) })
        });
        (reviewer, calls)
    }

    // --- Gate-wired execution -------------------------------------------------

    /// One gated turn over the real workspace, driven through the
    /// conversation: the policy layer enforces the rules and asks the
    /// fail-closed headless responder, for model calls and batch children
    /// alike.
    async fn gated_turn(
        model: crate::provider::ModelHandle,
        tmp: &tempfile::TempDir,
        permissions: Arc<PermissionManager>,
        reviewer: Option<crate::auto_review::Reviewer>,
    ) -> (Vec<crate::harness::Event>, crate::harness::TurnOutcome) {
        let mut policy = crate::harness::PolicyLayerInputs::new(
            Arc::clone(&permissions),
            Arc::new(HeadlessGate),
        );
        if let Some(reviewer) = reviewer {
            policy = policy.reviewer(reviewer);
        }
        let workspace = Workspace::new(tmp.path()).unwrap();
        let (sink, stream) = crate::harness::EventSink::new();
        let conversation = crate::harness::ConversationBuilder::new(
            &crate::config::Config::default(),
            &tmp.path().display().to_string(),
            "",
            model,
            workspace
                .registry(&animist::history::History::new())
                .unwrap(),
            sink,
        )
        .policy(policy)
        .build()
        .unwrap();
        let outcome = crate::harness::run_turn(
            &conversation,
            "go",
            &[],
            animist::agent::Cancellation::default(),
        )
        .await;
        let mut stream = stream;
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            let done = matches!(
                &event,
                crate::harness::Event::Craft(crate::harness::CraftEvent::Done { .. })
            );
            events.push(event);
            if done {
                break;
            }
        }
        (events, outcome)
    }

    /// The `(name, text)` pairs of every finished tool call.
    fn tool_results(events: &[crate::harness::Event]) -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                crate::harness::Event::Agent(animist::agent::AgentEvent::ToolFinished {
                    call,
                    output,
                }) => Some((
                    call.name.clone(),
                    crate::history::ext::tool_content_text(&output.content),
                )),
                _ => None,
            })
            .collect()
    }

    fn ends_cleanly(events: &[crate::harness::Event]) -> bool {
        matches!(
            events.last(),
            Some(crate::harness::Event::Craft(
                crate::harness::CraftEvent::Done {
                    reason: crate::harness::DoneReason::Stop,
                    ..
                }
            ))
        )
    }

    #[tokio::test]
    async fn denied_calls_never_execute_and_report_back_to_the_model() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        deny(&permissions, ToolKey::native("bash"));
        deny(&permissions, ToolKey::native("write"));
        let marker = tmp.path().join("marker");
        let model = mock(vec![
            crate::testing::tool_turn(vec![tool_event(
                "t1",
                "bash",
                serde_json::json!({"command": format!("touch {}", marker.display())}),
            )]),
            crate::testing::tool_turn(vec![tool_event(
                "t2",
                "write",
                serde_json::json!({"path": "blocked.rs", "content": "never"}),
            )]),
            crate::testing::text_turn("done anyway"),
        ]);
        let (events, outcome) = gated_turn(model, &tmp, permissions, None).await;

        assert!(
            ends_cleanly(&events),
            "a denial is a normal end, not a run failure: {outcome:?}"
        );
        assert!(!marker.exists(), "the denied bash call must not run");
        assert!(
            !tmp.path().join("blocked.rs").exists(),
            "the denied write must not run"
        );
        let results = tool_results(&events);
        assert_eq!(results.len(), 2);
        for (name, text) in &results {
            assert!(
                text.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
                "{name} result must carry the deny: {text}"
            );
        }
    }

    #[tokio::test]
    async fn yolo_actually_runs_what_standard_denies() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        deny(&permissions, ToolKey::native("write"));
        permissions.set_yolo(true);
        let model = mock(vec![
            crate::testing::tool_turn(vec![tool_event(
                "t1",
                "write",
                serde_json::json!({"path": "free.rs", "content": "made it"}),
            )]),
            crate::testing::text_turn("done"),
        ]);
        let (events, _) = gated_turn(model, &tmp, permissions, None).await;
        assert!(
            tmp.path().join("free.rs").exists(),
            "yolo must bypass the deny: {events:?}"
        );
    }

    #[tokio::test]
    async fn auto_review_allows_an_ask_into_execution() {
        // `bash` is the genuine ask (never builtin-allowed); the sandbox
        // wrapper must be off because nested sandbox-exec is denied on
        // hosts that already run the test process sandboxed.
        // SAFETY: single process-wide test knob, same as the bash tool tests.
        unsafe { std::env::set_var("CRAFT_SANDBOX", "off") };
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        permissions.set_auto_review(true);
        let (reviewer, _calls) = allow_reviewer();
        let marker = tmp.path().join("reviewed-marker");
        let model = mock(vec![
            crate::testing::tool_turn(vec![tool_event(
                "t1",
                "bash",
                serde_json::json!({"command": format!("touch {}", marker.display())}),
            )]),
            crate::testing::text_turn("done"),
        ]);
        let (events, _) = gated_turn(model, &tmp, permissions, Some(reviewer)).await;
        assert!(
            marker.exists(),
            "an allowed verdict must let the call execute: {events:?}"
        );
    }

    #[tokio::test]
    async fn batch_children_go_through_the_headless_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        // The parent batch call is allowed so the children are what matter.
        permissions.add_session_rule(PermissionRule {
            tool: ToolKey::native("batch"),
            scope: None,
            effect: Effect::Allow,
        });
        deny(&permissions, ToolKey::native("write"));
        let model = mock(vec![
            crate::testing::tool_turn(vec![tool_event(
                "t1",
                "batch",
                serde_json::json!({"tool_calls": [ {"tool": "write", "parameters": {"path": "child.rs", "content": "no"}} ]}),
            )]),
            crate::testing::text_turn("done"),
        ]);
        let (events, _) = gated_turn(model, &tmp, permissions, None).await;
        assert!(
            !tmp.path().join("child.rs").exists(),
            "the denied batch child must not run"
        );
        let results = tool_results(&events);
        let batch = results
            .iter()
            .find(|(name, _)| name == "batch")
            .expect("the batch call itself completed");
        assert!(
            batch
                .1
                .contains(crate::permissions::PERMISSION_DENIED_PREFIX),
            "the child denial must surface in the batch result: {}",
            batch.1
        );
    }

    #[tokio::test]
    async fn question_calls_complete_as_dismissed_headlessly() {
        let tmp = tempfile::tempdir().unwrap();
        let model = mock(vec![
            crate::testing::tool_turn(vec![tool_event(
                "t1",
                "question",
                serde_json::json!({"questions": [ {"question": "which?", "options": [{"label": "a"}, {"label": "b"}]} ]}),
            )]),
            crate::testing::text_turn("ok"),
        ]);
        let (events, _) = gated_turn(model, &tmp, manager(tmp.path()), None).await;
        assert!(
            ends_cleanly(&events),
            "a dismissed ask ends the run normally, it never hangs"
        );
        let asked = tool_results(&events);
        assert!(
            asked
                .iter()
                .any(|(name, text)| name == "question" && text.contains("dismissed")),
            "{asked:?}"
        );
    }
}
