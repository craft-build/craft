//! The policy layer (07): permission rules, auto-review, the surface's
//! [`ApprovalResponder`] and the plan-mode write gate, in that order. One
//! policy serves every surface; the responder only conducts the
//! interaction (the TUI overlay, an ACP `session/request_permission`, the
//! headless fail-closed answer).
//!
//! A rules denial or a rejection is reported as a normal (non-error)
//! result, the shape permission answers always had; the plan-mode gate is a
//! hard denial. The guardrail layer sits behind this one and sees only
//! calls the rules (or the interaction) allowed.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use async_trait::async_trait;

use animist::agent::tool::{ToolContext, ToolOutput};
use animist::agent::{Agent, LayerImpl, Result, RunContext, ToolDecision};
use animist::history::StoredMessage;

use crate::auto_review::{Reviewer, Verdict};
use crate::permissions::{
    PermissionAnswer, PermissionCheck, PermissionError, PermissionManager, ToolKey,
    append_permission_rule, scope_for_call,
};
use crate::run::GuardrailState;
use crate::run::approval::{Approval, ApprovalAsk, ApprovalResponder};

use super::conversation::TurnSettings;
use super::events::{CraftEvent, EventSink};

/// The policy layer's wiring: the shared permission rules, the surface's
/// responder, and (when the surface wires one) the auto-review reviewer.
#[derive(Clone)]
pub struct PolicyLayerInputs {
    /// The session's permission manager: rules, defaults, yolo, auto-review
    /// toggle. Every surface already holds one; all conversations in a
    /// session share it.
    pub permissions: Arc<PermissionManager>,
    /// The surface's half of the approval interaction.
    pub responder: Arc<dyn ApprovalResponder>,
    /// The one-shot reviewer model call for auto-review; `None` means the
    /// surface did not wire a model, so asks fall through to the responder.
    pub(crate) reviewer: Option<Reviewer>,
}

impl PolicyLayerInputs {
    pub fn new(permissions: Arc<PermissionManager>, responder: Arc<dyn ApprovalResponder>) -> Self {
        Self {
            permissions,
            responder,
            reviewer: None,
        }
    }

    /// Wire the auto-review reviewer.
    pub(crate) fn reviewer(mut self, reviewer: Reviewer) -> Self {
        self.reviewer = Some(reviewer);
        self
    }
}

pub(crate) struct PolicyLayer {
    inputs: Option<PolicyLayerInputs>,
    settings: Arc<ArcSwap<TurnSettings>>,
    write_root: Option<PathBuf>,
    /// Calls answered without execution, for the guardrail layer's
    /// post-hook to skip.
    guardrails: Arc<GuardrailState>,
    sink: EventSink,
    /// The run's plan-mode gate, snapshotted from the settings at
    /// `turn_start` so a mid-run mode switch cannot take effect inside a
    /// step. Switching modes does not rebuild the agent.
    plan: Mutex<crate::run::AgentMode>,
}

impl PolicyLayer {
    pub(crate) fn new(
        inputs: Option<PolicyLayerInputs>,
        settings: Arc<ArcSwap<TurnSettings>>,
        write_root: Option<PathBuf>,
        guardrails: Arc<GuardrailState>,
        sink: EventSink,
    ) -> Self {
        Self {
            inputs,
            settings,
            write_root,
            guardrails,
            sink,
            plan: Mutex::new(crate::run::AgentMode::default()),
        }
    }

    /// The policy answer as a model-visible non-error result: rejections
    /// have always been normal results the model can adapt to.
    fn respond(message: String) -> ToolDecision {
        ToolDecision::Respond {
            output: ToolOutput::builder().content(message).build(),
        }
    }

    /// Record a remembered answer, persisting "always" answers to
    /// permissions.bml (project-local for `*AlwaysLocal`). Write failures
    /// degrade to the session grant — the answer still applies now, it just
    /// may be asked again later.
    fn record_answer(
        permissions: &PermissionManager,
        tool: &ToolKey,
        scopes: &[String],
        answer: &PermissionAnswer,
    ) {
        for (tool, scope, effect, target) in permissions.apply_decision(tool, scopes, answer) {
            if let Err(err) = append_permission_rule(&tool, scope.as_deref(), effect, &target) {
                eprintln!("permissions: could not persist always-rule: {err}");
            }
        }
    }

    /// Auto-review path for a `NeedsPrompt` decision: one locked-down
    /// reviewer call answers the prompt, its verdict is recorded as session
    /// rules, and the outcome reports back to the model. Reviewer failures
    /// (timeout, provider error, unparseable output) deny without recording
    /// a rule — the reviewer never actually decided. Progress and verdict
    /// ride the conversation event sink so every surface renders the same
    /// review.
    async fn auto_review(
        &self,
        reviewer: &Reviewer,
        permissions: &PermissionManager,
        tool: &ToolKey,
        id: &str,
        scopes: &[String],
    ) -> ToolDecision {
        self.sink.craft(CraftEvent::AutoReviewStart {
            id: id.to_owned(),
            tool: tool.to_string(),
            scopes: scopes.to_vec(),
        });
        let reviewed = reviewer(tool.to_string(), scopes.to_vec()).await;
        let (allow, verdict, risk, rationale) = match &reviewed {
            Ok(decision) => (
                decision.verdict == Verdict::Allow,
                decision.verdict.as_str().to_owned(),
                decision.risk.as_str().to_owned(),
                decision.rationale.clone(),
            ),
            // `verdict = "failed"` marks the fail-closed path: nothing was
            // reviewed, so no rule is recorded and surfaces render the
            // failure line instead of a verdict.
            Err(err) => (false, "failed".to_owned(), String::new(), err.to_string()),
        };
        if reviewed.is_ok() {
            permissions.apply_auto_review(tool, scopes, allow);
        }
        self.sink.craft(CraftEvent::AutoReviewDecision {
            id: id.to_owned(),
            tool: tool.to_string(),
            scopes: scopes.to_vec(),
            verdict,
            risk,
            rationale: rationale.clone(),
        });
        if allow {
            return ToolDecision::Run;
        }
        let guidance = match reviewed {
            Ok(_) => format!("auto-review: {rationale}"),
            Err(_) => format!("auto-review denied this action: {rationale}"),
        };
        Self::respond(
            PermissionError::with_guidance(&tool.to_string(), scopes, guidance).to_string(),
        )
    }

    /// The rules → auto-review/responder pass. Returns the decision to map
    /// onto the run, or `None` when the call is allowed and the plan gate
    /// gets the final word.
    #[expect(
        clippy::result_large_err,
        reason = "AgentError is animist's public error type and cannot be boxed here"
    )]
    async fn authorize(
        &self,
        inputs: &PolicyLayerInputs,
        context: &ToolContext<'_>,
    ) -> Result<Option<ToolDecision>> {
        let call = context.call;
        let name = call.name.as_str();
        let tool = ToolKey::parse(name);
        let (scopes, force_prompt) = scope_for_call(inputs.permissions.cwd(), name, &call.input);
        match inputs.permissions.check_multi(&tool, &scopes, force_prompt) {
            PermissionCheck::Allowed => Ok(None),
            PermissionCheck::Denied => {
                self.guardrails.mark_answered(&call.id);
                Ok(Some(Self::respond(
                    PermissionError::new(&tool.to_string(), &scopes).to_string(),
                )))
            }
            PermissionCheck::NeedsPrompt { low_risk, .. } => {
                let decision = if inputs.permissions.is_auto_review()
                    && let Some(reviewer) = &inputs.reviewer
                {
                    self.auto_review(reviewer, &inputs.permissions, &tool, &call.id, &scopes)
                        .await
                } else {
                    let ask = ApprovalAsk {
                        id: &call.id,
                        name,
                        input: &call.input,
                        scopes: &scopes,
                        low_risk,
                    };
                    match inputs.responder.ask(ask).await {
                        Approval::Stop { reason } => ToolDecision::Stop { reason },
                        Approval::Allow { remember } => {
                            if let Some(answer) = remember {
                                Self::record_answer(&inputs.permissions, &tool, &scopes, &answer);
                            }
                            // Allowed: the plan gate decides next.
                            ToolDecision::Run
                        }
                        Approval::Deny { message, remember } => {
                            if let Some(answer) = remember {
                                Self::record_answer(&inputs.permissions, &tool, &scopes, &answer);
                            }
                            Self::respond(message)
                        }
                    }
                };
                if !matches!(decision, ToolDecision::Run) {
                    self.guardrails.mark_answered(&call.id);
                }
                Ok(if matches!(decision, ToolDecision::Run) {
                    None
                } else {
                    Some(decision)
                })
            }
        }
    }
}

#[async_trait]
impl LayerImpl for PolicyLayer {
    async fn turn_start(
        &self,
        _agent: &Agent,
        _run: &RunContext,
        message: &StoredMessage,
    ) -> Result<StoredMessage> {
        let mode = self.settings.load().mode.clone();
        *self
            .plan
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = mode;
        Ok(message.clone())
    }

    async fn pre_tool_exec(
        &self,
        _agent: &Agent,
        context: &ToolContext<'_>,
    ) -> Result<ToolDecision> {
        let call = context.call;
        // The plan-mode write gate runs whether or not the conversation
        // wires the approval seam: plan mode is conversation state, not an
        // approval-policy choice.
        if let Some(inputs) = &self.inputs
            && let Some(decision) = self.authorize(inputs, context).await?
        {
            return Ok(decision);
        }
        let plan = self
            .plan
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        Ok(
            match plan.plan_block_reason(self.write_root.as_deref(), &call.name, &call.input) {
                Some(reason) => {
                    self.guardrails.mark_answered(&call.id);
                    ToolDecision::Deny { reason }
                }
                None => ToolDecision::Run,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{Conversation, ConversationBuilder, Event, EventSink, EventStream};
    use crate::permissions::{PermissionsConfig, READ_ONLY_TOOLS};
    use crate::run::approval::ApprovalResponder;
    use crate::tools::Workspace;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A responder that allows everything but counts its asks.
    #[derive(Clone, Default)]
    struct AllowAll {
        asks: Arc<AtomicUsize>,
    }

    impl ApprovalResponder for AllowAll {
        fn ask(&self, _ask: ApprovalAsk<'_>) -> crate::run::BoxFuture<Approval> {
            self.asks.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Approval::Allow { remember: None } })
        }
    }

    /// A scripted reviewer returning one verdict, counting its calls.
    fn scripted_reviewer(verdict: Verdict, calls: Arc<AtomicUsize>) -> Reviewer {
        Arc::new(move |_tool, _scopes| {
            calls.fetch_add(1, Ordering::SeqCst);
            let decision = crate::auto_review::Decision {
                verdict,
                risk: crate::auto_review::Risk::Low,
                rationale: "local read-only build".into(),
            };
            Box::pin(async move { Ok(decision) })
        })
    }

    fn manager(root: &Path) -> Arc<PermissionManager> {
        Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            root.to_path_buf(),
        ))
    }

    fn deny_rule(permissions: &PermissionManager, tool: ToolKey) {
        permissions.add_session_rule(crate::permissions::PermissionRule {
            tool,
            scope: None,
            effect: crate::permissions::Effect::Deny,
        });
    }

    fn model(scripted: &crate::testing::ScriptedProvider) -> crate::provider::ModelHandle {
        crate::provider::ModelHandle::scripted(crate::testing::scripted_model(
            scripted,
            crate::testing::model_info("scripted", 200_000, 8_192, false, false),
        ))
    }

    /// A conversation over the real workspace, policy seam wired, running
    /// `turns` against it.
    async fn conversation_over(
        dir: &tempfile::TempDir,
        permissions: Arc<PermissionManager>,
        responder: Arc<dyn ApprovalResponder>,
        reviewer: Option<Reviewer>,
        turns: Vec<crate::testing::Attempt>,
    ) -> (Conversation, EventStream) {
        let scripted = crate::testing::ScriptedProvider::new(Vec::new());
        for turn in turns {
            scripted.push(turn);
        }
        let workspace = Workspace::new(dir.path()).expect("workspace");
        let (sink, stream) = EventSink::new();
        let mut policy = PolicyLayerInputs::new(permissions, responder);
        if let Some(reviewer) = reviewer {
            policy = policy.reviewer(reviewer);
        }
        let conversation = ConversationBuilder::new(
            &crate::config::Config::parse("").unwrap(),
            &dir.path().display().to_string(),
            "",
            model(&scripted),
            workspace.registry().unwrap(),
            sink,
        )
        .policy(policy)
        .workspace(workspace)
        .build()
        .unwrap();
        (conversation, stream)
    }

    /// One gated call then a terminal reply.
    fn call_then_done(name: &str, args: serde_json::Value) -> Vec<crate::testing::Attempt> {
        vec![
            crate::testing::tool_turn(vec![(name.to_owned(), name.to_owned(), args)]),
            crate::testing::text_turn("done"),
        ]
    }

    /// The model-visible result blocks committed by the run.
    async fn result_texts(conversation: &Conversation) -> Vec<(bool, String)> {
        conversation
            .history()
            .snapshot()
            .await
            .entries()
            .iter()
            .filter_map(animist::history::HistoryEntry::current)
            .flat_map(|stored| stored.message().content.iter().cloned())
            .filter_map(|block| match block {
                animist::history::ContentBlock::ToolResult {
                    content, is_error, ..
                } => Some((is_error, crate::history::ext::tool_content_text(&content))),
                _ => None,
            })
            .collect()
    }

    async fn run_one_turn(conversation: &Conversation) -> crate::harness::TurnOutcome {
        crate::harness::run_turn(
            conversation,
            "go",
            &[],
            animist::agent::Cancellation::default(),
        )
        .await
    }

    /// The craft events of one run, up to and including its `Done`.
    async fn events_until_done(stream: &mut EventStream) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(30), stream.next())
                .await
                .expect("event before the deadline")
                .expect("stream open");
            let done = matches!(
                &event,
                Event::Craft(crate::harness::CraftEvent::Done { .. })
            );
            events.push(event);
            if done {
                return events;
            }
        }
    }

    #[test]
    fn plan_gate_blocks_writes_outside_the_plan_file() {
        use crate::run::mode::{AgentMode, PLAN_WRITE_RESTRICTED};

        let mode = AgentMode::Plan(std::path::PathBuf::from("/w/plans/a.md"));
        let blocked = mode.plan_block_reason(
            Some(std::path::Path::new("/w")),
            "write",
            &serde_json::json!({ "path": "src/main.rs", "content": "hi" }),
        );
        assert_eq!(blocked.as_deref(), Some(PLAN_WRITE_RESTRICTED));
        let allowed = mode.plan_block_reason(
            Some(std::path::Path::new("/w")),
            "write",
            &serde_json::json!({ "path": "/w/plans/a.md", "content": "hi" }),
        );
        assert_eq!(allowed, None);
        let reads = mode.plan_block_reason(
            Some(std::path::Path::new("/w")),
            "read",
            &serde_json::json!({ "path": "src/main.rs" }),
        );
        assert_eq!(reads, None);
    }

    /// The scopes of an apply_patch ask cover every file the patch touches
    /// (restored from the pre-07 TUI gate tests, which owned it).
    #[test]
    fn apply_patch_scopes_cover_every_patched_file() {
        let root = std::path::Path::new("/repo");
        let (scopes, force) = crate::permissions::scope_for_call(
            root,
            "apply_patch",
            &serde_json::json!({"patch_text": "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** Delete File: sub/b.rs\n*** End Patch"}),
        );
        assert!(!force);
        assert_eq!(
            scopes,
            vec!["/repo/a.rs".to_string(), "/repo/sub/b.rs".to_string()]
        );
    }

    /// An unregistered tool name still rides the policy chain: the runtime
    /// resolves unknown tools at execution, so the call is an ask (the old
    /// dispatch errored before any gate ever saw it). Pinned as the named
    /// behaviour change of 07.
    #[tokio::test]
    async fn an_unknown_tool_name_still_asks_before_it_errors() {
        let dir = tempfile::tempdir().unwrap();
        let responder = Arc::new(AllowAll::default());
        let (conversation, _stream) = conversation_over(
            &dir,
            manager(dir.path()),
            responder.clone(),
            None,
            call_then_done("some_future_tool", serde_json::json!({})),
        )
        .await;
        let outcome = run_one_turn(&conversation).await;
        assert!(
            matches!(outcome, crate::harness::TurnOutcome::Done { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            responder.asks.load(Ordering::SeqCst),
            1,
            "an unknown tool prompts before the executor rejects it"
        );
        let results = result_texts(&conversation).await;
        assert!(
            results[0].0 && results[0].1.contains("Unknown tool"),
            "the model still sees the runtime's unknown-tool error: {:?}",
            results[0]
        );
    }

    #[tokio::test]
    async fn read_only_tools_run_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let responder = Arc::new(AllowAll::default());
        for name in READ_ONLY_TOOLS {
            let (conversation, _stream) = conversation_over(
                &dir,
                manager(dir.path()),
                responder.clone(),
                None,
                call_then_done(name, serde_json::json!({})),
            )
            .await;
            let outcome = run_one_turn(&conversation).await;
            assert!(
                matches!(outcome, crate::harness::TurnOutcome::Done { .. }),
                "{name}: {outcome:?}"
            );
        }
        assert_eq!(
            responder.asks.load(Ordering::SeqCst),
            0,
            "read-only defaults never ask"
        );
    }

    #[tokio::test]
    async fn deny_rules_block_mutations_and_mcp_keys() {
        let dir = tempfile::tempdir().unwrap();
        let permissions = manager(dir.path());
        deny_rule(&permissions, ToolKey::native("bash"));
        deny_rule(&permissions, ToolKey::native("write"));
        deny_rule(&permissions, ToolKey::parse("github__create_issue"));
        let responder = Arc::new(AllowAll::default());
        for (name, args) in [
            ("bash", serde_json::json!({"command": "rm -rf /"})),
            (
                "write",
                serde_json::json!({"path": "out.rs", "content": "x"}),
            ),
            ("github__create_issue", serde_json::json!({"title": "x"})),
        ] {
            let (conversation, _stream) = conversation_over(
                &dir,
                Arc::clone(&permissions),
                responder.clone(),
                None,
                call_then_done(name, args),
            )
            .await;
            let outcome = run_one_turn(&conversation).await;
            assert!(
                matches!(outcome, crate::harness::TurnOutcome::Done { .. }),
                "{name}: {outcome:?}"
            );
            let results = result_texts(&conversation).await;
            assert_eq!(results.len(), 1, "{name}");
            let (is_error, text) = &results[0];
            assert!(!is_error, "a permission denial is a normal result");
            assert!(
                text.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
                "{name}: {text}"
            );
        }
        assert_eq!(responder.asks.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unresolved_asks_fail_closed_with_headless_guidance() {
        let dir = tempfile::tempdir().unwrap();
        // `bash` defaults to an ask; the headless responder has no one to
        // answer it.
        let (conversation, _stream) = conversation_over(
            &dir,
            manager(dir.path()),
            Arc::new(crate::headless::HeadlessGate),
            None,
            call_then_done("bash", serde_json::json!({"command": "cargo build"})),
        )
        .await;
        let outcome = run_one_turn(&conversation).await;
        assert!(matches!(outcome, crate::harness::TurnOutcome::Done { .. }));
        let results = result_texts(&conversation).await;
        let (is_error, text) = &results[0];
        assert!(!is_error);
        assert!(
            text.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
            "{text}"
        );
        for opt in ["--yolo", "--auto-review", "permissions.bml"] {
            assert!(text.contains(opt), "guidance must name {opt}: {text}");
        }
    }

    #[tokio::test]
    async fn destructive_mcp_hints_fail_closed_despite_allow_rules() {
        let dir = tempfile::tempdir().unwrap();
        let permissions = manager(dir.path());
        permissions.add_session_rule(crate::permissions::PermissionRule {
            tool: ToolKey::parse("db__drop_table"),
            scope: None,
            effect: crate::permissions::Effect::Allow,
        });
        permissions.register_mcp_annotations("db", "drop_table", Some(false), Some(true));
        let (conversation, _stream) = conversation_over(
            &dir,
            permissions,
            Arc::new(crate::headless::HeadlessGate),
            None,
            call_then_done("db__drop_table", serde_json::json!({"table": "users"})),
        )
        .await;
        let outcome = run_one_turn(&conversation).await;
        assert!(matches!(outcome, crate::harness::TurnOutcome::Done { .. }));
        let results = result_texts(&conversation).await;
        let (_, text) = &results[0];
        assert!(
            text.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
            "{text}"
        );
        assert!(
            text.contains("--yolo"),
            "guidance must name the opt-outs: {text}"
        );
    }

    #[tokio::test]
    async fn yolo_runs_even_under_explicit_denies() {
        let dir = tempfile::tempdir().unwrap();
        let permissions = manager(dir.path());
        deny_rule(&permissions, ToolKey::native("write"));
        permissions.set_yolo(true);
        let (conversation, _stream) = conversation_over(
            &dir,
            permissions,
            Arc::new(crate::headless::HeadlessGate),
            None,
            call_then_done(
                "write",
                serde_json::json!({"path": "free.rs", "content": "made it"}),
            ),
        )
        .await;
        let outcome = run_one_turn(&conversation).await;
        assert!(matches!(outcome, crate::harness::TurnOutcome::Done { .. }));
        assert!(
            dir.path().join("free.rs").exists(),
            "yolo must bypass the deny"
        );
    }

    /// Reference parity: an in-project file write is a builtin allow, so it
    /// runs without a prompt.
    #[tokio::test]
    async fn in_project_write_runs_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let responder = Arc::new(AllowAll::default());
        let (conversation, _stream) = conversation_over(
            &dir,
            manager(dir.path()),
            responder.clone(),
            None,
            call_then_done(
                "write",
                serde_json::json!({"path": dir.path().join("src.rs").display().to_string(), "content": "x"}),
            ),
        )
        .await;
        let outcome = run_one_turn(&conversation).await;
        assert!(matches!(outcome, crate::harness::TurnOutcome::Done { .. }));
        assert_eq!(responder.asks.load(Ordering::SeqCst), 0);
    }

    /// A remembered allow covers the sibling call: the answer's generalized
    /// scope lands in the session rules, so the next identical ask never
    /// reaches the responder.
    #[tokio::test]
    async fn an_allow_session_answer_covers_the_sibling_call() {
        let dir = tempfile::tempdir().unwrap();
        let permissions = manager(dir.path());
        /// Allows every ask once, remembering it as a session grant.
        struct AllowSessionOnce(Arc<AtomicUsize>);
        impl ApprovalResponder for AllowSessionOnce {
            fn ask(&self, _ask: ApprovalAsk<'_>) -> crate::run::BoxFuture<Approval> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(std::future::ready(Approval::Allow {
                    remember: Some(crate::permissions::PermissionAnswer::AllowSession),
                }))
            }
        }
        let asks = Arc::new(AtomicUsize::new(0));
        let responder: Arc<dyn ApprovalResponder> = Arc::new(AllowSessionOnce(Arc::clone(&asks)));
        let call = serde_json::json!({"path": "/proj/src/lib.rs", "content": "x"});
        let turns = vec![
            crate::testing::tool_turn(vec![("t1".into(), "write".into(), call.clone())]),
            crate::testing::tool_turn(vec![("t2".into(), "write".into(), call.clone())]),
            crate::testing::text_turn("done"),
        ];
        let (conversation, _stream) =
            conversation_over(&dir, permissions, responder, None, turns).await;
        let outcome = run_one_turn(&conversation).await;
        assert!(matches!(outcome, crate::harness::TurnOutcome::Done { .. }));
        assert_eq!(
            asks.load(Ordering::SeqCst),
            1,
            "the recorded session grant answers the sibling"
        );
    }

    /// Builtin allows (`task`, the auto-allowed argosy tools) never ask.
    #[tokio::test]
    async fn task_and_argosy_tools_run_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let responder = Arc::new(AllowAll::default());
        for (name, args) in [
            (
                "task",
                serde_json::json!({"description": "research", "prompt": "go look"}),
            ),
            ("search", serde_json::json!({"q": "seam"})),
            ("repomap", serde_json::json!({})),
        ] {
            let (conversation, _stream) = conversation_over(
                &dir,
                manager(dir.path()),
                responder.clone(),
                None,
                call_then_done(name, args),
            )
            .await;
            let outcome = run_one_turn(&conversation).await;
            assert!(
                matches!(outcome, crate::harness::TurnOutcome::Done { .. }),
                "{name}: {outcome:?}"
            );
        }
        assert_eq!(responder.asks.load(Ordering::SeqCst), 0);
    }

    /// The repo-mutating argosy code tools are deliberately absent from the
    /// allow list, so they stay on the approval seam.
    #[tokio::test]
    async fn argosy_code_tools_still_ask() {
        let dir = tempfile::tempdir().unwrap();
        let responder = Arc::new(AllowAll::default());
        for name in ["astgrep", "conflicts"] {
            let (conversation, _stream) = conversation_over(
                &dir,
                manager(dir.path()),
                responder.clone(),
                None,
                call_then_done(name, serde_json::json!({})),
            )
            .await;
            let outcome = run_one_turn(&conversation).await;
            assert!(
                matches!(outcome, crate::harness::TurnOutcome::Done { .. }),
                "{name}: {outcome:?}"
            );
        }
        assert_eq!(responder.asks.load(Ordering::SeqCst), 2);
    }

    /// An allow-once answer records nothing: the sibling call asks again.
    #[tokio::test]
    async fn an_allow_once_answer_grants_nothing_persistent() {
        #[derive(Clone)]
        struct AllowOnceAlways;
        impl ApprovalResponder for AllowOnceAlways {
            fn ask(&self, _ask: ApprovalAsk<'_>) -> crate::run::BoxFuture<Approval> {
                Box::pin(std::future::ready(Approval::Allow {
                    remember: Some(crate::permissions::PermissionAnswer::AllowOnce),
                }))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let asks = Arc::new(AtomicUsize::new(0));
        /// Wraps the counting around the allow-once responder.
        struct Counting(Arc<AtomicUsize>);
        impl ApprovalResponder for Counting {
            fn ask(&self, ask: ApprovalAsk<'_>) -> crate::run::BoxFuture<Approval> {
                self.0.fetch_add(1, Ordering::SeqCst);
                AllowOnceAlways.ask(ask)
            }
        }
        let call = serde_json::json!({"path": "/proj/src/lib.rs", "content": "x"});
        let turns = vec![
            crate::testing::tool_turn(vec![("t1".into(), "write".into(), call.clone())]),
            crate::testing::tool_turn(vec![("t2".into(), "write".into(), call.clone())]),
            crate::testing::text_turn("done"),
        ];
        let (conversation, _stream) = conversation_over(
            &dir,
            manager(dir.path()),
            Arc::new(Counting(Arc::clone(&asks))),
            None,
            turns,
        )
        .await;
        let outcome = run_one_turn(&conversation).await;
        assert!(matches!(outcome, crate::harness::TurnOutcome::Done { .. }));
        assert_eq!(
            asks.load(Ordering::SeqCst),
            2,
            "allow-once grants no rule: the sibling asks again"
        );
    }

    /// An MCP wire-name tool prompts under the default rules, and a
    /// session-grant answer keys the rule as `McpTool` so it covers the
    /// tool on the wire.
    #[tokio::test]
    async fn an_mcp_wire_tool_ask_grants_as_an_mcp_key() {
        #[derive(Clone)]
        struct GrantSession;
        impl ApprovalResponder for GrantSession {
            fn ask(&self, _ask: ApprovalAsk<'_>) -> crate::run::BoxFuture<Approval> {
                Box::pin(std::future::ready(Approval::Allow {
                    remember: Some(crate::permissions::PermissionAnswer::AllowSession),
                }))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let permissions = manager(dir.path());
        let turns = vec![
            crate::testing::tool_turn(vec![(
                "t1".into(),
                "github__create_issue".into(),
                serde_json::json!({"title": "x"}),
            )]),
            crate::testing::tool_turn(vec![(
                "t2".into(),
                "github__create_issue".into(),
                serde_json::json!({"title": "x"}),
            )]),
            crate::testing::text_turn("done"),
        ];
        let (conversation, _stream) = conversation_over(
            &dir,
            Arc::clone(&permissions),
            Arc::new(GrantSession),
            None,
            turns,
        )
        .await;
        let outcome = run_one_turn(&conversation).await;
        assert!(matches!(outcome, crate::harness::TurnOutcome::Done { .. }));
        assert!(
            permissions
                .session_rules_snapshot()
                .iter()
                .any(|rule| rule.tool == ToolKey::parse("github__create_issue")
                    && rule.effect == crate::permissions::Effect::Allow),
            "the session grant is keyed as an MCP tool"
        );
    }

    #[tokio::test]
    async fn auto_review_allow_runs_and_records_a_session_rule() {
        let dir = tempfile::tempdir().unwrap();
        let permissions = manager(dir.path());
        permissions.set_auto_review(true);
        let calls = Arc::new(AtomicUsize::new(0));
        let args = serde_json::json!({"command": "cargo build"});
        let turns = vec![
            crate::testing::tool_turn(vec![("t1".into(), "bash".into(), args.clone())]),
            crate::testing::tool_turn(vec![("t2".into(), "bash".into(), args.clone())]),
            crate::testing::text_turn("done"),
        ];
        let (conversation, mut stream) = conversation_over(
            &dir,
            Arc::clone(&permissions),
            Arc::new(crate::headless::HeadlessGate),
            Some(scripted_reviewer(Verdict::Allow, Arc::clone(&calls))),
            turns,
        )
        .await;
        let outcome = run_one_turn(&conversation).await;
        assert!(matches!(outcome, crate::harness::TurnOutcome::Done { .. }));
        // The recorded rule answers the second call without the reviewer.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The review rides the conversation events: start, then verdict.
        let events = events_until_done(&mut stream).await;
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Craft(crate::harness::CraftEvent::AutoReviewStart { id, tool, .. })
                if id == "t1" && tool == "bash")));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Craft(crate::harness::CraftEvent::AutoReviewDecision { id, verdict, .. })
                if id == "t1" && verdict == "allow")));
    }

    #[tokio::test]
    async fn auto_review_deny_reports_rationale_and_sticks() {
        let dir = tempfile::tempdir().unwrap();
        let permissions = manager(dir.path());
        permissions.set_auto_review(true);
        let args = serde_json::json!({"command": "rm -rf /"});
        let turns = vec![
            crate::testing::tool_turn(vec![("t1".into(), "bash".into(), args.clone())]),
            crate::testing::tool_turn(vec![("t2".into(), "bash".into(), args.clone())]),
            crate::testing::text_turn("done"),
        ];
        let (conversation, _stream) = conversation_over(
            &dir,
            Arc::clone(&permissions),
            Arc::new(crate::headless::HeadlessGate),
            Some(scripted_reviewer(
                Verdict::Deny,
                Arc::new(AtomicUsize::new(0)),
            )),
            turns,
        )
        .await;
        let outcome = run_one_turn(&conversation).await;
        assert!(matches!(outcome, crate::harness::TurnOutcome::Done { .. }));
        let results = result_texts(&conversation).await;
        assert!(
            results
                .first()
                .is_some_and(|(_, text)| text.contains("auto-review: local read-only build")),
            "{results:?}"
        );
        // The verdict became a session rule: the sibling is denied outright,
        // never reaching the reviewer's wording again.
        assert!(
            results[1]
                .1
                .starts_with(crate::permissions::PERMISSION_DENIED_PREFIX)
                && !results[1].1.contains("auto-review"),
            "{}",
            results[1].1
        );
    }

    #[tokio::test]
    async fn auto_review_failure_fails_closed_without_recording() {
        let dir = tempfile::tempdir().unwrap();
        let permissions = manager(dir.path());
        permissions.set_auto_review(true);
        let reviewer: Reviewer = Arc::new(move |_tool, _scopes| {
            let error = crate::auto_review::ReviewError::Timeout {
                deadline: std::time::Duration::from_secs(30),
            };
            Box::pin(async move { Err(error) })
        });
        let (conversation, mut stream) = conversation_over(
            &dir,
            Arc::clone(&permissions),
            Arc::new(crate::headless::HeadlessGate),
            Some(reviewer),
            call_then_done("bash", serde_json::json!({"command": "cargo build"})),
        )
        .await;
        let outcome = run_one_turn(&conversation).await;
        assert!(matches!(outcome, crate::harness::TurnOutcome::Done { .. }));
        let results = result_texts(&conversation).await;
        assert!(
            results[0].1.contains("auto-review denied"),
            "{}",
            results[0].1
        );
        // Nothing was decided, so nothing was recorded: a retry still falls
        // through to a prompt (NeedsPrompt), not Denied.
        assert!(!matches!(
            permissions.check(&ToolKey::native("bash"), &["cargo build".to_string()]),
            crate::permissions::PermissionCheck::Denied
        ));
        // The fail-closed verdict rides the events for surfaces to render.
        assert!(
            events_until_done(&mut stream)
                .await
                .iter()
                .any(|event| matches!(event,
                Event::Craft(crate::harness::CraftEvent::AutoReviewDecision { verdict, .. })
                    if verdict == "failed"))
        );
    }

    /// Allowed and Denied both short-circuit before the reviewer.
    #[tokio::test]
    async fn pre_decided_calls_never_reach_the_reviewer() {
        let dir = tempfile::tempdir().unwrap();
        let permissions = manager(dir.path());
        permissions.set_auto_review(true);
        deny_rule(&permissions, ToolKey::native("write"));
        let calls = Arc::new(AtomicUsize::new(0));
        let (conversation, _stream) = conversation_over(
            &dir,
            Arc::clone(&permissions),
            Arc::new(crate::headless::HeadlessGate),
            Some(scripted_reviewer(Verdict::Allow, Arc::clone(&calls))),
            call_then_done(
                "write",
                serde_json::json!({"path": dir.path().join("blocked.rs").display().to_string(), "content": "x"}),
            ),
        )
        .await;
        let outcome = run_one_turn(&conversation).await;
        assert!(matches!(outcome, crate::harness::TurnOutcome::Done { .. }));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a rules decision must never reach the reviewer"
        );
    }
}
