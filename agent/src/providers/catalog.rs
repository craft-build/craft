//! Provider catalogs: crate-owned model entries and config/discovery merge.

use rig_core::model::{Model, ModelList};

use crate::config::ProviderConfig;

/// One selectable model in a provider catalog, in crate-owned form (the rig
/// listing DTO stays inside this module).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogModel {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub context_length: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

pub trait CatalogEntry {
    fn id(&self) -> &str;
    fn context_length_mut(&mut self) -> &mut Option<u32>;
    fn max_output_tokens_mut(&mut self) -> &mut Option<u32>;
}

impl CatalogEntry for CatalogModel {
    fn id(&self) -> &str {
        &self.id
    }
    fn context_length_mut(&mut self) -> &mut Option<u32> {
        &mut self.context_length
    }
    fn max_output_tokens_mut(&mut self) -> &mut Option<u32> {
        &mut self.max_output_tokens
    }
}

impl CatalogModel {
    /// Display label: the catalog name, falling back to the model id.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    fn from_rig(model: Model) -> Self {
        Self {
            id: model.id,
            name: model.name,
            description: model.description,
            context_length: model.context_length,
            max_output_tokens: model.max_output_tokens,
        }
    }
}

/// Configured fields win; omitted fields preserve discovery metadata. New IDs
/// are added, and the final catalog is sorted by ID.
pub(crate) fn merge_catalog(config: &ProviderConfig, discovered: ModelList) -> Vec<CatalogModel> {
    let mut models: std::collections::BTreeMap<String, CatalogModel> = discovered
        .into_iter()
        .map(|model| {
            let id = model.id.clone();
            (id, CatalogModel::from_rig(model))
        })
        .collect();
    for (id, settings) in &config.models {
        let model = models.entry(id.clone()).or_insert_with(|| CatalogModel {
            id: id.clone(),
            name: None,
            description: None,
            context_length: None,
            max_output_tokens: None,
        });
        if let Some(name) = &settings.name {
            model.name = Some(name.clone());
        }
        if let Some(description) = &settings.description {
            model.description = Some(description.clone());
        }
        if let Some(context_length) = settings.context_length {
            model.context_length = Some(context_length);
        }
        if let Some(max_output_tokens) = settings.max_output_tokens {
            model.max_output_tokens = Some(max_output_tokens);
        }
    }
    models.into_values().collect()
}
