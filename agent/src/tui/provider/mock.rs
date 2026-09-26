//! MockProvider: replays the prototype's scripted "flaky session refresh"
//! turn. Kept for seam and UI tests (compiled in test builds only).

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::sleep;

use super::{
    AgentEvent, Command, LineKind, PlanItem, Provider, Status, Tone, ToolCallData, ToolKind,
    ToolLine, TouchedFile,
};

pub struct MockProvider;

const FILE: &str = "src/auth/refresh.ts";

fn seed_plan() -> Vec<PlanItem> {
    vec![
        PlanItem {
            label: "Read the refresh token path".into(),
            done: true,
            active: false,
        },
        PlanItem {
            label: "Find where concurrent refresh calls happen".into(),
            done: true,
            active: false,
        },
        PlanItem {
            label: "Guard refreshToken() with a mutex".into(),
            done: false,
            active: true,
        },
        PlanItem {
            label: "Run the auth test suite".into(),
            done: true,
            active: false,
        },
        PlanItem {
            label: "Open PR against main".into(),
            done: false,
            active: false,
        },
    ]
}

fn seed_files() -> Vec<TouchedFile> {
    vec![TouchedFile {
        path: FILE.into(),
        status: "modified".into(),
        tone: Tone::Warning,
    }]
}

fn read_call(id: &str) -> ToolCallData {
    ToolCallData {
        id: id.into(),
        awaiting_approval: false,
        kind: ToolKind::Read {
            path: FILE.into(),
            summary: "38 lines".into(),
        },
        lines: [
            "export async function refreshToken(old: string) {",
            "  const res = await api.post('/auth/refresh', { token: old })",
            "  session.token = res.token",
            "  return res.token",
            "}",
        ]
        .iter()
        .map(|t| ToolLine {
            kind: LineKind::Context,
            text: t.to_string(),
            ..Default::default()
        })
        .collect(),
    }
}

fn grep_call(id: &str) -> ToolCallData {
    ToolCallData {
        id: id.into(),
        awaiting_approval: false,
        kind: ToolKind::Grep {
            pattern: "refreshToken(".into(),
            summary: "6 matches in 4 files".into(),
        },
        lines: [
            "src/auth/refresh.ts:12",
            "src/auth/session.ts:44",
            "src/auth/session.ts:61",
            "src/http/interceptor.ts:23",
            "src/http/interceptor.ts:58",
            "src/hooks/useAuth.ts:19",
        ]
        .iter()
        .map(|t| ToolLine {
            kind: LineKind::Context,
            text: t.to_string(),
            ..Default::default()
        })
        .collect(),
    }
}

fn edit_call(id: &str) -> ToolCallData {
    use LineKind::*;
    ToolCallData {
        id: id.into(),
        awaiting_approval: true,
        kind: ToolKind::Edit {
            path: FILE.into(),
            summary: String::new(),
        },
        lines: [
            (Context, "export async function refreshToken(old: string) {"),
            (
                Del,
                "  const res = await api.post('/auth/refresh', { token: old })",
            ),
            (Del, "  session.token = res.token"),
            (Del, "  return res.token"),
            (Add, "  if (inflight) return inflight"),
            (
                Add,
                "  inflight = api.post('/auth/refresh', { token: old })",
            ),
            (
                Add,
                "    .then(res => { session.token = res.token; return res.token })",
            ),
            (Add, "    .finally(() => { inflight = null })"),
            (Add, "  return inflight"),
            (Context, "}"),
        ]
        .iter()
        .map(|(kind, t)| ToolLine {
            kind: *kind,
            text: t.to_string(),
            ..Default::default()
        })
        .collect(),
    }
}

fn bash_call(id: &str) -> ToolCallData {
    ToolCallData {
        id: id.into(),
        awaiting_approval: false,
        kind: ToolKind::Bash {
            cmd: "pnpm test auth/refresh.spec.ts".into(),
        },
        lines: vec![
            ToolLine {
                kind: LineKind::Muted,
                text: "Running 6 tests in auth/refresh.spec.ts".into(),
                ..Default::default()
            },
            ToolLine {
                kind: LineKind::Success,
                text: "✓ refreshToken() returns cached promise for concurrent calls (4 passed)"
                    .into(),
                ..Default::default()
            },
            ToolLine {
                kind: LineKind::Success,
                text: "✓ refreshToken() clears inflight after resolution (2 passed)".into(),
                ..Default::default()
            },
        ],
    }
}

/// Sleep `ms`, then emit `ev`. Returns silently if the channel closed.
async fn emit(tx: &mpsc::UnboundedSender<AgentEvent>, ms: u64, ev: AgentEvent) {
    sleep(Duration::from_millis(ms)).await;
    let _ = tx.send(ev);
}

/// The full scripted first turn, ported from the prototype's seed session.
async fn run_first_turn(tx: mpsc::UnboundedSender<AgentEvent>) {
    emit(&tx, 500, AgentEvent::StatusChanged(Status::Thinking)).await;
    emit(
        &tx,
        800,
        AgentEvent::AssistantText("Looking at the refresh path first.".into()),
    )
    .await;
    // One scripted notice so app-level tests can assert its rendering.
    emit(
        &tx,
        300,
        AgentEvent::Notice {
            tone: Tone::Warning,
            text: "retrying (attempt 1): stream closed early — next in 500ms".into(),
        },
    )
    .await;
    emit(&tx, 500, AgentEvent::StatusChanged(Status::Running)).await;
    emit(&tx, 400, AgentEvent::ToolCall(read_call("mock-1-read"))).await;
    emit(&tx, 700, AgentEvent::ToolCall(grep_call("mock-1-grep"))).await;
    emit(
        &tx,
        700,
        AgentEvent::AssistantText(
            "Two requests hitting /auth/refresh at once both fire — the second one overwrites \
             session.token with a stale response. Adding a mutex so only one refresh runs at a \
             time and concurrent callers await the same promise."
                .into(),
        ),
    )
    .await;
    emit(&tx, 900, AgentEvent::ToolCall(edit_call("mock-1-edit"))).await;
    emit(&tx, 900, AgentEvent::ToolCall(bash_call("mock-1-bash"))).await;
    emit(
        &tx,
        500,
        AgentEvent::AssistantText(
            "All 6 tests pass. Diff above is ready — approve it (^Y) and I'll open the PR.".into(),
        ),
    )
    .await;
    emit(&tx, 200, AgentEvent::StatusChanged(Status::WaitingApproval)).await;
    emit(&tx, 0, AgentEvent::TokenUsage("45.9K/1M (4%)".into())).await;
}

/// Follow-up turns: the same canned acknowledgement the prototype uses.
async fn run_canned_turn(tx: mpsc::UnboundedSender<AgentEvent>) {
    emit(&tx, 500, AgentEvent::StatusChanged(Status::Thinking)).await;
    emit(
        &tx,
        900,
        AgentEvent::AssistantText("Got it — looking into that now.".into()),
    )
    .await;
    emit(&tx, 300, AgentEvent::StatusChanged(Status::Done)).await;
}

/// Initial state the provider pushes as soon as it starts.
fn emit_initial_state(
    tx: &mpsc::UnboundedSender<AgentEvent>,
    plan: &[PlanItem],
    files: &[TouchedFile],
) {
    let _ = tx.send(AgentEvent::PlanSet(plan.to_vec()));
    let _ = tx.send(AgentEvent::FilesSet(files.to_vec()));
    let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
    let _ = tx.send(AgentEvent::TokenUsage("44.8K/1M (4%)".into()));
}

impl Provider for MockProvider {
    fn start(
        self,
    ) -> (
        mpsc::UnboundedSender<Command>,
        mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Command>();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<AgentEvent>();

        tokio::spawn(async move {
            let mut turns: u32 = 0;
            let mut plan = seed_plan();
            let mut files = seed_files();
            let mut current: Option<AbortHandle> = None;
            let mut usage_turns: u32 = 0;

            emit_initial_state(&evt_tx, &plan, &files);

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    Command::SendMessage(_, _mode) => {
                        if let Some(h) = current.take() {
                            h.abort();
                        }
                        turns += 1;
                        let tx = evt_tx.clone();
                        let handle = if turns == 1 {
                            tokio::spawn(run_first_turn(tx))
                        } else {
                            tokio::spawn(run_canned_turn(tx))
                        };
                        current = Some(handle.abort_handle());
                    }
                    Command::Shell { command, .. } => {
                        // The scripted mock cannot run shells; echo the
                        // command back as a finished bash card.
                        let _ = evt_tx.send(AgentEvent::ToolCall(ToolCallData {
                            id: format!("shell-{turns}"),
                            kind: ToolKind::Bash { cmd: command },
                            lines: Vec::new(),
                            awaiting_approval: false,
                        }));
                        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
                    }
                    Command::Approve { .. } => {
                        // The scripted diff completes the linked plan step.
                        if let Some(p) = plan.get_mut(2) {
                            p.done = true;
                            p.active = false;
                        }
                        for f in &mut files {
                            f.status = "approved".into();
                            f.tone = Tone::Success;
                        }
                        let _ = evt_tx.send(AgentEvent::PlanSet(plan.clone()));
                        let _ = evt_tx.send(AgentEvent::FilesSet(files.clone()));
                        let _ = evt_tx.send(AgentEvent::AssistantText(
                            "Applied the diff to src/auth/refresh.ts — opening the PR now.".into(),
                        ));
                        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
                        usage_turns += 1;
                        let _ = evt_tx.send(AgentEvent::TokenUsage(format!(
                            "{:.1}K/1M (5%)",
                            46.1 + usage_turns as f64 * 0.2
                        )));
                    }
                    Command::Reject { .. } => {
                        for f in &mut files {
                            f.status = "reverted".into();
                            f.tone = Tone::Neutral;
                        }
                        let _ = evt_tx.send(AgentEvent::FilesSet(files.clone()));
                        let _ = evt_tx.send(AgentEvent::AssistantText(
                            "Discarded the proposed changes.".into(),
                        ));
                        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
                    }
                    Command::AnswerPermission { id, answer } => {
                        // Mirrors the gate: the request resolves and the
                        // tool proceeds (allow answers) or is skipped.
                        let _ = evt_tx.send(AgentEvent::PermissionResolved { id });
                        let allowed = answer.is_allow();
                        let _ = evt_tx.send(AgentEvent::AssistantText(format!(
                            "Permission answer: {}",
                            answer.encode()
                        )));
                        let _ = evt_tx.send(AgentEvent::StatusChanged(if allowed {
                            Status::Running
                        } else {
                            Status::Done
                        }));
                    }
                    Command::Interrupt => {
                        if let Some(h) = current.take() {
                            h.abort();
                        }
                        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
                    }
                    Command::Reset => {
                        if let Some(h) = current.take() {
                            h.abort();
                        }
                        turns = 0;
                        plan = seed_plan();
                        files = seed_files();
                        emit_initial_state(&evt_tx, &plan, &files);
                    }
                    Command::Clear => {
                        // Context cleared conceptually; mock keeps its script state.
                    }
                    Command::Undo => {
                        // No filesystem writes happen under the mock provider.
                        let _ = evt_tx.send(AgentEvent::AssistantText("Nothing to undo.".into()));
                    }
                    Command::SelectModel { .. } => {
                        // Fixed script: model selection is outside the demo.
                    }
                    Command::ToggleAutoReview => {
                        // The mock has no permission gate to re-route.
                        let _ =
                            evt_tx.send(AgentEvent::AssistantText("auto-review toggled.".into()));
                    }
                    Command::GetUsage => {
                        // The scripted demo tracks no real usage.
                        let _ = evt_tx.send(AgentEvent::UsageSnapshot(Vec::new()));
                    }
                    Command::Compact => {
                        // Mirror the real provider: announce the run, then the
                        // (scripted) neutral no-op.
                        let _ = evt_tx.send(AgentEvent::Notice {
                            tone: Tone::Info,
                            text: "compacting context…".into(),
                        });
                        let _ = evt_tx.send(AgentEvent::Notice {
                            tone: Tone::Neutral,
                            text: "context already compact".into(),
                        });
                    }
                    Command::LoadSession { .. } => {
                        let _ = evt_tx.send(AgentEvent::Notice {
                            tone: Tone::Neutral,
                            text: "the mock provider keeps a single scripted session".into(),
                        });
                    }
                }
            }
        });

        (cmd_tx, evt_rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::AgentMode;

    async fn recv(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> AgentEvent {
        tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .expect("timed out waiting for provider event")
            .unwrap()
    }

    /// The seam end-to-end: initial state -> scripted first turn -> approval
    /// flips the linked plan step and marks files approved.
    #[tokio::test]
    async fn scripted_turn_then_approve() {
        let (tx, mut rx) = MockProvider.start();

        // Initial state arrives immediately.
        let ev = rx.recv().await.unwrap();
        assert!(matches!(ev, AgentEvent::PlanSet(_)));
        let ev = rx.recv().await.unwrap();
        assert!(matches!(ev, AgentEvent::FilesSet(_)));

        tx.send(Command::SendMessage(
            "fix the flaky refresh".into(),
            AgentMode::Build,
        ))
        .unwrap();
        let mut saw_read = false;
        let mut saw_grep = false;
        let mut saw_edit = false;
        let mut saw_bash = false;
        loop {
            match recv(&mut rx).await {
                AgentEvent::ToolCall(t) => match t.kind {
                    ToolKind::Read { .. } => saw_read = true,
                    ToolKind::Grep { .. } => saw_grep = true,
                    ToolKind::Edit { .. } => saw_edit = true,
                    ToolKind::Bash { .. } => saw_bash = true,
                },
                AgentEvent::StatusChanged(Status::WaitingApproval) => break,
                _ => {}
            }
        }
        assert!(saw_read && saw_grep && saw_edit && saw_bash);

        tx.send(Command::Approve {
            id: "mock-1-edit".into(),
            always: false,
        })
        .unwrap();
        let mut plan_done = false;
        let mut files_approved = false;
        loop {
            match recv(&mut rx).await {
                AgentEvent::PlanSet(plan) => plan_done = plan[2].done && !plan[2].active,
                AgentEvent::FilesSet(files) => {
                    files_approved = files[0].status == "approved" && files[0].tone == Tone::Success
                }
                AgentEvent::StatusChanged(Status::Done) if plan_done && files_approved => break,
                _ => {}
            }
        }
        assert!(plan_done && files_approved);
    }

    /// Interrupt stops the scripted turn and returns status to done.
    #[tokio::test]
    async fn interrupt_stops_turn() {
        let (tx, mut rx) = MockProvider.start();
        // Drain initial state.
        for _ in 0..4 {
            rx.recv().await.unwrap();
        }
        tx.send(Command::SendMessage("go".into(), AgentMode::Build))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx.send(Command::Interrupt).unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            ev,
            AgentEvent::StatusChanged(Status::Done) | AgentEvent::AssistantText(_)
        ));
    }

    /// `/compact` announces the run before the (scripted) no-op result, so the
    /// status surface shows an indicator while it works.
    #[tokio::test]
    async fn compact_announces_the_run_before_the_result() {
        let (tx, mut rx) = MockProvider.start();
        // Drain initial state.
        for _ in 0..4 {
            rx.recv().await.unwrap();
        }
        tx.send(Command::Compact).unwrap();
        let first = recv(&mut rx).await;
        assert!(
            matches!(first, AgentEvent::Notice { tone: Tone::Info, ref text }
                if text.starts_with("compacting context")),
            "{first:?}"
        );
    }
}
