use std::env;
use std::io::{self, IsTerminal, Read};
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::Context;

use craft_agent::command::{self, CustomCommand};
use craft_agent::tools::ToolRegistry;
use craft_config::{load_env_files, load_permissions};
use craft_lua::PluginHost;
use craft_storage::StateDir;

use crate::setup;

pub struct DesktopArgs {
    pub model: Option<String>,
    pub continue_session: bool,
    pub session: Option<String>,
    pub yolo: bool,
    pub no_commands: bool,
    pub no_plugins: bool,
    pub no_rtk: bool,
    pub prompt: Option<String>,
}

type DesktopSession = craft_storage::sessions::Session<
    craft_providers::Message,
    craft_providers::TokenUsage,
    craft_agent::ToolOutput,
>;

fn discover_commands(disable: bool) -> Vec<CustomCommand> {
    if disable {
        return Vec::new();
    }
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    command::discover_commands(&cwd)
}

fn resolve_session(
    continue_session: bool,
    session_id: Option<String>,
    model: &str,
    cwd: &str,
    storage: &StateDir,
) -> Result<DesktopSession> {
    if let Some(id) = session_id {
        return DesktopSession::load(&id, storage).map_err(|e| color_eyre::eyre::eyre!("{e}"));
    }
    if continue_session {
        match DesktopSession::latest(cwd, storage) {
            Ok(Some(session)) => return Ok(session),
            Ok(None) => {
                tracing::info!("no previous session found for this directory, starting new");
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to load latest session, starting new");
            }
        }
    }
    Ok(DesktopSession::new(model, cwd))
}

fn read_initial_prompt(cli_prompt: Option<String>) -> Result<Option<String>> {
    match cli_prompt {
        Some(p) => Ok(Some(p)),
        None if !io::stdin().is_terminal() => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf).context("read stdin")?;
            Ok(Some(buf))
        }
        None => Ok(None),
    }
}

pub async fn run(args: DesktopArgs) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    craft_providers::tier_map::load_from_storage(&storage);

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());

    load_env_files(&cwd);

    let plugin_host = if args.no_plugins {
        PluginHost::disabled()
    } else {
        PluginHost::new(Arc::clone(ToolRegistry::native_arc()), None)
            .context("initialize lua plugin host")?
    };

    let raw_config = plugin_host
        .load_init_files(&cwd)
        .context("load init.lua files")?;

    let mut config = raw_config
        .unwrap_or_default()
        .into_config(args.no_rtk)
        .context("invalid config")?;
    config.permissions = load_permissions(&cwd);

    if args.yolo || config.always_yolo {
        config.permissions.allow_all = true;
        config.sandbox.mode = craft_config::SandboxMode::Off;
        config.sandbox.enabled = false;
    }
    config.validate()?;

    if let Some(handle) = plugin_host.event_handle().as_ref() {
        handle.set_sandbox_config(config.sandbox.clone());
    }

    let timeouts = craft_providers::Timeouts {
        connect: config.provider.connect_timeout,
        low_speed: config.provider.low_speed_timeout,
        stream: config.provider.stream_timeout,
    };

    let model = setup::resolve_model(args.model.as_deref(), &config.provider, &storage).await?;

    setup::init_logging(&storage, &config.storage);
    setup::install_panic_log_hook();

    let commands = discover_commands(args.no_commands);

    let cwd_str = cwd.to_string_lossy().into_owned();
    let session = resolve_session(
        args.continue_session,
        args.session,
        &model.spec(),
        &cwd_str,
        &storage,
    )?;

    let initial_prompt = read_initial_prompt(args.prompt)?;

    let cwd_for_mcp = cwd.clone();
    let (mcp_handle, mcp_config_errors) = craft_agent::mcp::start(&cwd_for_mcp).await;
    let provider: Arc<dyn craft_providers::provider::Provider> = Arc::from(
        craft_providers::provider::from_model(&model, timeouts)
            .await
            .context("create provider")?,
    );
    let handle = tokio::runtime::Handle::current();

    let params = craft_desktop::DesktopParams {
        model,
        commands,
        session,
        storage,
        config: config.agent,
        compression: config.compression,
        tool_output_lines: config.ui.tool_output_lines,
        permissions: Arc::new(craft_agent::permissions::PermissionManager::new(
            config.permissions,
            cwd.clone(),
        )),
        timeouts,
        provider,
        mcp_handle,
        mcp_config_errors,
    };

    craft_desktop::run(handle, params, initial_prompt).context("run desktop")?;

    Ok(())
}
