//! TEMPORARY debug harness: print the TUI thinking choices for every
//! discovered model, mirroring bootstrap's catalog_choices().
use std::collections::BTreeMap;

use animist::provider::ModelInfo;

#[tokio::main]
async fn main() {
    let config = craft::config::Config::load().await.unwrap();
    let catalog = craft::provider::load_catalog().await;
    let mut catalogs: BTreeMap<String, Vec<ModelInfo>> = BTreeMap::new();
    for (name, provider_config) in &config.providers {
        match craft::provider::ProviderHandle::from_config(provider_config, Some(&catalog)).await {
            Ok(provider) => match provider.models(provider_config, Some(&catalog)).await {
                Ok(models) => {
                    catalogs.insert(name.clone(), models);
                }
                Err(error) => println!("{name}: discovery failed: {error}"),
            },
            Err(error) => println!("{name}: construction failed: {error}"),
        }
    }
    for (provider, models) in &catalogs {
        for info in models {
            let id = info.ids.first().cloned().unwrap_or_default();
            let t = &info.features.thinking;
            let choices: Vec<String> =
                craft::thinking::choices_for(craft::thinking::ThinkingConfig::Off, Some(info))
                    .iter()
                    .map(|c| c.to_string())
                    .collect();
            println!(
                "{provider}/{id}\n  supported={} required={} efforts={:?}\n  picker={choices:?}",
                t.supported, t.required, t.efforts
            );
        }
    }
}
