mod api;
mod error;
pub mod language;
mod loader;
mod plugin_permissions;
mod runtime;
pub mod terminal_backend;

pub use api::command::{
    Anchor, Axis, Border, Dimension, Edge, FloatConfig, FloatConfigPatch, LuaCommandInfo,
    LuaCommandReader, Split, TitlePos, UiAction, WinCommand, WinEvent,
};
#[cfg(feature = "onnx")]
pub use api::embed::EmbedChannel;
pub use api::hooks::LuaHooks;
pub use error::PluginError;
pub use loader::{EventHandle, PluginHost};
pub use plugin_permissions::{Permission, PluginPermissions, denied_error};
pub use runtime::{ClickReply, RestoreItem, RestoreReply, SharedSandboxConfig};
pub use terminal_backend::{
    JobEvent as TerminalEvent, LocalTerminal, TerminalBackend, TerminalFuture, TerminalHandle,
    TerminalSpec, local_backend,
};

pub mod test_support {
    use crate::api::command::{LuaCommandInfo, LuaCommandReader, LuaCommandWriter};

    pub struct LuaCommandWriterHandle(LuaCommandWriter);

    impl LuaCommandWriterHandle {
        pub fn publish(&self, commands: Vec<LuaCommandInfo>) {
            self.0.publish(commands);
        }
    }

    pub fn lua_command_writer_pair() -> (LuaCommandWriterHandle, LuaCommandReader) {
        let (writer, reader) = LuaCommandWriter::new();
        (LuaCommandWriterHandle(writer), reader)
    }
}
