use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use color_eyre::Result;
use color_eyre::eyre::Context;

use craft_agent::recipe;

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
        no_plugins: args.no_plugins,
        no_rtk: false,
        extra_excluded_tools: vec![],
        context: vec![],
        persist_session: !args.no_session,
        max_turns: args.max_turns,
        allowed_tools: args.allowed_tools,
        stream: !args.quiet && matches!(args.output_format, OutputFormat::Text),
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
    let recipe = recipe::load(path).context("load recipe")?;
    let mut overrides = HashMap::new();
    for raw in &args.param {
        if let Some((k, v)) = raw.split_once('=') {
            overrides.insert(k.trim().to_string(), v.trim().to_string());
        } else {
            color_eyre::eyre::bail!("invalid --param '{raw}', expected key=value");
        }
    }

    for param in recipe.missing_required(&overrides) {
        let label = param.description.as_deref().unwrap_or(&param.name);
        print!("{label}: ");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            color_eyre::eyre::bail!(
                "missing required recipe parameter '{}' (no stdin available; pass via --param {}=...)",
                param.name,
                param.name
            );
        }
        let line = line.trim().to_string();
        if line.is_empty() {
            color_eyre::eyre::bail!("missing required recipe parameter '{}'", param.name);
        }
        overrides.insert(param.name.clone(), line);
    }

    let params = recipe
        .resolve_parameters(&overrides)
        .context("resolve recipe parameters")?;
    let prompt = recipe
        .render(&params, path)
        .context("render recipe template")?;

    if !args.quiet {
        if let Some(name) = &recipe.name {
            eprintln!("running recipe: {name}");
        }
        if let Some(desc) = &recipe.description {
            eprintln!("{desc}");
        }
    }

    let model = recipe.model.clone().or(args.model.clone());
    let outcome = headless::run_headless(HeadlessOptions {
        model,
        prompt,
        yolo: args.yolo,
        no_plugins: args.no_plugins,
        no_rtk: false,
        extra_excluded_tools: vec![],
        context: vec![],
        persist_session: !args.no_session,
        max_turns: recipe.max_turns.or(args.max_turns),
        allowed_tools: args.allowed_tools,
        stream: !args.quiet && matches!(args.output_format, OutputFormat::Text),
    })
    .await?;
    headless::print_outcome(&outcome, args.output_format);
    Ok(())
}
