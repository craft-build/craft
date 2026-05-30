use std::env;
use std::path::Path;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};

use craft_agent::mcp::{config as mcp_config, oauth as mcp_oauth};
use craft_agent::tools::ToolRegistry;
use craft_config::{load_env_files, load_permissions};
use craft_lua::PluginHost;
use craft_providers::provider::fetch_all_models;
use craft_providers::{copilot_auth, dynamic, openai_auth};
use craft_storage::StateDir;

pub async fn auth_login(provider: &str, storage: &StateDir) -> Result<()> {
    match provider {
        "openai" => openai_auth::login(storage).await?,
        "copilot" => copilot_auth::login()?,
        slug => dynamic::login(slug)?,
    }
    Ok(())
}

pub fn auth_logout(provider: &str, storage: &StateDir) -> Result<()> {
    match provider {
        "openai" => openai_auth::logout(storage)?,
        "copilot" => copilot_auth::logout()?,
        slug => dynamic::logout(slug)?,
    }
    Ok(())
}

pub async fn models() {
    fetch_all_models(|batch| {
        for model in batch.models {
            println!("{model}");
        }
        for warning in batch.warnings {
            eprintln!("warning: {warning}");
        }
    })
    .await;
}

pub async fn index(path: &str, no_plugins: bool) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);

    let mut host = if no_plugins {
        PluginHost::disabled()
    } else {
        PluginHost::new(Arc::clone(ToolRegistry::native_arc()))
            .context("initialize lua plugin host")?
    };

    let raw_config = host.load_init_files(&cwd).context("load init.lua files")?;

    let mut config = raw_config.unwrap_or_default().into_config(false);
    config.validate()?;
    config.permissions = load_permissions(&cwd);

    host.load_builtins(&config.plugins)
        .context("load builtin plugins")?;

    let abs_path = Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(path).to_path_buf());
    let input = serde_json::json!({"path": abs_path.to_str().unwrap_or(path)});
    let reg = ToolRegistry::native_arc();
    let entry = reg
        .get("index")
        .ok_or_else(|| color_eyre::eyre::eyre!("index tool not registered"))?;
    let inv = entry
        .tool
        .parse(&input)
        .map_err(|e| color_eyre::eyre::eyre!("parse index input: {e}"))?;
    let ctx = craft_agent::tools::cli_tool_ctx();
    let result: Result<craft_agent::ToolOutput, String> = inv.execute(&ctx).await.map_err(|e| e.to_string());
    match result {
        Ok(output) => print!("{}", output.as_text()),
        Err(e) => bail!("index failed: {e}"),
    }
    Ok(())
}

pub async fn mcp_auth(server: &str, storage: &StateDir) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let (config, _) = mcp_config::load_config(&cwd);
    let raw = config
        .mcp
        .get(server)
        .ok_or_else(|| color_eyre::eyre::eyre!("unknown MCP server: {server}"))?;
    let url = match mcp_config::parse_server(server.to_owned(), raw.clone())?.transport {
        mcp_config::Transport::Http { url, .. } => url,
        _ => color_eyre::eyre::bail!("server '{server}' is not an HTTP transport"),
    };
    mcp_oauth::authenticate(server, &url, None, storage).await?;
    eprintln!("Successfully authenticated with MCP server '{server}'");
    Ok(())
}

pub fn mcp_logout(server: &str, storage: &StateDir) -> Result<()> {
    let deleted = craft_storage::auth::delete_mcp_auth(storage, server)?;
    if deleted {
        eprintln!("Removed OAuth credentials for MCP server '{server}'");
    } else {
        eprintln!("No stored credentials for MCP server '{server}'");
    }
    Ok(())
}
