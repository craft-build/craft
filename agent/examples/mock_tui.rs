//! Debug harness: run the TUI against the scripted mock provider.
//!
//! `cargo run -p craft --example mock_tui` — used by the migration's PTY
//! tests to watch streaming behavior without a live LLM.
//!
//! `MOCK_TUI_STREAM=1` swaps in a streaming variant: timed
//! `AssistantDelta` chunks (the live-provider path) instead of the
//! script's settled `AssistantText` messages.

use craft::tui;
use craft::tui::provider::mock::MockProvider;
use craft::tui::provider::{AgentEvent, Command, Provider, Status};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    let streaming = std::env::var_os("MOCK_TUI_STREAM").is_some();
    let result = if streaming {
        tui::run(StreamingMock, None).await
    } else {
        let provider = MockProvider::default();
        let prompt = std::env::var("MOCK_TUI_PROMPT").ok();
        tui::run(provider, prompt).await
    };
    if let Err(e) = result {
        eprintln!("mock_tui: {e}");
        std::process::exit(1);
    }
}

/// Emits a turn of timed deltas so the streaming render path is exercised
/// end to end (the way the live provider does).
struct StreamingMock;

impl Provider for StreamingMock {
    fn start(
        self,
    ) -> (
        mpsc::UnboundedSender<Command>,
        mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Command>();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<AgentEvent>();
        tokio::spawn(async move {
            // Drain commands so sends never fail.
            while cmd_rx.recv().await.is_some() {}
        });
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Thinking));
            // Reasoning stream first (the GLM pattern), markdown-flavored.
            for i in 0..12 {
                let delta = if i % 3 == 0 {
                    format!("*thinking* chunk {i} `code` ", )
                } else {
                    format!("reason {i} ")
                };
                let _ = evt_tx.send(AgentEvent::ReasoningDelta(delta));
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            }
            let _ = evt_tx.send(AgentEvent::AssistantEnd);
            let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Running));
            // Markdown-heavy assistant stream: headings, fences, bold.
            let mut text = String::new();
            for i in 0..20 {
                let delta = match i {
                    0 => "## Fix plan\n\n".to_string(),
                    5 => "```rust\nfn main() {}\n```\n\n".to_string(),
                    12 => "**bold** and `inline` ".to_string(),
                    _ => format!("word{i} ", ),
                };
                text.push_str(&delta);
                let _ = evt_tx.send(AgentEvent::AssistantDelta(delta));
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
            let _ = evt_tx.send(AgentEvent::AssistantEnd);
            let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
        });
        (cmd_tx, evt_rx)
    }
}
