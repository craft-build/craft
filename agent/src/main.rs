use clap::{CommandFactory, Parser};
use clap_complete::generate;
use craft::{
    acp,
    cli::{Cli, Commands, run_print},
    config::Config,
    error::{AcpConnectionSnafu, Error, TuiSnafu},
    tui::{self, provider::live::CraftProvider},
};
use snafu::ResultExt;

#[tokio::main]
#[snafu::report]
async fn main() -> Result<(), Error> {
    let cli = Cli::parse();
    cli.warn_ignored_flags();
    cli.validate()?;
    match &cli.command {
        Some(Commands::Completions { shell }) => {
            let mut cmd = Cli::command();
            let bin = cmd.get_name().to_owned();
            generate(*shell, &mut cmd, bin, &mut std::io::stdout());
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
            let mut config = Config::load().await?;
            // G.1 run overrides land in the config the TUI builds its run
            // parameters from.
            config.agent.preamble = cli.effective_preamble(&config.agent.preamble);
            if let Some(max_turns) = cli.max_turns {
                config.agent.max_turns = Some(max_turns);
            }
            if cli.print {
                return run_print(&cli, config).await;
            }
            let cwd = std::env::current_dir().context(TuiSnafu {
                context: "resolving the current directory",
            })?;
            let mut provider = CraftProvider::new(config, cwd).await?;
            if let Some(spec) = &cli.model {
                provider = provider.with_model_spec(spec)?;
            }
            let provider = provider
                .with_resume_latest(cli.continue_session)
                .with_session(cli.session.clone())
                .with_permission_flags(cli.yolo, cli.auto_review);
            tui::run(provider).await.context(TuiSnafu {
                context: "running the terminal UI",
            })
        }
    }
}
