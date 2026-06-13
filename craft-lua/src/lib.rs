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
pub use error::PluginError;
pub use loader::{EventHandle, PluginHost};
pub use plugin_permissions::{denied_error, Permission, PluginPermissions};
pub use runtime::{ClickReply, RestoreItem, RestoreReply};
pub use terminal_backend::{
    JobEvent as TerminalEvent, LocalTerminal, TerminalBackend, TerminalFuture, TerminalHandle,
    TerminalSpec, local_backend,
};
#[cfg(feature = "onnx")]
pub use api::embed::EmbedChannel;

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
