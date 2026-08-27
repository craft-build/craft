-- The tasks plugin owns the /tasks picker over the subagents spawned by the
-- Rust `task` tool. picker.lua registers the command and the keymap when this
-- file is loaded, so the two cannot be enabled apart.

require("picker")
