//! The runtime setup contract: the one place every execution surface (TUI,
//! headless print mode, ACP) obtains the shared session environment —
//! discovered instruction files, the workspace with its MCP handle
//! installed, and the permission engine.
//!
//! Surfaces keep their orchestration (MCP startup mode, reviewer install)
//! and differ only in how they call [`workspace_env`]: the TUI starts MCP
//! in the background so the first frame never blocks, headless and ACP
//! connect before the first prompt.

use std::path::Path;
use std::sync::Arc;

use crate::error::{InvalidSnafu, Result};
use crate::instructions::Instructions;
use crate::permissions::PermissionManager;
use crate::tools::Workspace;

/// The session environment every surface shares: discovered instruction
/// files, the workspace with its MCP handle already installed, and the
/// permission engine (constructed on every surface, even where nothing
/// consults it yet).
pub struct WorkspaceEnv {
    pub instructions: Instructions,
    pub workspace: Workspace,
    pub permissions: Arc<PermissionManager>,
    /// MCP config problems (unreadable/unparseable server entries); the
    /// surface surfaces them as startup notes.
    pub mcp_errors: crate::mcp::config::McpConfigErrors,
    /// Sandbox degradation warning (unsupported platform, or the required
    /// backend missing while confinement is requested); surfaced like
    /// `mcp_errors` at startup.
    pub sandbox_note: Option<String>,
}

/// How MCP servers start for the session.
pub enum McpStartup {
    /// `start_connected`: every server settles (bounded) before the first
    /// prompt — for surfaces with no frame to protect (headless, ACP).
    Connected,
    /// `start_with_events`: startup returns without waiting; the first turn
    /// gates on `ready`. The TUI routes MCP log notifications through the
    /// event channel.
    Background(crate::mcp::McpEvents),
}

/// Discover instructions and permission rules off the async worker, build
/// the workspace with the instructions loaded, and start MCP in the
/// surface's mode.
pub async fn workspace_env(
    cwd: &Path,
    mcp: McpStartup,
    sandbox: crate::sandbox::SandboxPolicy,
) -> Result<WorkspaceEnv> {
    let (instructions, permissions) = tokio::task::spawn_blocking({
        let cwd = cwd.to_path_buf();
        move || {
            let instructions = crate::instructions::load_instructions(&cwd.display().to_string());
            let permissions =
                PermissionManager::new(crate::permissions::load_permissions(&cwd), cwd);
            (instructions, permissions)
        }
    })
    .await
    .map_err(|e| {
        InvalidSnafu {
            reason: format!("resolving the session environment: {e}"),
        }
        .build()
    })?;
    let workspace = Workspace::new(cwd)
        .map_err(crate::error::client_error)?
        .with_loaded_instructions(instructions.loaded.clone());
    let (mcp_handle, mcp_errors) = match mcp {
        McpStartup::Connected => crate::mcp::start_connected(cwd).await,
        McpStartup::Background(events) => crate::mcp::start_with_events(cwd, events).await,
    };
    workspace.set_mcp(mcp_handle);
    workspace.set_sandbox_policy(sandbox.clone());
    let sandbox_note = if !sandbox.enforced() {
        None
    } else {
        match crate::sandbox::BackendStatus::detect() {
            crate::sandbox::BackendStatus::Available => None,
            crate::sandbox::BackendStatus::UnsupportedPlatform => Some(format!(
                "sandbox: unsupported on {}; commands run unsandboxed",
                std::env::consts::OS
            )),
            crate::sandbox::BackendStatus::BinaryMissing(binary) => Some(format!(
                "sandbox: backend '{binary}' is not installed; bash commands will fail \
                 until it is installed or sandbox.mode = \"off\" is set"
            )),
        }
    };
    Ok(WorkspaceEnv {
        instructions,
        workspace,
        permissions: Arc::new(permissions),
        mcp_errors,
        sandbox_note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn workspace_env_discovers_instructions_and_builds_every_piece() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "runtime-contract-marker instructions",
        )
        .unwrap();
        for startup in ["connected", "background"] {
            let mode = if startup == "connected" {
                McpStartup::Connected
            } else {
                McpStartup::Background(crate::mcp::McpEvents::default())
            };
            let env = workspace_env(dir.path(), mode, crate::sandbox::SandboxPolicy::off())
                .await
                .unwrap();
            assert!(
                env.instructions
                    .text
                    .contains("runtime-contract-marker instructions"),
                "{startup}: instructions must be discovered"
            );
            // The workspace canonicalizes its root (macOS /var symlink).
            assert_eq!(
                env.workspace.root(),
                dir.path().canonicalize().unwrap().as_path()
            );
            assert!(env.mcp_errors.is_empty());
            // The permission engine exists even where nothing consults it.
            assert!(!env.permissions.is_yolo());
        }
    }
}
