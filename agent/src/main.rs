use craft::{acp, config::Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The ACP client owns provider/model selection through session config
    // options; the config file supplies the provider catalog and agent defaults.
    let config = Config::load().await?;
    acp::serve(config)
        .await
        .map_err(|error| anyhow::anyhow!("ACP connection failed: {error}"))
}
