use craft::{
    acp,
    config::Config,
    error::{AcpConnectionSnafu, Error},
};
use snafu::ResultExt;

#[tokio::main]
#[snafu::report]
async fn main() -> Result<(), Error> {
    // The ACP client owns provider/model selection through session config
    // options; the config file supplies the provider catalog and agent defaults.
    let config = Config::load().await?;
    acp::serve(config).await.context(AcpConnectionSnafu)
}
