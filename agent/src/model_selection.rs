//! Model selection: one shared policy for resolving a model against the
//! discovered provider catalogs, used by both the TUI provider and the ACP
//! server so the two surfaces cannot drift.

use std::collections::BTreeMap;

use crate::providers::CatalogModel;

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
    catalogs: &BTreeMap<String, Vec<CatalogModel>>,
    spec: &str,
) -> Option<(String, ResolvedModel)> {
    let (provider, model) = spec.split_once('/')?;
    let entry = catalogs.get(provider)?.iter().find(|m| m.id == model)?;
    Some((
        provider.to_string(),
        ResolvedModel {
            model: entry.id.clone(),
            context_length: entry.context_length,
        },
    ))
}

/// The model a session should serve: the requested id when the catalog
/// still lists it, otherwise the catalog's first entry. The ACP protocol
/// needs a concrete model at all times, so an absent model falls back
/// rather than fails.
pub fn resolve_or_default(
    models: &[CatalogModel],
    requested: Option<&str>,
) -> Option<ResolvedModel> {
    let entry = requested
        .and_then(|id| models.iter().find(|m| m.id == id))
        .or_else(|| models.first())?;
    Some(ResolvedModel {
        model: entry.id.clone(),
        context_length: entry.context_length,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, ctx: Option<u32>) -> CatalogModel {
        CatalogModel {
            id: id.to_string(),
            name: None,
            description: None,
            context_length: ctx,
            max_output_tokens: None,
        }
    }

    fn catalogs() -> BTreeMap<String, Vec<CatalogModel>> {
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
