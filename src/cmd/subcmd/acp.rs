use std::env;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::Context;

use craft_agent::tools::ToolRegistry;
use craft_config::{load_env_files, load_permissions};
use craft_lua::PluginHost;
use craft_storage::StateDir;

use crate::setup;

pub async fn run(yolo: bool) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    craft_providers::tier_map::load_from_storage(&storage);

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);

    let mut plugin_host = PluginHost::new(Arc::clone(ToolRegistry::native_arc()), None)
        .context("initialize lua plugin host")?;

    let raw_config = plugin_host
        .load_init_files(&cwd)
        .context("load init.lua files")?;

    let mut config = raw_config
        .unwrap_or_default()
        .into_config(false)
        .context("invalid config")?;
    config.permissions = load_permissions(&cwd);

    if yolo || config.always_yolo {
        config.permissions.allow_all = true;
        config.sandbox.mode = craft_config::SandboxMode::Off;
        config.sandbox.enabled = false;
    }
    config.validate()?;

    plugin_host
        .load_builtins(&config.plugins)
        .context("load builtin plugins")?;

    if let Some(handle) = plugin_host.event_handle().as_ref() {
        handle.set_sandbox_config(config.sandbox.clone());
    }

    let timeouts = craft_providers::Timeouts {
        connect: config.provider.connect_timeout,
        low_speed: config.provider.low_speed_timeout,
        stream: config.provider.stream_timeout,
    };

    let model = setup::resolve_model(None, &config.provider, &storage).await?;

    setup::init_logging(&storage, &config.storage);
    setup::install_panic_log_hook();

    let (mcp_config, _mcp_config_errors) = craft_agent::mcp::config::load_config(&cwd);

    let prompt_slots = plugin_host
        .event_handle()
        .map(|h| h.collect_prompt_slots())
        .unwrap_or_default();

    craft_acp::run(craft_acp::AcpParams {
        model,
        config: config.agent,
        permissions_config: config.permissions,
        timeouts,
        initial_wd: cwd,
        mcp_config,
        prompt_slots: Arc::new(prompt_slots),
        yolo,
        plugin_host,
    })
    .await
}
