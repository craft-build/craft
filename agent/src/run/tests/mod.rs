//! Run-loop integration tests, split by subsystem. The shared scripted
//! harness lives here; the topic modules hold the tests.

mod advisor;
mod cli_policy;
mod compression;
mod dispatch;
mod overflow;
mod spec;
mod turns;

use crate::provider::ModelHandle;
use crate::testing::{Attempt, ScriptedProvider};

/// A run-loop model over the scripted attempts, and the provider recording
/// every request it serves.
fn stream_turns(turns: Vec<Attempt>) -> (ModelHandle, ScriptedProvider) {
    let provider = ScriptedProvider::new(Vec::new());
    for turn in turns {
        provider.push(turn);
    }
    (model(&provider), provider)
}

/// A run-loop model handle over a scripted provider.
fn model(provider: &ScriptedProvider) -> ModelHandle {
    ModelHandle::scripted(crate::testing::scripted_model(
        provider,
        crate::testing::model_info("scripted", 200_000, 8_192, false, false),
    ))
}

/// One tool-call spec for [`crate::testing::tool_turn`].
fn tool_event(id: &str, name: &str, args: serde_json::Value) -> crate::testing::ToolCallSpec {
    (id.to_owned(), name.to_owned(), args)
}
