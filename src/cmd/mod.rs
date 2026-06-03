mod migrate;
mod subcmd;
mod tui;

use color_eyre::Result;
use color_eyre::eyre::Context;

use craft_storage::StateDir;

use crate::cli::{AuthAction, Cli, Command, McpAction, MigrateAction};
use crate::update;

pub async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Auth { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            match action {
                AuthAction::Login { provider } => subcmd::auth_login(&provider, &storage).await?,
                AuthAction::Logout { provider } => subcmd::auth_logout(&provider, &storage)?,
            }
        }
        Some(Command::Index { path }) => {
            subcmd::index(&path, cli.no_plugins).await?;
        }
        Some(Command::Models) => {
            subcmd::models().await;
        }
        Some(Command::Mcp { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            match action {
                McpAction::Auth { server } => subcmd::mcp_auth(&server, &storage).await?,
                McpAction::Logout { server } => subcmd::mcp_logout(&server, &storage)?,
            }
        }
        Some(Command::Update { yes, no_color }) => {
            update::update(yes, no_color).await.map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
        }
        Some(Command::Rollback) => {
            update::rollback().map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
        }
        Some(Command::Migrate { action }) => match action {
            MigrateAction::Xdg => migrate::xdg()?,
        },
        None => {
            tui::run(cli).await?;
        }
    }
    Ok(())
}
