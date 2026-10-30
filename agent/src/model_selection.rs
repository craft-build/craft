//! Model selection: one shared policy for resolving a model against the
//! discovered provider catalogs, used by both the TUI provider and the ACP
//! server so the two surfaces cannot drift.

use std::collections::BTreeMap;

use animist::provider::ModelInfo;

/// One row of a provider's model listing as the pickers and session
/// options see it: an animist [`ModelInfo`] projected with the display
/// fields the config's manual table and the models.dev catalog carry
/// (`ModelInfo` itself holds neither name nor description).
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogModel {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub context_length: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

impl CatalogModel {
    /// Project a listing row, attaching the manual table's display fields.
    pub fn from_info(info: &ModelInfo, name: Option<String>, description: Option<String>) -> Self {
        Self {
            id: info.ids.first().cloned().unwrap_or_default(),
            name,
            description,
            context_length: info.config.context_window,
            max_output_tokens: info.config.max_output_tokens,
        }
    }

    /// Project a full listing, attaching the config's manual display
    /// fields by exact id. A session whose provider left the config
    /// projects bare rows.
    pub fn rows(infos: &[ModelInfo], config: Option<&crate::config::ProviderConfig>) -> Vec<Self> {
        infos
            .iter()
            .map(|info| {
                let manual = info
                    .ids
                    .first()
                    .and_then(|id| config.and_then(|c| c.models().get(id)));
                Self::from_info(
                    info,
                    manual.and_then(|m| m.name.clone()),
                    manual.and_then(|m| m.description.clone()),
                )
            })
            .collect()
    }

    /// Human label for pickers.
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.id.clone())
    }
}

/// A resolved model: its catalog id and the context window the catalog
/// reports for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub model: String,
    pub context_length: Option<u32>,
}

/// Resolve a persisted `provider/model` spec (from `--model`, a tier
/// default, or a prior session's header) against the discovered catalogs.
/// Strict: `None` when the provider is gone or the model is no longer
/// listed — callers use the miss to re-prompt instead of silently serving
/// a different model.
pub fn resolve_spec(
    catalogs: &BTreeMap<String, Vec<ModelInfo>>,
    spec: &str,
) -> Option<(String, ResolvedModel)> {
    let (provider, model) = spec.split_once('/')?;
    let entry = catalogs
        .get(provider)?
        .iter()
        .find(|info| info.ids.first().is_some_and(|id| id == model))?;
    Some((
        provider.to_string(),
        ResolvedModel {
            model: entry.ids.first()?.clone(),
            context_length: entry.config.context_window,
        },
    ))
}

/// The model a session should serve: the requested id when the catalog
/// still lists it, otherwise the catalog's first entry. The ACP protocol
/// needs a concrete model at all times, so an absent model falls back
/// rather than fails.
pub fn resolve_or_default(models: &[ModelInfo], requested: Option<&str>) -> Option<ResolvedModel> {
    let entry = requested
        .and_then(|id| {
            models
                .iter()
                .find(|info| info.ids.first().is_some_and(|listed| listed == id))
        })
        .or_else(|| models.first())?;
    Some(ResolvedModel {
        model: entry.ids.first()?.clone(),
        context_length: entry.config.context_window,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, ctx: Option<u32>) -> ModelInfo {
        let mut info = ModelInfo::default();
        info.ids = vec![id.to_string()];
        info.config.context_window = ctx;
        info
    }

    fn catalogs() -> BTreeMap<String, Vec<ModelInfo>> {
        BTreeMap::from([(
            "acme".to_string(),
            vec![model("one", Some(8)), model("two", None)],
        )])
    }

    #[test]
    fn resolve_spec_is_strict_about_both_halves() {
        let catalogs = catalogs();
        let (provider, resolved) = resolve_spec(&catalogs, "acme/two").unwrap();
        assert_eq!(provider, "acme");
        assert_eq!(resolved.model, "two");
        assert_eq!(resolved.context_length, None);
        assert!(resolve_spec(&catalogs, "acme/gone").is_none());
        assert!(resolve_spec(&catalogs, "gone/one").is_none());
        assert!(resolve_spec(&catalogs, "no-slash").is_none());
    }

    #[test]
    fn resolve_or_default_falls_back_to_first() {
        let models = catalogs()["acme"].clone();
        let resolved = resolve_or_default(&models, Some("two")).unwrap();
        assert_eq!(resolved.model, "two");
        // An absent requested id falls back to the catalog's first entry,
        // keeping its context window.
        let resolved = resolve_or_default(&models, Some("gone")).unwrap();
        assert_eq!(resolved.model, "one");
        assert_eq!(resolved.context_length, Some(8));
        assert_eq!(resolve_or_default(&models, None).unwrap().model, "one");
        assert!(resolve_or_default(&[], None).is_none());
    }
}
