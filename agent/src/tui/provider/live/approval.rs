//! The approval gate: tool calls gated behind the permission engine and,
//! when it asks, the UI's approve/reject seam.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc, oneshot};

use crate::history;
use crate::permissions::{
    ASK_TIMEOUT, FILE_WRITE_TOOLS, PermissionAnswer, PermissionCheck, PermissionError,
    PermissionManager, ToolKey, append_permission_rule,
};
use crate::run::{CancelToken, Decision};

use super::SessionState;
use crate::tui::provider::cards;
use crate::tui::provider::{AgentEvent, Status, Tone, ToolCallData};

/// Gates tool calls behind the permission engine and, when it asks, the
/// UI's approve/reject seam: no workspace mutation runs without an explicit
/// user decision. Approval runs the tool and grants the session; rejection
/// skips it and reports the denial back to the model. With auto-review on,
/// the prompt is answered by a locked-down reviewer model call (E.7)
/// instead of the user; the reviewer fails closed.
pub(super) struct ApprovalGate {
    state: Arc<Mutex<SessionState>>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: CancelToken,
    permissions: Arc<PermissionManager>,
    /// One-shot reviewer model call; `None` in tests that drive the gate
    /// without a provider.
    reviewer: Option<Reviewer>,
}

impl ApprovalGate {
    pub(super) fn new(
        state: Arc<Mutex<SessionState>>,
        tx: mpsc::UnboundedSender<AgentEvent>,
        cancel: CancelToken,
        permissions: Arc<PermissionManager>,
        reviewer: Option<Reviewer>,
    ) -> Self {
        Self {
            state,
            tx,
            cancel,
            permissions,
            reviewer,
        }
    }
}

/// Injectable reviewer so gate tests run without a live provider.
pub(super) type Reviewer = Arc<
    dyn Fn(
            String,
            Vec<String>,
        ) -> crate::run::BoxFuture<
            Result<crate::auto_review::Decision, crate::auto_review::ReviewError>,
        > + Send
        + Sync,
>;

/// Production reviewer: one locked-down model call per NeedsPrompt decision.
pub(super) fn model_reviewer(model: crate::providers::DynamicModel) -> Reviewer {
    Arc::new(move |tool, scopes| {
        let model = model.clone();
        Box::pin(async move { crate::auto_review::review(&model, &tool, &scopes).await })
    })
}

/// The scope a call is about: the touched path for file tools, per-command
/// scopes for bash (task B.2), `*` otherwise (scope-less rules still match
/// it; everything else falls to the default). The flag is the bash parser's
/// `force_prompt`: the scopes could not be derived confidently, so allow
/// rules must not silence the prompt.
fn scope_for_call(root: &Path, name: &str, args: &serde_json::Value) -> (Vec<String>, bool) {
    if name == "bash"
        && let Some(command) = args.get("command").and_then(|v| v.as_str())
        && let Some(scopes) = crate::permissions::bash::permission_scopes(command)
    {
        return (scopes.scopes, scopes.force_prompt);
    }
    if name == "apply_patch"
        && let Some(patch) = args.get("patch_text").and_then(|v| v.as_str())
    {
        return (
            crate::tools::patch_paths(patch)
                .iter()
                .map(|p| resolve_scope_path(root, p))
                .collect(),
            false,
        );
    }
    if name == "move"
        && let (Some(source), Some(destination)) = (
            args.get("source").and_then(|v| v.as_str()),
            args.get("destination").and_then(|v| v.as_str()),
        )
    {
        return (
            [source, destination]
                .iter()
                .map(|p| resolve_scope_path(root, p))
                .collect(),
            false,
        );
    }
    if FILE_WRITE_TOOLS.contains(&name) {
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            return (vec![resolve_scope_path(root, path)], false);
        }
        if let Some(files) = args.get("files").and_then(|v| v.as_array()) {
            return (
                files
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|p| resolve_scope_path(root, p))
                    .collect(),
                false,
            );
        }
    }
    (vec!["*".to_string()], false)
}

fn resolve_scope_path(root: &Path, path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        path.to_string()
    } else {
        root.join(p).display().to_string()
    }
}

fn denied_message(tool: &ToolKey, scopes: &[String]) -> String {
    PermissionError::new(&tool.to_string(), scopes).to_string()
}

/// Record a user answer, persisting "always" answers to permissions.toml
/// (project-local for `*AlwaysLocal`). Write failures degrade to the session
/// grant — the answer still applies now, it just may be asked again later.
fn record_answer(
    permissions: &PermissionManager,
    tool: &ToolKey,
    scopes: &[String],
    answer: &PermissionAnswer,
) -> bool {
    let allow = answer.is_allow();
    for (tool, scope, effect, target) in permissions.apply_decision(tool, scopes, answer) {
        if let Err(err) = append_permission_rule(&tool, scope.as_deref(), effect, &target) {
            eprintln!("permissions: could not persist always-rule: {err}");
        }
    }
    allow
}

/// Auto-review path for a `NeedsPrompt` decision (E.7): one locked-down
/// reviewer call answers the prompt, its verdict is recorded as a session
/// rule, and the outcome is reported back to the model. Reviewer failures
/// (timeout, provider error, unparseable output) deny without recording a
/// rule — the reviewer never actually decided.
async fn auto_review_decide(
    tx: mpsc::UnboundedSender<AgentEvent>,
    reviewer: &Reviewer,
    permissions: &Arc<PermissionManager>,
    tool: &ToolKey,
    scopes: &[String],
    id: String,
) -> Decision {
    // In-progress marker carrying the call's id: it renders as a line under
    // the tool card (not inside it), and the verdict below replaces it in
    // place, so the card stays focused on the tool's own output.
    let _ = tx.send(AgentEvent::AutoReview {
        id: id.clone(),
        tone: Tone::Neutral,
        text: "auto-review: reviewing…".into(),
    });
    let review = reviewer(tool.to_string(), scopes.to_vec());
    let outcome = match review.await {
        Ok(decision) => {
            let allow = decision.verdict == crate::auto_review::Verdict::Allow;
            permissions.apply_auto_review(tool, scopes, allow);
            let _ = tx.send(AgentEvent::AutoReview {
                id,
                tone: if allow { Tone::Success } else { Tone::Warning },
                text: format!(
                    "auto-review {}: {} — {}",
                    decision.verdict.as_str(),
                    decision.risk.as_str(),
                    decision.rationale
                ),
            });
            if allow {
                return Decision::Run;
            }
            PermissionError::with_guidance(
                &tool.to_string(),
                scopes,
                format!("auto-review: {}", decision.rationale),
            )
            .to_string()
        }
        Err(err) => {
            let _ = tx.send(AgentEvent::AutoReview {
                id,
                tone: Tone::Danger,
                text: format!("auto-review failed closed: {err}"),
            });
            PermissionError::with_guidance(
                &tool.to_string(),
                scopes,
                format!("auto-review denied this action: {err}"),
            )
            .to_string()
        }
    };
    Decision::Skip(outcome)
}

impl crate::run::BeforeExecute for ApprovalGate {
    fn decide(&self, call: history::ToolCall) -> crate::run::BoxFuture<Decision> {
        let ApprovalGate {
            state,
            tx,
            cancel,
            permissions,
            reviewer,
        } = self.clone_for_call();
        Box::pin(async move { gate_decide(state, tx, cancel, permissions, reviewer, call).await })
    }
}

impl ApprovalGate {
    fn clone_for_call(&self) -> Self {
        Self {
            state: self.state.clone(),
            tx: self.tx.clone(),
            cancel: self.cancel.clone(),
            permissions: self.permissions.clone(),
            reviewer: self.reviewer.clone(),
        }
    }
}

async fn gate_decide(
    state: Arc<Mutex<SessionState>>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: CancelToken,
    permissions: Arc<PermissionManager>,
    reviewer: Option<Reviewer>,
    call: history::ToolCall,
) -> Decision {
    if cancel.cancelled() {
        return Decision::Stop("cancelled by client".into());
    }
    let name = call.function.name.as_str();
    let tool = ToolKey::native(name);
    let (scopes, force_prompt) = scope_for_call(permissions.cwd(), name, &call.function.arguments);
    match permissions.check_multi(&tool, &scopes, force_prompt) {
        PermissionCheck::Allowed => return Decision::Run,
        PermissionCheck::Denied => {
            return Decision::Skip(denied_message(&tool, &scopes));
        }
        PermissionCheck::NeedsPrompt { .. } => {}
    }
    let id = call.id.clone();
    if permissions.is_auto_review()
        && let Some(reviewer) = reviewer.as_ref()
    {
        return auto_review_decide(tx.clone(), reviewer, &permissions, &tool, &scopes, id).await;
    }
    let _ = tx.send(AgentEvent::AssistantEnd);
    let _ = tx.send(AgentEvent::ToolCall(ToolCallData {
        id: id.clone(),
        kind: cards::tool_head(&call.function.name, &call.function.arguments),
        lines: Vec::new(),
        awaiting_approval: true,
    }));
    let _ = tx.send(AgentEvent::StatusChanged(Status::WaitingApproval));

    let (decision_tx, mut decision_rx) = oneshot::channel();
    state.lock().await.pending_approval = Some((id, decision_tx));
    let mut cancel_rx = cancel.subscribe();
    // Cancellation is epoch-based: `changed()` fires only on a
    // `set(true)` generation bump or a dropped flag — a re-arm
    // never writes, so a ready change is always a real cancel.
    let answer = tokio::select! {
        biased;
        changed = cancel_rx.changed() => {
            let _ = changed;
            state.lock().await.pending_approval = None;
            return Decision::Stop("cancelled by client".into());
        }
        decision = tokio::time::timeout(ASK_TIMEOUT, &mut decision_rx) => {
            state.lock().await.pending_approval = None;
            decision
                .unwrap_or(Ok(PermissionAnswer::Deny))
                .unwrap_or(PermissionAnswer::Deny)
        }
    };
    let _ = tx.send(AgentEvent::StatusChanged(Status::Running));
    if record_answer(&permissions, &tool, &scopes, &answer) {
        Decision::Run
    } else {
        Decision::Skip(denied_message(&tool, &scopes))
    }
}

/// Deliver the user's decision to a tool call waiting on it, if the id
/// matches the currently pending one.
pub(super) async fn decide(state: &Arc<Mutex<SessionState>>, id: String, answer: PermissionAnswer) {
    let mut session = state.lock().await;
    if matches!(&session.pending_approval, Some((pid, _)) if *pid == id)
        && let Some((_, decision)) = session.pending_approval.take()
    {
        let _ = decision.send(answer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermissionsConfig;
    use crate::run::BeforeExecute;

    fn tool_call(id: &str, name: &str) -> history::ToolCall {
        history::ToolCall {
            id: id.into(),
            function: history::ToolFunction {
                name: name.into(),
                arguments: serde_json::json!({}),
            },
        }
    }

    fn gate(state: &Arc<Mutex<SessionState>>) -> (ApprovalGate, crate::run::CancelFlag) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (flag, cancel) = crate::run::cancel_channel();
        (
            ApprovalGate::new(
                state.clone(),
                tx,
                cancel,
                Arc::new(PermissionManager::new(
                    PermissionsConfig::default(),
                    std::env::temp_dir(),
                )),
                None,
            ),
            flag,
        )
    }

    #[test]
    fn apply_patch_scope_covers_every_patched_file() {
        let root = std::path::Path::new("/repo");
        let (scopes, force) = scope_for_call(
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

    #[tokio::test]
    async fn read_only_tools_run_without_approval() {
        let state = Arc::new(Mutex::new(SessionState::default()));
        for name in crate::permissions::READ_ONLY_TOOLS {
            let (gate, _flag) = gate(&state);
            let decision = gate.decide(tool_call("t1", name)).await;
            assert!(matches!(decision, Decision::Run), "{name}");
        }
        assert!(state.lock().await.pending_approval.is_none());
    }

    #[tokio::test]
    async fn unknown_and_mutating_tools_require_approval() {
        let state = Arc::new(Mutex::new(SessionState::default()));
        for name in ["write", "delete", "some_future_bash_tool"] {
            let (gate, _flag) = gate(&state);
            let call = tool_call("t1", name);
            let pending = tokio::spawn(async move { gate.decide(call).await });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while state.lock().await.pending_approval.is_none() {
                assert!(std::time::Instant::now() < deadline, "{name} never parked");
                tokio::task::yield_now().await;
            }
            decide(&state, "t1".into(), PermissionAnswer::Deny).await;
            let decision = pending.await.unwrap();
            assert!(matches!(decision, Decision::Skip(_)), "{name}");
        }
    }

    #[tokio::test]
    async fn decide_ignores_stale_ids_and_delivers_current_one() {
        let state = Arc::new(Mutex::new(SessionState::default()));
        let (tx, mut rx) = oneshot::channel();
        state.lock().await.pending_approval = Some(("current".into(), tx));

        decide(&state, "stale".into(), PermissionAnswer::AllowSession).await;
        assert!(matches!(&state.lock().await.pending_approval, Some((id, _)) if id == "current"));
        assert!(
            rx.try_recv().is_err(),
            "stale id must not consume a decision"
        );

        decide(&state, "current".into(), PermissionAnswer::Deny).await;
        assert!(state.lock().await.pending_approval.is_none());
        assert_eq!(rx.await.unwrap(), PermissionAnswer::Deny);
    }

    #[tokio::test]
    async fn approval_grants_the_session_scope() {
        let state = Arc::new(Mutex::new(SessionState::default()));
        let permissions = Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            std::env::temp_dir(),
        ));
        let make_gate = |permissions: Arc<PermissionManager>| {
            let (tx, _rx) = mpsc::unbounded_channel();
            let (flag, cancel) = crate::run::cancel_channel();
            (
                ApprovalGate::new(state.clone(), tx, cancel, permissions, None),
                flag,
            )
        };
        let (gate, _flag) = make_gate(permissions.clone());

        let mut call = tool_call("t1", "write");
        call.function.arguments = serde_json::json!({ "path": "src/lib.rs" });
        let pending = tokio::spawn(async move { gate.decide(call).await });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while state.lock().await.pending_approval.is_none() {
            assert!(std::time::Instant::now() < deadline, "write never parked");
            tokio::task::yield_now().await;
        }
        decide(&state, "t1".into(), PermissionAnswer::AllowSession).await;
        assert!(matches!(pending.await.unwrap(), Decision::Run));

        // The grant generalized to the parent dir and lives in the session:
        // a sibling write now runs without asking again.
        let (gate, _flag) = make_gate(permissions);
        let mut sibling = tool_call("t2", "write");
        sibling.function.arguments = serde_json::json!({ "path": "src/other.rs" });
        assert!(matches!(gate.decide(sibling).await, Decision::Run));
        assert!(state.lock().await.pending_approval.is_none());
    }

    #[tokio::test]
    async fn flag_rearm_while_pending_does_not_cancel_the_approval() {
        // A re-arm (`set(false)`) must not disturb a pending approval;
        // with epoch-based cancellation it never writes, so a ready
        // `changed()` is always a genuine cancel.
        let state = Arc::new(Mutex::new(SessionState::default()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let (flag, cancel) = crate::run::cancel_channel();
        let permissions = Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            std::env::temp_dir(),
        ));
        let gate = ApprovalGate::new(state.clone(), tx, cancel, permissions, None);

        let mut call = tool_call("t1", "bash");
        call.function.arguments = serde_json::json!({ "command": "echo hi" });
        let pending = tokio::spawn(async move { gate.decide(call).await });
        while state.lock().await.pending_approval.is_none() {
            tokio::task::yield_now().await;
        }
        // Version bump with the same (false) value, delivered after the
        // gate subscribed but before the decision.
        flag.set(false);
        tokio::task::yield_now().await;
        decide(&state, "t1".into(), PermissionAnswer::AllowSession).await;
        assert!(matches!(pending.await.unwrap(), Decision::Run));
    }

    fn scripted_reviewer(
        result: Result<crate::auto_review::Decision, crate::auto_review::ReviewError>,
    ) -> (Reviewer, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        let reviewer: Reviewer = Arc::new(move |_tool, _scopes| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let result = result.clone();
            Box::pin(async move { result })
        });
        (reviewer, calls)
    }

    fn auto_review_gate(
        reviewer: Reviewer,
    ) -> (
        ApprovalGate,
        Arc<Mutex<SessionState>>,
        Arc<PermissionManager>,
    ) {
        let state = Arc::new(Mutex::new(SessionState::default()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let (_flag, cancel) = crate::run::cancel_channel();
        let permissions = Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            std::env::temp_dir(),
        ));
        permissions.toggle_auto_review();
        let gate = ApprovalGate::new(
            state.clone(),
            tx,
            cancel,
            permissions.clone(),
            Some(reviewer),
        );
        (gate, state, permissions)
    }

    /// The in-progress status line precedes the verdict and shares the call's
    /// id so the conversation view updates it in place under the tool card.
    #[tokio::test]
    async fn auto_review_posts_an_in_progress_line_before_the_verdict() {
        // The reviewer parks until released.
        let gate_open = Arc::new(tokio::sync::Notify::new());
        let proceed = gate_open.clone();
        let reviewer: Reviewer = Arc::new(move |_tool, _scopes| {
            let proceed = proceed.clone();
            Box::pin(async move {
                proceed.notified().await;
                Ok(crate::auto_review::Decision {
                    verdict: crate::auto_review::Verdict::Allow,
                    risk: crate::auto_review::Risk::Low,
                    rationale: "in-project edit".into(),
                })
            })
        });
        let state = Arc::new(Mutex::new(SessionState::default()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_flag, cancel) = crate::run::cancel_channel();
        let permissions = Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            std::env::temp_dir(),
        ));
        permissions.toggle_auto_review();
        let gate = ApprovalGate::new(state, tx, cancel, permissions, Some(reviewer));
        let pending = tokio::spawn(async move { gate.decide(tool_call("t1", "write")).await });

        let first = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for the in-progress line")
            .expect("channel closed");
        let AgentEvent::AutoReview { id, tone, text } = first else {
            panic!("expected the in-progress AutoReview, got {first:?}");
        };
        assert_eq!(id, "t1");
        assert_eq!(tone, Tone::Neutral);
        assert_eq!(text, "auto-review: reviewing…");

        gate_open.notify_one();
        assert!(matches!(pending.await.unwrap(), Decision::Run));

        let second = rx.recv().await.expect("verdict line");
        let AgentEvent::AutoReview { id, tone, text } = second else {
            panic!("expected the verdict AutoReview, got {second:?}");
        };
        assert_eq!(id, "t1", "the verdict replaces the in-progress line");
        assert_eq!(tone, Tone::Success, "an allow verdict reads as success");
        assert!(text.starts_with("auto-review allow"));
    }

    #[tokio::test]
    async fn auto_review_allow_runs_and_records_a_session_rule() {
        let (reviewer, calls) = scripted_reviewer(Ok(crate::auto_review::Decision {
            verdict: crate::auto_review::Verdict::Allow,
            risk: crate::auto_review::Risk::Low,
            rationale: "in-project edit".to_string(),
        }));
        let (gate, state, _permissions) = auto_review_gate(reviewer);
        assert!(matches!(
            gate.decide(tool_call("t1", "write")).await,
            Decision::Run
        ));
        assert!(state.lock().await.pending_approval.is_none());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the recorded rule answers the sibling call without the reviewer"
        );
        assert!(matches!(
            gate.decide(tool_call("t2", "write")).await,
            Decision::Run
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn auto_review_deny_skips_with_rationale_and_records_a_deny_rule() {
        let (reviewer, _calls) = scripted_reviewer(Ok(crate::auto_review::Decision {
            verdict: crate::auto_review::Verdict::Deny,
            risk: crate::auto_review::Risk::High,
            rationale: "rm -rf outside the project".to_string(),
        }));
        let (gate, _state, permissions) = auto_review_gate(reviewer);
        let mut call = tool_call("t1", "bash");
        call.function.arguments = serde_json::json!({ "command": "rm -rf /" });
        let decision = gate.decide(call).await;
        let Decision::Skip(message) = decision else {
            panic!("expected skip, got {decision:?}");
        };
        assert!(message.contains("rm -rf outside the project"));
        assert!(
            message.contains("auto-review:"),
            "the reviewer rationale replaces the generic guidance: {message}"
        );
        let mut sibling = tool_call("t2", "bash");
        sibling.function.arguments = serde_json::json!({ "command": "rm -rf /" });
        assert!(matches!(gate.decide(sibling).await, Decision::Skip(_)));
        assert!(matches!(
            permissions.check(&ToolKey::native("bash"), &["rm -rf /".to_string()]),
            PermissionCheck::Denied
        ));
    }

    #[tokio::test]
    async fn auto_review_failure_fails_closed_without_recording_a_rule() {
        let (reviewer, _calls) = scripted_reviewer(Err(crate::auto_review::ReviewError::Timeout {
            deadline: std::time::Duration::from_secs(30),
        }));
        let (gate, _state, permissions) = auto_review_gate(reviewer);
        let mut call = tool_call("t1", "write");
        call.function.arguments = serde_json::json!({ "path": "src/a.rs" });
        let Decision::Skip(message) = gate.decide(call).await else {
            panic!("expected skip");
        };
        assert!(message.contains("auto-review denied this action"));
        // No reviewer decision happened, so nothing was recorded: a retry
        // still falls through to the prompt (NeedsPrompt), not Denied.
        let tool = ToolKey::native("write");
        let scope = permissions.cwd().join("src/a.rs").display().to_string();
        assert!(matches!(
            permissions.check(&tool, &[scope]),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[tokio::test]
    async fn pre_decided_calls_never_reach_the_reviewer() {
        let (reviewer, calls) = scripted_reviewer(Ok(crate::auto_review::Decision {
            verdict: crate::auto_review::Verdict::Allow,
            risk: crate::auto_review::Risk::Low,
            rationale: String::new(),
        }));
        let (gate, _state, permissions) = auto_review_gate(reviewer);
        // Read-only: allowed by defaults.
        assert!(matches!(
            gate.decide(tool_call("t1", "read")).await,
            Decision::Run
        ));
        // Explicit deny rule: denied without review.
        permissions.add_session_rule(crate::permissions::PermissionRule {
            tool: ToolKey::native("write"),
            scope: Some(
                permissions
                    .cwd()
                    .join("src/blocked.rs")
                    .display()
                    .to_string(),
            ),
            effect: crate::permissions::Effect::Deny,
        });
        let mut call = tool_call("t2", "write");
        call.function.arguments = serde_json::json!({ "path": "src/blocked.rs" });
        assert!(matches!(gate.decide(call).await, Decision::Skip(_)));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "Allowed and Denied must short-circuit before the reviewer"
        );
    }
}
