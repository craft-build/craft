use craft_acp::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Validate configuration without resolving credentials or contacting providers.
    // Clients are constructed lazily when the GUI selects a provider.
    let _config = Config::load().await?;
    Ok(())
}
