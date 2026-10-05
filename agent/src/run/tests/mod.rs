//! Run-loop integration tests, split by subsystem. The shared mock harness
//! lives here; the topic modules hold the tests.

use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

mod advisor;
mod cli_policy;
mod compression;
mod dispatch;
mod overflow;
mod spec;
mod turns;

fn stream_turns(
    turns: Vec<Vec<MockStreamEvent>>,
) -> (MockCompletionModel, Vec<Vec<MockStreamEvent>>) {
    let model = MockCompletionModel::from_stream_turns(turns.clone());
    (model, turns)
}

fn tool_event(id: &str, name: &str, args: serde_json::Value) -> MockStreamEvent {
    MockStreamEvent::tool_call(id, name, args)
}

fn length_final(total_tokens: u64) -> MockStreamEvent {
    use rig_core::streaming::StreamFinal;
    MockStreamEvent::FinalResponse(
        StreamFinal::new(
            "mock",
            rig_core::completion::Usage {
                total_tokens,
                ..Default::default()
            },
        )
        .with_finish_reason(rig_core::completion::FinishReason::Length),
    )
}
