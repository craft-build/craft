//! The approval responder: the TUI's half of the approval interaction.
//! Permission rules and auto-review live in the harness policy layer;
//! what remains here is the overlay — park the ask on the shared slots,
//! render the permission request, and deliver the user's answer.

use std::sync::Arc;

use tokio::sync::{Mutex, mpsc, oneshot};

use crate::permissions::{ASK_TIMEOUT, FILE_WRITE_TOOLS, PermissionAnswer};
use crate::harness::approval::{Approval, ApprovalAsk, ApprovalResponder};

use super::SharedPark;
use crate::tui::provider::cards;
use crate::tui::provider::{AgentEvent, Status, Tone, ToolCallData};

/// The TUI approval responder: a `NeedsPrompt` decision from the policy
/// layer parks here until the user answers the permission form (or the
/// turn is cancelled). Approval runs the tool; rejection reports the
/// denial back to the model — the layer derives both from the answer.
pub(super) struct ApprovalGate {
    park: SharedPark,
    tx: mpsc::UnboundedSender<AgentEvent>,
    /// The session's per-turn cancellation cell; each ask races whatever
    /// turn is current.
    cancel: std::sync::Arc<arc_swap::ArcSwap<animist::agent::Cancellation>>,
    /// Serializes asks: the overlay is one modal with one pending slot, so
    /// two concurrently gated calls must queue rather than clobber each
    /// other's parked oneshot.
    ask_lock: Arc<Mutex<()>>,
}

impl ApprovalGate {
    pub(super) fn new(
        park: SharedPark,
        tx: mpsc::UnboundedSender<AgentEvent>,
        cancel: std::sync::Arc<arc_swap::ArcSwap<animist::agent::Cancellation>>,
    ) -> Self {
        Self {
            park,
            tx,
            cancel,
            ask_lock: Arc::new(Mutex::new(())),
        }
    }
}

impl ApprovalResponder for ApprovalGate {
    fn ask(&self, ask: ApprovalAsk<'_>) -> crate::harness::BoxFuture<Approval> {
        // Owned copies: the boxed future outlives the borrowed ask.
        let id = ask.id.to_owned();
        let name = ask.name.to_owned();
        let arguments = ask.input.clone();
        let scopes = ask.scopes.to_vec();
        let low_risk = ask.low_risk;
        let park = self.park.clone();
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();
        let ask_lock = self.ask_lock.clone();
        Box::pin(async move {
            // Held across the whole ask: the overlay shows one permission
            // form at a time, so a second gated call waits for this one.
            let _serial = ask_lock.lock().await;
            let turn_cancel = (*cancel.load_full()).clone();
            if turn_cancel.is_cancelled() {
                return Approval::Stop {
                    reason: "cancelled by client".into(),
                };
            }
            // A server-declared read-only tool still prompts, but
            // the card can phrase it in a neutral, low-risk tone.
            if low_risk {
                let _ = tx.send(AgentEvent::Notice {
                    tone: Tone::Neutral,
                    text: "server declares this tool read-only (low risk)".into(),
                });
            }
            let _ = tx.send(AgentEvent::AssistantEnd);
            let _ = tx.send(AgentEvent::ToolCall(ToolCallData {
                id: id.clone(),
                kind: cards::tool_head(&name, &arguments),
                lines: Vec::new(),
                awaiting_approval: true,
                image: None,
            }));
            let _ = tx.send(AgentEvent::StatusChanged(Status::WaitingApproval));
            let (files, commands) = display_context(&name, &arguments, &scopes);
            // Register before emitting: an answer racing in on event receipt
            // must find the oneshot already parked.
            let (decision_tx, mut decision_rx) = oneshot::channel();
            park.lock().await.pending_approval = Some((id.clone(), decision_tx));
            let _ = tx.send(AgentEvent::PermissionRequest {
                id: id.clone(),
                tool: name.clone(),
                scopes: scopes.clone(),
                files,
                commands,
            });

            let answer = tokio::select! {
                biased;
                () = turn_cancel.cancelled() => {
                    park.lock().await.pending_approval = None;
                    let _ = tx.send(AgentEvent::PermissionResolved { id });
                    return Approval::Stop {
                        reason: "cancelled by client".into(),
                    };
                }
                decision = tokio::time::timeout(ASK_TIMEOUT, &mut decision_rx) => {
                    park.lock().await.pending_approval = None;
                    let _ = tx.send(AgentEvent::PermissionResolved { id });
                    decision
                        .unwrap_or(Ok(PermissionAnswer::Deny))
                        .unwrap_or(PermissionAnswer::Deny)
                }
            };
            let _ = tx.send(AgentEvent::StatusChanged(Status::Running));
            if answer.is_allow() {
                Approval::Allow {
                    remember: Some(answer),
                }
            } else {
                Approval::Deny {
                    message: denied_message(&name, &scopes, answer.guidance()),
                    remember: Some(answer),
                }
            }
        })
    }
}

/// The model-visible denial text: the guidance the answer carried, else
/// the engine's standard denial.
fn denied_message(name: &str, scopes: &[String], guidance: Option<&str>) -> String {
    let tool = crate::permissions::ToolKey::parse(name);
    match guidance {
        Some(guidance) => crate::permissions::PermissionError::with_guidance(
            &tool.to_string(),
            scopes,
            guidance.to_string(),
        )
        .to_string(),
        None => crate::permissions::PermissionError::new(&tool.to_string(), scopes).to_string(),
    }
}

/// Display context for the permission-prompt overlay: the files a
/// file-write tool touches, or the raw bash command(s). The scopes already
/// carry resolved paths; files simply re-present them for the form.
fn display_context(
    name: &str,
    args: &serde_json::Value,
    scopes: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut files = Vec::new();
    let mut commands = Vec::new();
    if name == "bash" {
        if let Some(command) = args.get("command").and_then(|v| v.as_str()) {
            match crate::permissions::bash::permission_scopes(command) {
                Some(split) => commands.extend(split.scopes),
                None => commands.push(command.to_string()),
            }
        }
    } else if FILE_WRITE_TOOLS.contains(&name) {
        files.extend(scopes.iter().cloned());
    }
    (files, commands)
}

/// Deliver the user's decision to a tool call waiting on it, if the id
/// matches the currently pending one.
pub(super) async fn decide(park: &SharedPark, id: String, answer: PermissionAnswer) {
    let mut slots = park.lock().await;
    if matches!(&slots.pending_approval, Some((pid, _)) if *pid == id)
        && let Some((_, decision)) = slots.pending_approval.take()
    {
        let _ = decision.send(answer);
    }
}

#[cfg(test)]
mod tests {
    use super::super::ParkSlots;
    use super::*;
    use crate::harness::approval::ApprovalResponder;

    type Cell = std::sync::Arc<arc_swap::ArcSwap<animist::agent::Cancellation>>;

    fn cell() -> Cell {
        std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
            animist::agent::Cancellation::default(),
        ))
    }

    /// A responder over a fresh park and channel; the cell lets a test fire
    /// the current turn's cancellation.
    fn gate() -> (
        ApprovalGate,
        Arc<Mutex<ParkSlots>>,
        mpsc::UnboundedReceiver<AgentEvent>,
        Cell,
    ) {
        let park = Arc::new(Mutex::new(ParkSlots::default()));
        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = cell();
        (
            ApprovalGate::new(park.clone(), tx, cancel.clone()),
            park,
            rx,
            cancel,
        )
    }

    fn bash_ask<'a>(
        id: &'a str,
        input: &'a serde_json::Value,
        scopes: &'a [String],
    ) -> ApprovalAsk<'a> {
        ApprovalAsk {
            id,
            name: "bash",
            input,
            scopes,
            low_risk: false,
        }
    }

    fn bash_input() -> serde_json::Value {
        serde_json::json!({ "command": "echo hi" })
    }

    fn bash_scopes() -> Vec<String> {
        vec!["echo hi".to_string()]
    }

    /// Wait until an ask parks on the shared slots.
    async fn wait_parked(park: &Arc<Mutex<ParkSlots>>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while park.lock().await.pending_approval.is_none() {
            assert!(std::time::Instant::now() < deadline, "the ask never parked");
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn an_ask_parks_and_the_answer_allows() {
        let (gate, park, _rx, _cell) = gate();
        let pending = {
            let input = bash_input();
            let scopes = bash_scopes();
            tokio::spawn(async move { gate.ask(bash_ask("t1", &input, &scopes)).await })
        };
        wait_parked(&park).await;
        decide(&park, "t1".into(), PermissionAnswer::AllowSession).await;
        match pending.await.unwrap() {
            Approval::Allow { remember } => {
                assert_eq!(remember, Some(PermissionAnswer::AllowSession))
            }
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rejection_carries_the_denial_message_and_remember_scope() {
        let (gate, park, _rx, _cell) = gate();
        let pending = {
            let input = bash_input();
            let scopes = bash_scopes();
            tokio::spawn(async move { gate.ask(bash_ask("t1", &input, &scopes)).await })
        };
        wait_parked(&park).await;
        decide(
            &park,
            "t1".into(),
            PermissionAnswer::DenyWithGuidance("use read instead".into()),
        )
        .await;
        match pending.await.unwrap() {
            Approval::Deny { message, remember } => {
                assert!(
                    message.contains("use read instead"),
                    "guidance missing from: {message}"
                );
                assert_eq!(
                    remember,
                    Some(PermissionAnswer::DenyWithGuidance(
                        "use read instead".into()
                    ))
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    /// A cancellation arriving while the ask is parked stops the run — the
    /// responder's answer to a client that went away.
    #[tokio::test]
    async fn a_cancelled_turn_stops_the_parked_ask() {
        let (gate, park, _rx, cancel) = gate();
        let pending = {
            let input = bash_input();
            let scopes = bash_scopes();
            tokio::spawn(async move { gate.ask(bash_ask("t1", &input, &scopes)).await })
        };
        wait_parked(&park).await;
        let parked = cancel.load_full();
        parked.cancel();
        match pending.await.unwrap() {
            Approval::Stop { reason } => assert_eq!(reason, "cancelled by client"),
            other => panic!("expected Stop, got {other:?}"),
        }
        assert!(park.lock().await.pending_approval.is_none());
    }

    /// The permission-request event carries the display context (files vs
    /// commands), and the resolution event closes the form.
    #[tokio::test]
    async fn permission_request_event_carries_display_context() {
        let (gate, park, mut rx, _cell) = gate();
        let pending = {
            let input = bash_input();
            let scopes = bash_scopes();
            tokio::spawn(async move { gate.ask(bash_ask("t1", &input, &scopes)).await })
        };
        wait_parked(&park).await;
        let mut request = None;
        let mut resolved = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::PermissionRequest {
                    id,
                    tool,
                    files,
                    commands,
                    ..
                } => request = Some((id, tool, files, commands)),
                AgentEvent::PermissionResolved { id } if id == "t1" => resolved = true,
                _ => {}
            }
        }
        let (id, tool, files, commands) = request.expect("no PermissionRequest event");
        assert_eq!(id, "t1");
        assert_eq!(tool, "bash");
        assert!(files.is_empty());
        assert_eq!(commands, vec!["echo hi".to_string()]);
        decide(&park, "t1".into(), PermissionAnswer::Deny).await;
        let _ = pending.await;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, AgentEvent::PermissionResolved { id } if id == "t1") {
                resolved = true;
            }
        }
        assert!(resolved, "no PermissionResolved event");
    }

    /// A re-arm (the next turn storing a fresh cancellation) must not
    /// disturb a pending approval: the ask watches the cancellation it
    /// captured, so only a real cancel ends it early.
    #[tokio::test]
    async fn a_new_turns_cancellation_does_not_cancel_a_parked_approval() {
        let (gate, park, _rx, cancel) = gate();
        let pending = {
            let input = bash_input();
            let scopes = bash_scopes();
            tokio::spawn(async move { gate.ask(bash_ask("t1", &input, &scopes)).await })
        };
        wait_parked(&park).await;
        cancel.store(std::sync::Arc::new(animist::agent::Cancellation::default()));
        tokio::task::yield_now().await;
        decide(&park, "t1".into(), PermissionAnswer::AllowSession).await;
        assert!(matches!(pending.await.unwrap(), Approval::Allow { .. }));
    }

    /// Two gated calls running concurrently (a parallel wave) must never
    /// show two permission prompts at once: the overlay is one modal with
    /// one pending slot, so asks serialize inside the responder.
    #[tokio::test]
    async fn two_parallel_asks_never_show_two_prompts() {
        let (gate, park, mut rx, _cell) = gate();
        // Both asks go through the one shared responder, as the Arc'd
        // trait object is shared across a turn's concurrent calls.
        let gate = Arc::new(gate);
        let second = {
            let gate = Arc::clone(&gate);
            let input = bash_input();
            let scopes = bash_scopes();
            tokio::spawn(async move { gate.ask(bash_ask("t2", &input, &scopes)).await })
        };
        let first = {
            let gate = Arc::clone(&gate);
            let input = bash_input();
            let scopes = bash_scopes();
            tokio::spawn(async move { gate.ask(bash_ask("t1", &input, &scopes)).await })
        };
        // The first ask's permission request is sent while it holds the
        // serialization lock; wait for it (not just the park).
        let parked_id = loop {
            match next_event(&mut rx).await {
                AgentEvent::PermissionRequest { id, .. } => break id,
                _ => continue,
            }
        };
        // While it is unanswered, the queued ask can neither take the slot
        // nor show a second prompt: it is blocked on the lock, not timing.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut extra_requests = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, AgentEvent::PermissionRequest { .. }) {
                extra_requests += 1;
            }
        }
        assert_eq!(extra_requests, 0, "one prompt at a time");
        assert_eq!(
            park.lock()
                .await
                .pending_approval
                .as_ref()
                .map(|(id, _)| id.clone()),
            Some(parked_id.clone()),
            "the pending slot holds the first ask"
        );
        decide(&park, parked_id.clone(), PermissionAnswer::AllowOnce).await;
        // Only now does the queued ask take the overlay.
        let next_id = loop {
            match next_event(&mut rx).await {
                AgentEvent::PermissionRequest { id, .. } => break id,
                _ => continue,
            }
        };
        assert_ne!(parked_id, next_id, "the second ask parks after the first");
        decide(&park, next_id, PermissionAnswer::Deny).await;
        let (first, second) = tokio::join!(first, second);
        let outcomes = [first.unwrap(), second.unwrap()];
        assert!(
            outcomes
                .iter()
                .any(|outcome| matches!(outcome, Approval::Allow { .. })),
            "the answered ask allows: {outcomes:?}"
        );
        assert!(
            outcomes
                .iter()
                .any(|outcome| matches!(outcome, Approval::Deny { .. })),
            "the queued ask denies: {outcomes:?}"
        );
    }

    #[tokio::test]
    async fn decide_ignores_stale_ids_and_delivers_current_one() {
        let (_gate, park, _rx, _cell) = gate();
        let (tx, mut rx) = oneshot::channel();
        park.lock().await.pending_approval = Some(("current".into(), tx));

        decide(&park, "stale".into(), PermissionAnswer::AllowSession).await;
        assert!(matches!(&park.lock().await.pending_approval, Some((id, _)) if id == "current"));
        assert!(
            rx.try_recv().is_err(),
            "a stale id must not consume a decision"
        );

        decide(&park, "current".into(), PermissionAnswer::Deny).await;
        assert!(park.lock().await.pending_approval.is_none());
        assert_eq!(rx.await.unwrap(), PermissionAnswer::Deny);
    }

    /// The next TUI event, failing (not hanging) when none arrives.
    async fn next_event(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> AgentEvent {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("event before the deadline")
            .expect("channel open")
    }
}
