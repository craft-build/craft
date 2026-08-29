mod api;
mod error;
pub mod language;
mod loader;
mod pack;
pub(crate) mod plugin_permissions;
mod runtime;
pub mod terminal_backend;

pub use api::embed::EmbedChannel;
pub use api::hooks::LuaHooks;
pub use api::keymap::{KeymapEntry, KeymapReader, KeymapSnapshot};
pub use api::options::{OptionSpec, OptionType, PluginOptionSpecs};
pub use api::pack::{Declared, PackOp};
pub use api::util::command::{
    Anchor, Axis, Border, BuiltinAction, Dimension, Edge, FloatConfig, FloatConfigPatch,
    HintReader, HintSnapshot, LuaCommandInfo, LuaCommandReader, ModelRequest, SessionRequest,
    Split, TaskRequest, TitlePos, UiAction, UiReply, WinCommand, WinEvent, WinView,
};
pub use craft_agent::SessionEndReason;
pub use error::PluginError;
pub use loader::{EventHandle, LuaRecencySource, PluginHost, SKIPPED_PLUGIN_WARNING};
pub use pack::{
    DiscoveredPackage, Discovery, InstallReport, Interaction, MANAGED_GROUP, Origin, discover,
    discover_installed, install_declared, lockfile_path, sanitize_message, site_dir,
};
pub use plugin_permissions::{Permission, PluginPermissions, Requested, denied_error};
pub use runtime::{KILL_GRACE, RestoreItem, SharedSandboxConfig};
pub use terminal_backend::{
    JobEvent as TerminalEvent, LocalTerminal, TerminalBackend, TerminalFuture, TerminalHandle,
    TerminalSpec, local_backend,
};

pub mod test_support {
    use crate::KeymapReader;
    use crate::SessionEndReason;
    use crate::api::keymap::{KeymapEntry, KeymapWriter};
    use crate::api::util::command::{
        HintEntries, HintReader, HintWriter, LuaCommandInfo, LuaCommandReader, LuaCommandWriter,
    };
    use crate::loader::EventHandle;
    use crate::runtime::Request;

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

    /// Stands in for the Lua thread publishing a plugin's status hints.
    pub struct HintWriterHandle(HintWriter);

    impl HintWriterHandle {
        pub fn publish(&self, entries: HintEntries) {
            self.0.publish(entries);
        }
    }

    pub fn hint_writer_pair() -> (HintWriterHandle, HintReader) {
        let (writer, reader) = HintWriter::new();
        (HintWriterHandle(writer), reader)
    }

    pub fn keymap_reader_with(entries: Vec<KeymapEntry>) -> KeymapReader {
        let (writer, reader) = KeymapWriter::new();
        writer.publish(entries);
        reader
    }

    /// Live `EventHandle` paired with a probe that observes every dispatched
    /// request. The probe is unused by most callers; it exists so a test can
    /// hold a live runtime while exercising the override path.
    pub fn probed_event_handle() -> (EventHandle, RequestProbe) {
        let (tx, rx) = flume::unbounded();
        (EventHandle::from_tx(tx), RequestProbe(rx))
    }

    /// Sink for requests dispatched by a probed `EventHandle`.
    pub struct RequestProbe(flume::Receiver<Request>);

    impl RequestProbe {
        /// `Some(())` when the probed handle dispatched a request since the
        /// last call, `None` when the channel is empty. Used by keymap tests
        /// to assert whether a plugin override callback actually fired.
        pub fn try_recv(&self) -> Option<()> {
            self.0.try_recv().ok().map(|_| ())
        }

        /// Next dispatched slash command as `(command, args, depth)`, draining
        /// other requests.
        pub fn try_recv_command(&self) -> Option<(String, String, u8)> {
            while let Ok(req) = self.0.try_recv() {
                if let Request::RunCommand {
                    command,
                    args,
                    depth,
                    ..
                } = req
                {
                    return Some((command.to_string(), args, depth));
                }
            }
            None
        }

        /// Next `SessionEnd` request as the session being left behind and why.
        pub fn try_recv_end_session(
            &self,
        ) -> Option<(craft_storage::id::CraftId, SessionEndReason)> {
            while let Ok(req) = self.0.try_recv() {
                if let Request::EndSession(end) = req {
                    return Some((end.session, end.reason));
                }
            }
            None
        }

        /// Next fired autocmd as `(event, data)`, draining other requests.
        pub fn try_recv_autocmd(&self) -> Option<(String, serde_json::Value)> {
            while let Ok(req) = self.0.try_recv() {
                if let Request::FireAutocmd { event, data } = req {
                    return Some((event, data));
                }
            }
            None
        }
    }
}
