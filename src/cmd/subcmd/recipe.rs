use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;

use color_eyre::Result;
use color_eyre::eyre::Context;

use craft_agent::discovery::Discovery;
use craft_agent::recipe;

use crate::cli::{RecipeAction, RecipeCommand};
use crate::cmd::headless::{self, HeadlessOptions};
use crate::print::OutputFormat;

pub(crate) struct RecipeRunOptions {
    pub model: Option<String>,
    pub quiet: bool,
    pub yolo: bool,
    pub no_plugins: bool,
    pub no_session: bool,
    pub max_turns: Option<u32>,
    pub allowed_tools: Vec<String>,
    pub output_format: OutputFormat,
}

pub async fn run(args: RecipeCommand) -> Result<()> {
    match args.action {
        RecipeAction::List => list().await,
        RecipeAction::Run {
            name,
            model,
            output_format,
            no_session,
            quiet,
            yolo,
            no_plugins,
            param,
            max_turns,
            allowed_tools,
        } => {
            let mut overrides = HashMap::new();
            for raw in &param {
                if let Some((k, v)) = raw.split_once('=') {
                    overrides.insert(k.trim().to_string(), v.trim().to_string());
                } else {
                    color_eyre::eyre::bail!("invalid --param '{raw}', expected key=value");
                }
            }
            let opts = RecipeRunOptions {
                model,
                quiet,
                yolo,
                no_plugins,
                no_session,
                max_turns,
                allowed_tools,
                output_format,
            };
            run_by_name(&name, &mut overrides, opts).await
        }
    }
}

async fn list() -> Result<()> {
    let discovery = Discovery::from_env();
    let files = discovery.discover_files("recipes", &["yaml", "yml", "json"]);
    if files.is_empty() {
        eprintln!("no recipes found");
        return Ok(());
    }
    for f in &files {
        let recipe = match recipe::load(&f.path) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{}: {e}", f.name);
                continue;
            }
        };
        let name = recipe.name.as_deref().unwrap_or(&f.name);
        if let Some(desc) = &recipe.description {
            println!("{name}\t{desc}");
        } else {
            println!("{name}");
        }
    }
    Ok(())
}

async fn run_by_name(
    name: &str,
    overrides: &mut HashMap<String, String>,
    opts: RecipeRunOptions,
) -> Result<()> {
    let discovery = Discovery::from_env();
    let files = discovery.discover_files("recipes", &["yaml", "yml", "json"]);

    if let Some(f) = files.iter().find(|f| f.name == name) {
        return execute_recipe(&f.path, overrides, opts).await;
    }

    let mut matches = Vec::new();
    for f in &files {
        if let Ok(r) = recipe::load(&f.path)
            && r.name.as_deref() == Some(name)
        {
            matches.push(&f.path);
        }
    }
    match matches.len() {
        0 => color_eyre::eyre::bail!("recipe '{name}' not found"),
        1 => execute_recipe(matches[0], overrides, opts).await,
        _ => color_eyre::eyre::bail!("recipe '{name}' is ambiguous (multiple recipes match)"),
    }
}

pub(crate) async fn execute_recipe(
    path: &Path,
    overrides: &mut HashMap<String, String>,
    opts: RecipeRunOptions,
) -> Result<()> {
    let recipe = recipe::load(path).context("load recipe")?;

    for param in recipe.missing_required(overrides) {
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
        .resolve_parameters(overrides)
        .context("resolve recipe parameters")?;
    let prompt = recipe
        .render(&params, path)
        .context("render recipe template")?;

    if !opts.quiet {
        if let Some(name) = &recipe.name {
            eprintln!("running recipe: {name}");
        }
        if let Some(desc) = &recipe.description {
            eprintln!("{desc}");
        }
    }

    let model = recipe.model.clone().or(opts.model.clone());
    let outcome = headless::run_headless(HeadlessOptions {
        model,
        prompt,
        yolo: opts.yolo,
        no_plugins: opts.no_plugins,
        no_rtk: false,
        extra_excluded_tools: vec![],
        context: vec![],
        persist_session: !opts.no_session,
        max_turns: recipe.max_turns.or(opts.max_turns),
        allowed_tools: opts.allowed_tools,
        stream: !opts.quiet && matches!(opts.output_format, OutputFormat::Text),
        mode: craft_agent::AgentMode::Build,
    })
    .await?;
    headless::print_outcome(&outcome, opts.output_format);
    Ok(())
}
