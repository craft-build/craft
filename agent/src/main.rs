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
use std::io::IsTerminal;

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
            let mut config = Config::load().await?;
            cli.apply_thinking(&mut config)?;
            acp::serve(config).await.context(AcpConnectionSnafu)
        }
        Some(Commands::Models) => {
            let config = Config::load().await?;
            craft::subcmd::models(config).await
        }
        Some(Commands::Stats { sessions }) => craft::subcmd::stats(*sessions),
        Some(Commands::Doctor { export }) => {
            let config = Config::load().await?;
            craft::subcmd::doctor(config, *export).await
        }
        Some(Commands::Update { yes, no_color }) => craft::update::update(*yes, *no_color).await,
        Some(Commands::Rollback) => craft::update::rollback(),
        Some(Commands::Prompt {
            variant,
            plan,
            tools,
            names,
        }) => craft::subcmd::prompt(variant.clone(), *plan, *tools, *names).await,
        Some(Commands::Term { action }) => {
            craft::term::run(action.clone(), cli.permission_policy()?).await
        }
        Some(Commands::Recipe { action }) => match action.clone() {
            craft::cli::RecipeAction::List => craft::subcmd::recipe_list().await,
            craft::cli::RecipeAction::Run {
                name,
                param,
                model,
                output_format,
            } => {
                craft::subcmd::recipe_run(
                    &name,
                    &param,
                    model,
                    output_format,
                    cli.permission_policy()?,
                )
                .await
            }
        },
        None => {
            let mut config = Config::load().await?;
            cli.apply_thinking(&mut config)?;
            // G.1 run overrides land in the config the TUI builds its run
            // parameters from.
            // G.6 first run: auto-detect providers from credential env
            // vars when craft.bml configures none.
            let setup_notes = craft::setup::first_run(&mut config);
            cli.apply_prompt_overrides(&mut config);
            if let Some(max_turns) = cli.max_turns {
                config.agent.max_turns = Some(max_turns);
            }
            if cli.print {
                for note in &setup_notes {
                    eprintln!("warning: {note}");
                }
                return run_print(&cli, config).await;
            }
            let cwd = std::env::current_dir().context(TuiSnafu {
                context: "resolving the current directory",
            })?;
            // The startup prompt (positional `PROMPT` / piped stdin) is
            // resolved before the provider and the terminal: a read failure
            // must surface as a plain error, not inside the TUI.
            let startup_prompt = craft::cli::resolve_prompt_input(
                cli.initial_prompt.clone(),
                &mut std::io::stdin(),
                std::io::stdin().is_terminal(),
            )?;
            let mut provider = CraftProvider::new(config, cwd).await?;
            if let Some(spec) = &cli.model {
                provider = provider.with_model_spec(spec)?;
            }
            let provider = provider
                .with_startup_notes(setup_notes)
                .with_resume_latest(cli.continue_session)
                .with_session(cli.session.clone())
                .with_permission_flags(
                    matches!(cli.permission_policy()?, craft::cli::PermissionPolicy::Yolo),
                    cli.auto_review,
                )
                .with_cli_tool_rules(cli.tool_policy()?)
                .with_custom_commands(!cli.no_commands)
                .with_initial_mode(cli.run_mode());
            tui::run(provider, startup_prompt).await.context(TuiSnafu {
                context: "running the terminal UI",
            })
        }
    }
}
