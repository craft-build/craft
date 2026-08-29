use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::Context;

use craft_agent::tools::ToolRegistry;
use craft_config::{load_env_files, load_permissions};
use craft_lua::PluginHost;
use craft_storage::StateDir;
use craft_storage::flow::FlowStore;

use crate::setup;

pub async fn run(
    yolo: bool,
    auto_review: bool,
    cwd: Option<PathBuf>,
    no_plugins: bool,
    no_jit: bool,
) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    craft_providers::model_registry::load_from_storage(&storage);

    let cwd = match cwd {
        Some(p) => {
            std::env::set_current_dir(&p).with_context(|| format!("chdir to {}", p.display()))?;
            p
        }
        None => env::current_dir().unwrap_or_else(|_| ".".into()),
    };
    load_env_files(&cwd);

    let mut plugin_host =
        PluginHost::with_jit(Arc::clone(ToolRegistry::native_arc()), None, !no_jit)
            .context("initialize lua plugin host")?;

    let (config, warnings) = super::super::load_plugins(
        &mut plugin_host,
        no_plugins,
        super::super::BuiltinFailure::Fatal,
        craft_lua::Interaction::None,
        |host, names, _| {
            let mut config = host
                .load_init_files_or_skip(no_plugins, &cwd)
                .context("load init.lua files")?
                .unwrap_or_default()
                .into_config(&names(host)?)
                .context("invalid config")?;
            config.permissions = load_permissions(&cwd);

            if yolo || config.always_yolo {
                config.permissions.yolo = true;
                config.sandbox.mode = craft_config::SandboxMode::Off;
                config.sandbox.enabled = false;
            }
            if auto_review || config.always_auto_review {
                config.permissions.auto_review = true;
            }
            config.validate()?;
            Ok(config)
        },
    )?;
    super::super::report_warnings(warnings);

    plugin_host
        .event_handle()
        .set_sandbox_config(config.sandbox.clone());

    let timeouts = craft_providers::Timeouts {
        connect: config.provider.connect_timeout,
        low_speed: config.provider.low_speed_timeout,
        stream: config.provider.stream_timeout,
    };

    let model = setup::resolve_model(None, &config.provider, &storage).await?;

    setup::init_logging(&config.storage);
    setup::install_panic_log_hook();
    setup::warn_ignored_provider_fields();

    let (mcp_config, _mcp_config_errors) = craft_agent::mcp::config::load_config(&cwd);

    let prompt_slots = plugin_host.event_handle().collect_prompt_slots();

    let flow_store = Arc::new(FlowStore::new(&storage).context("init flow store")?);

    craft_acp::run(craft_acp::AcpParams {
        model,
        config: config.agent,
        permissions_config: config.permissions,
        timeouts,
        initial_wd: cwd,
        mcp_config,
        prompt_slots: Arc::new(prompt_slots),
        yolo,
        model_policy: Arc::new(config.provider.model_policy.clone()),
        plugin_rules: plugin_host.plugin_rules(),
        plugin_host,
        flow_store,
    })
    .await
}
