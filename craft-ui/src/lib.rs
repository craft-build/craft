//! Multi-session ratatui event loop; each session owns an `App` plus an agent
//! running on tokio tasks. `AgentHandles` bundles all flume channels to a
//! session's agent. `dispatch()` processes `Action`s returned by `App::update()`.
//! Scroll and drag events are coalesced from the queue to avoid jank.

pub mod animation;
pub mod app;
pub mod chat;
mod clipboard;
mod components;
pub use components::command::{BUILTIN_COMMANDS, BuiltinCommand};
pub use components::keybindings;
mod highlight;
pub use highlight::highlight_ansi;
mod hyperlink;
pub mod image;
mod image_render;
mod markdown;
mod render_worker;
mod selection;
pub mod splash;
mod storage_writer;
mod text_buffer;
mod theme;
pub mod update;

mod agent;
mod event_loop;
mod input;
mod terminal;
mod watch;

use color_eyre::Result;
use craft_agent::ToolOutput;
use craft_providers::Message;
use craft_providers::TokenUsage;

pub type AppSession = craft_storage::sessions::Session<Message, TokenUsage, ToolOutput>;

pub(crate) use agent::AgentCommand;
pub use event_loop::EventLoopParams;

/// How a UI generation ended. On `Reload`, each tab carries its in-memory
/// session so the caller reopens everything without re-reading from disk.
pub enum RunOutcome {
    Exit {
        session_id: Option<craft_storage::id::CraftId>,
        code: i32,
    },
    Reload {
        tabs: Vec<AppSession>,
        focused: usize,
    },
}

pub fn run(
    handle: tokio::runtime::Handle,
    params: EventLoopParams,
    initial_prompt: Option<String>,
) -> Result<RunOutcome> {
    let _guard = handle.enter();
    let (_guard, mut terminal) = terminal::TerminalGuard::init()?;
    let report = event_loop::EventLoop::new(&mut terminal, params)?.run(initial_prompt)?;
    Ok(match report.exit_request() {
        components::ExitRequest::Reload => RunOutcome::Reload {
            tabs: report.tabs().to_vec(),
            focused: report.focused(),
        },
        _ => RunOutcome::Exit {
            session_id: report.session_id(),
            code: report.exit_code(),
        },
    })
}
