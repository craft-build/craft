use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use color_eyre::Result;
use color_eyre::eyre::Context;

use crate::cli::RunCommand;
use crate::cmd::headless::{self, HeadlessOptions};
use crate::print::OutputFormat;

pub async fn run(args: RunCommand) -> Result<()> {
    if let Some(target) = args.prompt.clone() {
        let path = PathBuf::from(&target);
        if is_recipe_path(&path) {
            return run_recipe(&path, args).await;
        }
    }

    let prompt = match args.prompt {
        Some(p) => p,
        None => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf).context("read stdin")?;
            buf
        }
    };
    let outcome = headless::run_headless(HeadlessOptions {
        model: args.model,
        prompt,
        yolo: args.yolo,
        auto_review: args.auto_review,
        no_plugins: args.no_plugins,
        extra_excluded_tools: vec![],
        context: vec![],
        persist_session: !args.no_session,
        max_turns: args.max_turns,
        allowed_tools: args.allowed_tools,
        stream: !args.quiet && matches!(args.output_format, OutputFormat::Text),
        mode: craft_agent::AgentMode::Build,
    })
    .await?;
    headless::print_outcome(&outcome, args.output_format);
    Ok(())
}

fn is_recipe_path(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml") | Some("json")
    )
}

async fn run_recipe(path: &Path, args: RunCommand) -> Result<()> {
    let mut overrides = HashMap::new();
    for raw in &args.param {
        if let Some((k, v)) = raw.split_once('=') {
            overrides.insert(k.trim().to_string(), v.trim().to_string());
        } else {
            color_eyre::eyre::bail!("invalid --param '{raw}', expected key=value");
        }
    }
    super::recipe::execute_recipe(
        path,
        &mut overrides,
        super::recipe::RecipeRunOptions {
            model: args.model,
            quiet: args.quiet,
            yolo: args.yolo,
            auto_review: args.auto_review,
            no_plugins: args.no_plugins,
            no_session: args.no_session,
            max_turns: args.max_turns,
            allowed_tools: args.allowed_tools,
            output_format: args.output_format,
        },
    )
    .await
}
