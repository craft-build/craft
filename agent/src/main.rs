use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use craft::{
    acp,
    config::Config,
    error::{AcpConnectionSnafu, Error, TuiSnafu},
    tui::{self, provider::live::CraftProvider},
};
use snafu::ResultExt;

#[derive(Parser)]
#[command(
    version,
    about = "Craft coding agent: launches the interactive TUI by default",
    long_about = "Craft coding agent. With no subcommand, launches the interactive terminal UI \
                  backed by the configured providers in ~/.config/craft/agent.toml."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Serve the agent loop over the ACP protocol on stdin/stdout (for editors
    /// and other ACP clients).
    Acp,
    /// Emit shell completion scripts for the given shell to stdout.
    Completions { shell: Shell },
}

#[tokio::main]
#[snafu::report]
async fn main() -> Result<(), Error> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Completions { shell }) => {
            let mut cmd = Cli::command();
            let bin = cmd.get_name().to_owned();
            generate(shell, &mut cmd, bin, &mut std::io::stdout());
            Ok(())
        }
        Some(Commands::Acp) => {
            // The ACP client owns provider/model selection through session
            // config options; the config file supplies the provider catalog
            // and agent defaults.
            let config = Config::load().await?;
            acp::serve(config).await.context(AcpConnectionSnafu)
        }
        None => {
            let config = Config::load().await?;
            let cwd = std::env::current_dir().context(TuiSnafu {
                context: "resolving the current directory",
            })?;
            let provider = CraftProvider::new(config, cwd).await?;
            tui::run(provider).await.context(TuiSnafu {
                context: "running the terminal UI",
            })
        }
    }
}
