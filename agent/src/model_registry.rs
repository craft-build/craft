//! Per-model tier assignments (weak / medium / strong / compaction).
//!
//! Ported from the reference `craft-providers/src/model_registry.rs`, adapted
//! to this repo's Rig-based architecture: the reference's per-provider
//! `manifest.rs` layer is replaced by a compact static table keyed by
//! `ProviderKind` string, and the models.dev catalog (task 49) carries no
//! tier metadata — verified against the reference, whose catalog fills only
//! context/pricing/vision — so `ModelInfo::tier` is set by dynamic-provider
//! scripts there and stays `None` here; positional auto-assignment covers
//! the gap.
//!
//! Three layers, checked in order: user overrides (persisted, a model may hold
//! several tiers) > static entries from the provider manifest > auto-assignment
//! by position in the discovered model list.
//!
//! The global lock never escapes this module: accessors lock internally and
//! return owned data, so a caller can never hold a read guard across model
//! construction. The module owns persistence: [`load_from_storage`] at
//! startup, [`set_and_persist`] on user edits. Callers never touch the
//! on-disk format directly.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde::{Deserialize, Serialize};

use crate::storage::{StateDir, atomic_write};

const TIERS_FILE: &str = "model-tiers";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    Weak,
    Medium,
    Strong,
    Compaction,
}

impl std::fmt::Display for ModelTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Weak => "weak",
            Self::Medium => "medium",
            Self::Strong => "strong",
            Self::Compaction => "compaction",
        })
    }
}

/// A discovered model, from the provider's model listing. Not persisted —
/// rebuilt every session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub context_window: Option<u32>,
    /// Tier metadata, when a source that knows one supplies it. The models.dev
    /// catalog has none (matching the reference), so in practice this is
    /// `None` and positional auto-assignment applies.
    pub tier: Option<ModelTier>,
}

impl ModelInfo {
    pub fn new(id: String) -> Self {
        Self {
            id,
            context_window: None,
            tier: None,
        }
    }
}

/// Static (curated) tier entry: every model ID starting with one of these
/// prefixes maps to `tier`.
struct StaticModel {
    prefixes: &'static [&'static str],
    tier: ModelTier,
    default: bool,
}

/// Minimal replacement for the reference `manifest.rs` layer, keyed by
/// `ProviderKind::as_str()`. Providers absent from this table are treated as
/// accepting arbitrary models (reference behavior for unknown slugs).
struct ProviderManifest {
    accepts_arbitrary_models: bool,
    models: &'static [StaticModel],
}

static ANTHROPIC: &[StaticModel] = &[
    StaticModel {
        prefixes: &["claude-haiku-4-5"],
        tier: ModelTier::Weak,
        default: true,
    },
    StaticModel {
        prefixes: &["claude-sonnet-5"],
        tier: ModelTier::Medium,
        default: true,
    },
    StaticModel {
        prefixes: &["claude-opus-4-8", "claude-fable-5"],
        tier: ModelTier::Strong,
        default: false,
    },
    StaticModel {
        prefixes: &["claude-opus-5"],
        tier: ModelTier::Strong,
        default: true,
    },
];

static OPENAI: &[StaticModel] = &[
    StaticModel {
        prefixes: &["gpt-5.6-luna"],
        tier: ModelTier::Weak,
        default: true,
    },
    StaticModel {
        prefixes: &["gpt-5.6-terra"],
        tier: ModelTier::Medium,
        default: true,
    },
    StaticModel {
        prefixes: &["gpt-5.6-sol"],
        tier: ModelTier::Strong,
        default: true,
    },
    StaticModel {
        prefixes: &["gpt-6-astra"],
        tier: ModelTier::Strong,
        default: false,
    },
    StaticModel {
        prefixes: &["gpt-5.4-nano", "gpt-5.4-mini", "gpt-4.1-nano"],
        tier: ModelTier::Weak,
        default: false,
    },
];

static COPILOT: &[StaticModel] = &[
    StaticModel {
        prefixes: &[
            "gpt-5-mini",
            "gpt-5.4-mini",
            "gpt-5.4-nano",
            "claude-haiku-4.5",
        ],
        tier: ModelTier::Weak,
        default: false,
    },
    StaticModel {
        prefixes: &["claude-sonnet-4.5", "claude-sonnet-4.6", "claude-sonnet-5"],
        tier: ModelTier::Medium,
        default: false,
    },
    StaticModel {
        prefixes: &[
            "claude-opus-5",
            "claude-opus-4.8",
            "claude-opus-4.7",
            "claude-opus-4.6",
            "claude-opus-4.5",
        ],
        tier: ModelTier::Strong,
        default: true,
    },
];

fn manifest(kind: &str) -> Option<ProviderManifest> {
    let (accepts_arbitrary_models, models) = match kind {
        "anthropic" => (false, ANTHROPIC),
        "openai" | "deepseek" => (false, OPENAI),
        "copilot" => (true, COPILOT),
        _ => return None,
    };
    Some(ProviderManifest {
        accepts_arbitrary_models,
        models,
    })
}

static REGISTRY: OnceLock<RwLock<ModelRegistry>> = OnceLock::new();

fn read() -> RwLockReadGuard<'static, ModelRegistry> {
    registry().read().unwrap()
}

fn write() -> RwLockWriteGuard<'static, ModelRegistry> {
    registry().write().unwrap()
}

fn registry() -> &'static RwLock<ModelRegistry> {
    REGISTRY.get_or_init(|| RwLock::new(ModelRegistry::default()))
}

pub fn spec_for_tier(provider: &str, tier: ModelTier) -> Option<String> {
    read().spec_for_tier(provider, tier)
}

pub fn spec_for_tier_any(tier: ModelTier) -> Option<String> {
    read().spec_for_tier_any(tier)
}

pub fn discovered(provider: &str, model_id: &str) -> Option<ModelInfo> {
    read().discovered(provider, model_id).cloned()
}

pub fn tier_for(spec: &str, provider: &str, static_tier: Option<ModelTier>) -> ModelTier {
    read().tier_for(spec, provider, static_tier)
}

/// Register a provider's discovered models. `kind` is the `ProviderKind`
/// string (this repo keys catalogs by user-chosen config name; the manifest
/// layer needs the kind to decide curated vs arbitrary).
pub fn set_known_models(provider: &str, kind: &str, models: Vec<ModelInfo>) {
    write().set_known_models(provider, kind, models);
}

/// Tiers whose override points at `spec`, in descending tier order.
pub fn override_tiers(spec: &str) -> Vec<ModelTier> {
    read().override_tiers(spec)
}

pub fn load_from_storage(dir: &StateDir) {
    let overrides = read_overrides(dir.path().join(TIERS_FILE).as_path());
    write().set_overrides(overrides);
}

pub fn set_and_persist(spec: String, tier: ModelTier, dir: &StateDir) {
    update_and_persist(dir, |reg| reg.set(spec, tier));
}

pub fn unset_and_persist(spec: &str, tier: ModelTier, dir: &StateDir) {
    update_and_persist(dir, |reg| reg.unset(spec, tier));
}

/// Snapshot under the lock, persist outside it: file IO must never run while
/// holding the registry lock.
fn update_and_persist(dir: &StateDir, update: impl FnOnce(&mut ModelRegistry)) {
    let snapshot = {
        let mut reg = write();
        update(&mut reg);
        reg.overrides.clone()
    };
    write_overrides(dir.path().join(TIERS_FILE).as_path(), &snapshot);
}

#[derive(Default)]
struct ModelRegistry {
    /// Keyed by tier (not spec) so each tier has at most one holder. A model
    /// may legitimately hold several tiers. Persisted to disk.
    overrides: BTreeMap<ModelTier, String>,
    /// Ordered model info per provider (config name), populated from catalog
    /// discovery. Not persisted - rebuilt every session. Used for auto-tier
    /// assignment and discovered metadata lookup.
    known_models: HashMap<String, Vec<ModelInfo>>,
    /// Config name → provider kind string, for manifest lookups.
    kinds: HashMap<String, String>,
}

impl ModelRegistry {
    fn set_overrides(&mut self, overrides: BTreeMap<ModelTier, String>) {
        self.overrides = overrides;
    }

    fn set_known_models(&mut self, provider: &str, kind: &str, models: Vec<ModelInfo>) {
        self.kinds.insert(provider.to_string(), kind.to_string());
        self.known_models.insert(provider.to_string(), models);
    }

    fn set(&mut self, spec: String, tier: ModelTier) {
        self.overrides.insert(tier, spec);
    }

    fn unset(&mut self, spec: &str, tier: ModelTier) {
        if self.has_override(spec, tier) {
            self.overrides.remove(&tier);
        }
    }

    fn has_override(&self, spec: &str, tier: ModelTier) -> bool {
        self.overrides.get(&tier).map(String::as_str) == Some(spec)
    }

    /// Lookup discovered metadata for a model by ID.
    fn discovered(&self, provider: &str, model_id: &str) -> Option<&ModelInfo> {
        self.known_models
            .get(provider)?
            .iter()
            .find(|m| m.id == model_id)
    }

    fn kind_of<'a>(&'a self, provider: &'a str) -> &'a str {
        self.kinds
            .get(provider)
            .map(String::as_str)
            .unwrap_or(provider)
    }

    fn tier_for(&self, spec: &str, provider: &str, static_tier: Option<ModelTier>) -> ModelTier {
        let mut tiers = self.override_tiers(spec).into_iter();
        if let Some(first) = tiers.next() {
            return match first {
                ModelTier::Compaction => tiers.next().unwrap_or(first),
                t => t,
            };
        }
        if self.tiers_from_discovery(provider)
            && let Some((_, model_id)) = spec.split_once('/')
            && let Some(models) = self.known_models.get(provider)
            && let Some(pos) = models.iter().position(|model| model.id == model_id)
        {
            if let Some(tier) = models[pos].tier {
                return tier;
            }
            if static_tier.is_none() {
                return tier_for_position(pos);
            }
        }
        if let Some(t) = static_tier {
            return t;
        }
        ModelTier::Medium
    }

    fn spec_for_tier(&self, provider: &str, tier: ModelTier) -> Option<String> {
        let prefix = format!("{provider}/");
        if let Some(spec) = self.overrides.get(&tier)
            && spec.starts_with(&prefix)
        {
            return Some(spec.clone());
        }

        let candidate = if self.tiers_from_discovery(provider) {
            self.discovered_static_candidate(provider, tier)
                .or_else(|| self.metadata_candidate(provider, tier))
                .or_else(|| self.static_candidate(provider, tier))
                .or_else(|| self.positional_candidate(provider, tier))
        } else {
            self.static_candidate(provider, tier)
        }?;

        (!self.claimed_elsewhere(&candidate, tier)).then_some(candidate)
    }

    /// The curated default the provider actually offers, so discovery cannot
    /// replace a still available default with whatever it lists first.
    fn discovered_static_candidate(&self, provider: &str, tier: ModelTier) -> Option<String> {
        self.static_prefixes(provider, tier)
            .find(|prefix| self.discovered(provider, prefix).is_some())
            .map(|prefix| format!("{provider}/{prefix}"))
    }

    /// Lowest ID wins, so the tier default survives provider list reordering.
    fn metadata_candidate(&self, provider: &str, tier: ModelTier) -> Option<String> {
        self.known_models
            .get(provider)?
            .iter()
            .filter(|model| model.tier == Some(tier))
            .map(|model| model.id.as_str())
            .min()
            .map(|id| format!("{provider}/{id}"))
    }

    fn positional_candidate(&self, provider: &str, tier: ModelTier) -> Option<String> {
        let models = self.known_models.get(provider).filter(|m| !m.is_empty())?;
        let slot = match tier {
            ModelTier::Strong => 0,
            ModelTier::Medium => 1,
            ModelTier::Weak => 2,
            ModelTier::Compaction => return None,
        };
        Some(format!(
            "{provider}/{}",
            models[slot.min(models.len() - 1)].id
        ))
    }

    fn claimed_elsewhere(&self, spec: &str, tier: ModelTier) -> bool {
        self.overrides.iter().any(|(&t, s)| s == spec && t != tier)
    }

    fn spec_for_tier_any(&self, tier: ModelTier) -> Option<String> {
        if let Some(spec) = self.overrides.get(&tier) {
            return Some(spec.clone());
        }

        for provider in self.known_models.keys() {
            let models = &self.known_models[provider];
            if models.is_empty() {
                continue;
            }
            let want = match tier {
                ModelTier::Strong => 0,
                ModelTier::Medium => 1,
                ModelTier::Weak => 2,
                ModelTier::Compaction => return None,
            };
            let idx = want.min(models.len() - 1);
            let spec = format!("{provider}/{}", models[idx].id);
            let overridden_elsewhere = self.overrides.iter().any(|(&t, s)| s == &spec && t != tier);
            if !overridden_elsewhere {
                return Some(spec);
            }
        }
        None
    }

    fn override_tiers(&self, spec: &str) -> Vec<ModelTier> {
        self.overrides
            .iter()
            .rev()
            .filter(|(_, s)| s.as_str() == spec)
            .map(|(&t, _)| t)
            .collect()
    }

    /// Discovery metadata is stored for every provider, but only providers
    /// that accept arbitrary models may use the discovered list for tier
    /// auto-assignment; curated providers keep their static tier tables.
    fn tiers_from_discovery(&self, provider: &str) -> bool {
        manifest(self.kind_of(provider)).is_none_or(|m| m.accepts_arbitrary_models)
    }

    fn static_candidate(&self, provider: &str, tier: ModelTier) -> Option<String> {
        self.static_prefixes(provider, tier)
            .next()
            .map(|prefix| format!("{provider}/{prefix}"))
    }

    fn static_prefixes<'a>(
        &'a self,
        provider: &'a str,
        tier: ModelTier,
    ) -> impl Iterator<Item = &'static str> + use<'a> {
        manifest(self.kind_of(provider))
            .into_iter()
            .flat_map(|m| m.models.iter())
            .filter(move |entry| entry.default && entry.tier == tier)
            .flat_map(|entry| entry.prefixes.iter().copied())
    }
}

fn tier_for_position(pos: usize) -> ModelTier {
    [ModelTier::Strong, ModelTier::Medium, ModelTier::Weak][pos.min(2)]
}

// On-disk format: { "tier": "spec", ... } keyed by tier, matching the in-memory
// `BTreeMap<ModelTier, String>`. Tier-keyed storage preserves a model assigned
// to multiple tiers; a spec-keyed file would collapse them to a single entry.
// Legacy files were spec-keyed and are inverted on read.

fn read_overrides(path: &Path) -> BTreeMap<ModelTier, String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    if raw.trim().is_empty() {
        return BTreeMap::new();
    }
    if let Ok(map) = serde_json::from_str::<BTreeMap<ModelTier, String>>(&raw) {
        return map;
    }
    // Human-readable / legacy format: { "provider/model": "tier" } — invert on read.
    match serde_json::from_str::<BTreeMap<String, ModelTier>>(&raw) {
        Ok(legacy) => legacy.into_iter().map(|(s, t)| (t, s)).collect(),
        Err(e) => {
            eprintln!(
                "warning: failed to parse tier overrides at {}: {e}",
                path.display()
            );
            BTreeMap::new()
        }
    }
}

fn write_overrides(path: &Path, overrides: &BTreeMap<ModelTier, String>) {
    let json = match serde_json::to_vec_pretty(overrides) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("warning: failed to serialize tier overrides: {e}");
            return;
        }
    };
    if let Err(e) = atomic_write(path, &json) {
        eprintln!(
            "warning: failed to persist tier overrides at {}: {e}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use test_case::test_case;

    fn make_map(overrides: &[(ModelTier, &str)], models: &[&str]) -> ModelRegistry {
        let mut reg = ModelRegistry::default();
        reg.set_overrides(overrides.iter().map(|(t, s)| (*t, s.to_string())).collect());
        if !models.is_empty() {
            reg.set_known_models(
                "ollama",
                "ollama",
                models
                    .iter()
                    .map(|s| ModelInfo::new(s.to_string()))
                    .collect(),
            );
        }
        reg
    }

    #[test]
    fn tier_for_resolution_priority() {
        let mut reg = make_map(&[], &["pos0", "pos1", "pos2"]);
        reg.set("ollama/pos1".into(), ModelTier::Weak);

        let t = |spec, static_tier| reg.tier_for(spec, "ollama", static_tier);

        assert_eq!(t("ollama/pos1", Some(ModelTier::Strong)), ModelTier::Weak);
        assert_eq!(t("ollama/pos0", Some(ModelTier::Weak)), ModelTier::Weak);
        assert_eq!(t("ollama/pos0", None), ModelTier::Strong);
        assert_eq!(t("ollama/pos1", None), ModelTier::Weak);
        assert_eq!(t("ollama/pos2", None), ModelTier::Weak);
        assert_eq!(t("ollama/unknown", None), ModelTier::Medium);
    }

    fn make_tiered(models: &[(&str, ModelTier)]) -> ModelRegistry {
        let mut reg = ModelRegistry::default();
        reg.set_known_models(
            "copilot",
            "copilot",
            models
                .iter()
                .map(|&(id, tier)| ModelInfo {
                    tier: Some(tier),
                    ..ModelInfo::new(id.to_string())
                })
                .collect(),
        );
        reg
    }

    #[test]
    fn discovered_category_tier_beats_position_and_static_fallback() {
        let reg = make_tiered(&[
            ("terra", ModelTier::Medium),
            ("luna", ModelTier::Weak),
            ("gpt-5.6-sol", ModelTier::Strong),
        ]);

        assert_eq!(
            reg.tier_for("copilot/gpt-5.6-sol", "copilot", Some(ModelTier::Medium)),
            ModelTier::Strong
        );
        assert_eq!(
            reg.tier_for("copilot/terra", "copilot", None),
            ModelTier::Medium
        );
        assert_eq!(
            reg.tier_for("copilot/luna", "copilot", None),
            ModelTier::Weak
        );
        assert_eq!(
            reg.spec_for_tier("copilot", ModelTier::Strong),
            Some("copilot/gpt-5.6-sol".into())
        );
    }

    #[test]
    fn curated_provider_ignores_discovered_tiers() {
        let mut reg = ModelRegistry::default();
        reg.set_known_models(
            "openai",
            "openai",
            vec![ModelInfo {
                tier: Some(ModelTier::Strong),
                ..ModelInfo::new("syn:discovered:strong".into())
            }],
        );

        assert_ne!(
            reg.tier_for("openai/syn:discovered:strong", "openai", None),
            ModelTier::Strong
        );
        assert_ne!(
            reg.spec_for_tier("openai", ModelTier::Strong),
            Some("openai/syn:discovered:strong".into())
        );
    }

    #[test_case(&[("gpt-5.4", ModelTier::Strong), ("claude-opus-4.7", ModelTier::Strong)], "copilot/claude-opus-4.7"; "curated default beats discovered tier")]
    #[test_case(&[("claude-opus-4.6", ModelTier::Strong), ("alpha", ModelTier::Strong)], "copilot/claude-opus-4.6"; "later curated prefix when first is unavailable")]
    #[test_case(&[("zeta", ModelTier::Strong), ("alpha", ModelTier::Strong)], "copilot/alpha"; "lowest id when no curated default is entitled")]
    fn spec_for_tier_prefers_entitled_curated_default(
        models: &[(&str, ModelTier)],
        expected: &str,
    ) {
        let reg = make_tiered(models);
        assert_eq!(
            reg.spec_for_tier("copilot", ModelTier::Strong),
            Some(expected.into())
        );
    }

    #[test]
    fn spec_for_tier_ignores_discovery_list_order() {
        let models = [
            ("zeta", ModelTier::Strong),
            ("alpha", ModelTier::Strong),
            ("mid", ModelTier::Medium),
        ];
        let mut reversed = models;
        reversed.reverse();

        assert_eq!(
            make_tiered(&models).spec_for_tier("copilot", ModelTier::Strong),
            make_tiered(&reversed).spec_for_tier("copilot", ModelTier::Strong)
        );
    }

    #[test]
    fn tier_for_prefers_strongest_over_multi_tier_spec() {
        let mut reg = make_map(&[], &[]);
        reg.set("ollama/multi".into(), ModelTier::Medium);
        reg.set("ollama/multi".into(), ModelTier::Strong);
        reg.set("ollama/multi".into(), ModelTier::Compaction);
        reg.set("ollama/compact-only".into(), ModelTier::Compaction);

        let t = |spec| reg.tier_for(spec, "ollama", None);

        assert_eq!(t("ollama/multi"), ModelTier::Strong);
        assert_eq!(t("ollama/compact-only"), ModelTier::Compaction);
    }

    #[test]
    fn spec_for_tier_resolution() {
        let reg = make_map(
            &[(ModelTier::Strong, "ollama/custom")],
            &["big", "mid", "small"],
        );
        let s = |t| reg.spec_for_tier("ollama", t);

        assert_eq!(s(ModelTier::Strong), Some("ollama/custom".into()));
        assert_eq!(s(ModelTier::Medium), Some("ollama/mid".into()));
        assert_eq!(s(ModelTier::Weak), Some("ollama/small".into()));

        let scoped = make_map(&[(ModelTier::Strong, "openai/gpt-foo")], &[]);
        assert_eq!(scoped.spec_for_tier("ollama", ModelTier::Strong), None);

        let conflict = make_map(&[(ModelTier::Weak, "ollama/big")], &["big", "mid", "small"]);
        assert_eq!(conflict.spec_for_tier("ollama", ModelTier::Strong), None);
    }

    #[test]
    fn spec_for_tier_any_no_models_returns_none() {
        let reg = make_map(&[], &[]);
        assert_eq!(reg.spec_for_tier_any(ModelTier::Strong), None);
    }

    #[test]
    fn spec_for_tier_any_cross_provider() {
        let reg = make_map(
            &[
                (ModelTier::Weak, "zai/glm-5"),
                (ModelTier::Strong, "openai/gpt-foo"),
            ],
            &["big", "mid", "small"],
        );
        assert_eq!(
            reg.spec_for_tier_any(ModelTier::Strong),
            Some("openai/gpt-foo".into())
        );
        assert_eq!(
            reg.spec_for_tier_any(ModelTier::Weak),
            Some("zai/glm-5".into())
        );
        assert_eq!(
            reg.spec_for_tier_any(ModelTier::Medium),
            Some("ollama/mid".into())
        );
    }

    #[test]
    fn discovered_looks_up_by_id() {
        let mut reg = ModelRegistry::default();
        let mut info_a = ModelInfo::new("model-a".into());
        info_a.context_window = Some(32_000);
        let mut info_b = ModelInfo::new("model-b".into());
        info_b.context_window = Some(128_000);
        reg.set_known_models("llamafile", "llamafile", vec![info_a, info_b]);
        let info = reg.discovered("llamafile", "model-a").unwrap();
        assert_eq!(info.id, "model-a");
        assert_eq!(info.context_window, Some(32_000));
        assert!(reg.discovered("llamafile", "model-x").is_none());
        assert!(reg.discovered("ollama", "model-a").is_none());
    }

    #[test]
    fn persistence_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(TIERS_FILE);

        assert!(read_overrides(&path).is_empty());

        let mut m = BTreeMap::new();
        m.insert(ModelTier::Strong, "ollama/qwen3".into());
        m.insert(ModelTier::Medium, "ollama/qwen3:8b".into());
        write_overrides(&path, &m);

        let loaded = read_overrides(&path);
        assert_eq!(loaded.get(&ModelTier::Strong).unwrap(), "ollama/qwen3");
        assert_eq!(loaded.get(&ModelTier::Medium).unwrap(), "ollama/qwen3:8b");
    }

    #[test]
    fn unset_removes_matching_override() {
        let mut reg = make_map(&[(ModelTier::Strong, "ollama/a")], &[]);
        reg.unset("ollama/a", ModelTier::Strong);
        assert!(!reg.has_override("ollama/a", ModelTier::Strong));
        assert!(reg.overrides.is_empty());
    }

    #[test]
    fn unset_ignores_mismatched_spec() {
        let mut reg = make_map(&[(ModelTier::Strong, "ollama/a")], &[]);
        reg.unset("ollama/b", ModelTier::Strong);
        assert!(reg.has_override("ollama/a", ModelTier::Strong));
    }

    #[test]
    fn unset_ignores_mismatched_tier() {
        let mut reg = make_map(&[(ModelTier::Strong, "ollama/a")], &[]);
        reg.unset("ollama/a", ModelTier::Weak);
        assert!(reg.has_override("ollama/a", ModelTier::Strong));
    }

    #[test]
    fn has_override_returns_false_for_no_override() {
        let reg = make_map(&[], &[]);
        assert!(!reg.has_override("ollama/a", ModelTier::Strong));
    }

    #[test]
    fn backwards_compat_reads_legacy_format() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(TIERS_FILE);
        let legacy = r#"{"ollama/a": "strong", "ollama/b": "strong", "ollama/c": "weak"}"#;
        std::fs::write(&path, legacy).unwrap();

        let loaded = read_overrides(&path);
        assert_eq!(loaded.get(&ModelTier::Strong).unwrap(), "ollama/b");
        assert_eq!(loaded.get(&ModelTier::Weak).unwrap(), "ollama/c");
    }

    #[test]
    fn write_then_read_preserves_multi_tier_assignment() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(TIERS_FILE);

        let mut m = BTreeMap::new();
        m.insert(ModelTier::Strong, "ollama/qwen3".into());
        m.insert(ModelTier::Medium, "ollama/qwen3".into());
        m.insert(ModelTier::Weak, "ollama/qwen3:8b".into());
        write_overrides(&path, &m);

        let loaded = read_overrides(&path);
        assert_eq!(loaded.get(&ModelTier::Strong).unwrap(), "ollama/qwen3");
        assert_eq!(loaded.get(&ModelTier::Medium).unwrap(), "ollama/qwen3");
        assert_eq!(loaded.get(&ModelTier::Weak).unwrap(), "ollama/qwen3:8b");
    }
}
