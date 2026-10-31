//! Headless session API: the substrate non-TUI surfaces (ACP, print mode,
//! SDK-style embedding) share instead of each hand-rolling a run loop.
//!
//! Turns run on the shared [`crate::harness::Conversation`] seam;
//! persistence lands on the JSONL
//! [`Session`](crate::storage::sessions::Session) store behind
//! [`SessionStore`]. MCP handles, Flow attachments, and plugin rule stores
//! from the reference have no counterpart here (yet).

use std::sync::Arc;

use crate::history::{self, ContentBlock};
use crate::permissions::{
    PermissionCheck, PermissionError, PermissionManager, ToolKey, scope_for_call,
};
use crate::run::BeforeExecute;

/// The session store every surface shares; moved to the storage layer in 05.
pub use crate::storage::sessions::SessionStore;
/// The persisted shape of a live session: our own message/usage/tool-result
/// domain types.
pub use crate::storage::sessions::StoredSession;

/// The headless permission gate (`--print`, `craft term run`,
/// `craft recipe run`): the permission engine decides every tool call, and
/// where an interactive surface would prompt, headless fails closed — the
/// auto-review reviewer answers when the session started with `-A`,
/// otherwise the call is skipped with guidance naming the deliberate
/// opt-outs. Installing it on the dispatch table covers batch children and
/// subagent dispatch through the shared `before` hook; the `question` tool
/// degrades to `DismissAsk` headlessly, so nothing can park on a user.
pub struct HeadlessGate {
    permissions: Arc<PermissionManager>,
    /// One reviewer call per `NeedsPrompt` decision; `None` when the run
    /// did not start with auto-review on.
    reviewer: Option<crate::auto_review::Reviewer>,
}

impl HeadlessGate {
    pub fn new(
        permissions: Arc<PermissionManager>,
        reviewer: Option<crate::auto_review::Reviewer>,
    ) -> Self {
        Self {
            permissions,
            reviewer,
        }
    }
}

impl BeforeExecute for HeadlessGate {
    fn decide(&self, call: ContentBlock) -> crate::run::BoxFuture<crate::run::Decision> {
        let gate = Self {
            permissions: Arc::clone(&self.permissions),
            reviewer: self.reviewer.clone(),
        };
        Box::pin(async move { gate_decide(gate, call).await })
    }
}

fn denied_message(tool: &ToolKey, scopes: &[String]) -> String {
    PermissionError::new(&tool.to_string(), scopes).to_string()
}

/// What an unresolved ask becomes headlessly: a skip whose guidance says
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

async fn gate_decide(gate: HeadlessGate, call: ContentBlock) -> crate::run::Decision {
    let history::ContentBlock::ToolUse {
        name,
        input: arguments,
        ..
    } = call
    else {
        return crate::run::Decision::Skip("not a tool call".into());
    };
    let HeadlessGate {
        permissions,
        reviewer,
    } = gate;
    let name = name.as_str();
    let tool = ToolKey::parse(name);
    let (scopes, force_prompt) = scope_for_call(permissions.cwd(), name, &arguments);
    match permissions.check_multi(&tool, &scopes, force_prompt) {
        PermissionCheck::Allowed => crate::run::Decision::Run,
        PermissionCheck::Denied => crate::run::Decision::Skip(denied_message(&tool, &scopes)),
        PermissionCheck::NeedsPrompt { .. } => {
            if permissions.is_auto_review()
                && let Some(reviewer) = reviewer.as_ref()
            {
                auto_review_decide(reviewer, &permissions, &tool, &scopes).await
            } else {
                crate::run::Decision::Skip(headless_deny_message(&tool, &scopes))
            }
        }
    }
}

/// Auto-review path for a `NeedsPrompt` decision: one reviewer call answers
/// the prompt, its verdict is recorded as a session rule, and the outcome
/// is reported back to the model. Reviewer failures (timeout, provider
/// error, unparseable output) deny without recording a rule — the reviewer
/// never actually decided. Mirrors the TUI gate minus the overlay events.
async fn auto_review_decide(
    reviewer: &crate::auto_review::Reviewer,
    permissions: &Arc<PermissionManager>,
    tool: &ToolKey,
    scopes: &[String],
) -> crate::run::Decision {
    match reviewer(tool.to_string(), scopes.to_vec()).await {
        Ok(decision) => {
            let allow = decision.verdict == crate::auto_review::Verdict::Allow;
            permissions.apply_auto_review(tool, scopes, allow);
            eprintln!(
                "auto-review {}: {} — {}",
                decision.verdict.as_str(),
                decision.risk.as_str(),
                decision.rationale
            );
            if allow {
                return crate::run::Decision::Run;
            }
            crate::run::Decision::Skip(
                PermissionError::with_guidance(
                    &tool.to_string(),
                    scopes,
                    format!("auto-review: {}", decision.rationale),
                )
                .to_string(),
            )
        }
        Err(err) => {
            eprintln!("auto-review failed closed: {err}");
            crate::run::Decision::Skip(
                PermissionError::with_guidance(
                    &tool.to_string(),
                    scopes,
                    format!("auto-review denied this action: {err}"),
                )
                .to_string(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StoredSession;
    use super::*;
    use crate::id::SessionRef;
    use crate::storage::StateDir;
    use crate::storage::sessions::generate_title;
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

    use crate::permissions::{Effect, PermissionRule, PermissionsConfig, READ_ONLY_TOOLS};
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

    async fn decide(
        gate: &HeadlessGate,
        name: &str,
        args: serde_json::Value,
    ) -> crate::run::Decision {
        let call = history::ContentBlock::ToolUse {
            id: "t1".into(),
            name: name.into(),
            input: args,
            thought_signature: None,
        };
        gate.decide(call).await
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

    #[tokio::test]
    async fn read_only_tools_pass_the_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let gate = HeadlessGate::new(manager(tmp.path()), None);
        for name in READ_ONLY_TOOLS {
            assert!(
                matches!(
                    decide(&gate, name, serde_json::json!({})).await,
                    crate::run::Decision::Run
                ),
                "{name} should run without a decision"
            );
        }
    }

    #[tokio::test]
    async fn deny_rules_block_bash_mutations_and_mcp_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        deny(&permissions, ToolKey::native("bash"));
        deny(&permissions, ToolKey::native("write"));
        deny(&permissions, ToolKey::parse("github__create_issue"));
        let gate = HeadlessGate::new(permissions, None);
        for (name, args) in [
            ("bash", serde_json::json!({"command": "rm -rf /"})),
            (
                "write",
                serde_json::json!({"path": "out.rs", "content": "x"}),
            ),
            ("github__create_issue", serde_json::json!({"title": "x"})),
        ] {
            match decide(&gate, name, args).await {
                crate::run::Decision::Skip(reason) => assert!(
                    reason.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
                    "{name}: {reason}"
                ),
                other => panic!("{name} must be denied, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn unresolved_asks_fail_closed_with_headless_guidance() {
        let tmp = tempfile::tempdir().unwrap();
        let gate = HeadlessGate::new(manager(tmp.path()), None);
        // `bash` defaults to an ask; headless has no approver to answer it.
        match decide(&gate, "bash", serde_json::json!({"command": "cargo build"})).await {
            crate::run::Decision::Skip(reason) => {
                assert!(
                    reason.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
                    "{reason}"
                );
                for opt in ["--yolo", "--auto-review", "permissions.bml"] {
                    assert!(reason.contains(opt), "guidance must name {opt}: {reason}");
                }
            }
            other => panic!("an unresolved ask must skip, not {other:?}"),
        }
    }

    #[tokio::test]
    async fn destructive_mcp_hints_fail_closed_despite_allow_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        permissions.add_session_rule(PermissionRule {
            tool: ToolKey::parse("db__drop_table"),
            scope: None,
            effect: Effect::Allow,
        });
        permissions.register_mcp_annotations("db", "drop_table", Some(false), Some(true));
        let gate = HeadlessGate::new(permissions, None);
        match decide(
            &gate,
            "db__drop_table",
            serde_json::json!({"table": "users"}),
        )
        .await
        {
            crate::run::Decision::Skip(reason) => {
                assert!(
                    reason.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
                    "{reason}"
                );
                assert!(
                    reason.contains("--yolo"),
                    "guidance must name the opt-outs: {reason}"
                );
            }
            other => panic!("a destructive hint must force a decision, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn yolo_runs_even_under_explicit_denies() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        deny(&permissions, ToolKey::native("bash"));
        permissions.set_yolo(true);
        let gate = HeadlessGate::new(permissions, None);
        assert!(matches!(
            decide(&gate, "bash", serde_json::json!({"command": "rm -rf /"})).await,
            crate::run::Decision::Run
        ));
    }

    #[tokio::test]
    async fn auto_review_allow_runs_and_records_a_session_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        permissions.set_auto_review(true);
        let (reviewer, calls) = allow_reviewer();
        let gate = HeadlessGate::new(Arc::clone(&permissions), Some(reviewer));
        let args = serde_json::json!({"command": "cargo build"});
        for _ in 0..2 {
            assert!(matches!(
                decide(&gate, "bash", args.clone()).await,
                crate::run::Decision::Run
            ));
        }
        // The recorded session rule answers the second call; one review.
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn auto_review_deny_skips_with_rationale_and_sticks() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        permissions.set_auto_review(true);
        let reviewer: crate::auto_review::Reviewer = Arc::new(move |_tool, _scopes| {
            let decision = crate::auto_review::Decision {
                verdict: crate::auto_review::Verdict::Deny,
                risk: crate::auto_review::Risk::High,
                rationale: "destructive glob".into(),
            };
            Box::pin(async move { Ok(decision) })
        });
        let gate = HeadlessGate::new(Arc::clone(&permissions), Some(reviewer));
        let args = serde_json::json!({"command": "rm -rf /"});
        match decide(&gate, "bash", args.clone()).await {
            crate::run::Decision::Skip(reason) => {
                assert!(reason.contains("auto-review: destructive glob"), "{reason}");
            }
            other => panic!("a deny verdict must skip, not {other:?}"),
        }
        // The verdict became a session rule: the sibling call is denied
        // outright, never reaching a reviewer.
        let reason = match decide(&gate, "bash", args).await {
            crate::run::Decision::Skip(reason) => reason,
            other => panic!("expected the recorded deny, got {other:?}"),
        };
        assert!(!reason.contains("auto-review"), "{reason}");
    }

    #[tokio::test]
    async fn auto_review_failure_fails_closed_without_recording() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        permissions.set_auto_review(true);
        let reviewer: crate::auto_review::Reviewer = Arc::new(move |_tool, _scopes| {
            let error = crate::auto_review::ReviewError::Timeout {
                deadline: std::time::Duration::from_secs(30),
            };
            Box::pin(async move { Err(error) })
        });
        let gate = HeadlessGate::new(Arc::clone(&permissions), Some(reviewer));
        match decide(&gate, "bash", serde_json::json!({"command": "cargo build"})).await {
            crate::run::Decision::Skip(reason) => {
                assert!(reason.contains("auto-review denied"), "{reason}");
            }
            other => panic!("a failed review must deny, not {other:?}"),
        }
        // Nothing was decided, so nothing was recorded.
        assert!(!matches!(
            permissions.check(&ToolKey::native("bash"), &["cargo build".to_string()]),
            PermissionCheck::Denied
        ));
        // And a session that somehow lost its reviewer still fails closed.
        let gate = HeadlessGate::new(permissions, None);
        assert!(matches!(
            decide(&gate, "bash", serde_json::json!({"command": "cargo build"})).await,
            crate::run::Decision::Skip(_)
        ));
    }

    // --- Gate-wired execution -------------------------------------------------

    /// One gated turn over the real workspace, driven through the
    /// conversation: the gate rides the policy layer for model calls and
    /// the workspace's before hook for batch children.
    async fn gated_turn(
        model: crate::provider::ModelHandle,
        tmp: &tempfile::TempDir,
        permissions: Arc<PermissionManager>,
        reviewer: Option<crate::auto_review::Reviewer>,
    ) -> (Vec<crate::harness::Event>, crate::harness::TurnOutcome) {
        let gate: Arc<dyn BeforeExecute> =
            Arc::new(HeadlessGate::new(Arc::clone(&permissions), reviewer));
        let workspace = Workspace::new(tmp.path()).unwrap();
        let (sink, stream) = crate::harness::EventSink::new();
        let conversation = crate::harness::ConversationBuilder::new(
            &crate::config::Config::default(),
            &tmp.path().display().to_string(),
            "",
            model,
            workspace.registry().unwrap(),
            sink,
        )
        .approval(gate)
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
