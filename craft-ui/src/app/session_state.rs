use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use craft_agent::TurnType;
use craft_agent::permissions::PermissionManager;
use craft_config::{Effect, ModelPolicy};
use craft_providers::provider::adjust_model;
use craft_providers::{Model, ThinkingConfig, Timeouts, TokenUsage, settle_session};
use craft_storage::StateDir;
use craft_storage::sessions::{SessionMeta, StoredEffect, StoredMode, StoredRule};

use crate::AppSession;

use super::mode::{FlowState, Mode, PlanState};

pub(crate) struct SessionState {
    /// Shared with the writer thread, so a checkpoint is just a refcount bump.
    pub session: Arc<AppSession>,
    pub model: Model,
    pub token_usage: TokenUsage,
    pub cost: Option<f64>,
    pub context_size: u32,
    pub mode: Mode,
    pub plan: PlanState,
    pub flow: FlowState,
    pub warnings: Vec<String>,
    pub thinking: ThinkingConfig,
    pub fast: bool,
    pub context_window_overrides: HashMap<String, u32>,
}

const PLAN_FILE_MISSING_WARNING: &str = "Plan file was deleted \u{2014} started a new plan";

impl SessionState {
    pub fn from_session(
        mut session: AppSession,
        fallback_model: &Model,
        storage: &StateDir,
        model_policy: &ModelPolicy,
    ) -> Self {
        let mut model = model_policy
            .allows(&session.model)
            .then(|| Model::from_spec(&session.model))
            .transpose()
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                session.model = fallback_model.spec();
                fallback_model.clone()
            });
        // Apply the provider's per-model adjustments (e.g. Aperture's
        // routed-provider inheritance) so a resumed session matches one
        // started fresh.
        if let Err(e) = adjust_model(&mut model, Timeouts::default()) {
            tracing::warn!(model = %model.id, error = %e, "failed to adjust resumed model");
        }

        let mode = match session.meta.mode {
            Some(StoredMode::Plan) => Mode::Plan,
            Some(StoredMode::Flow) => Mode::Flow,
            _ => Mode::Build,
        };

        let mut warnings = Vec::new();

        let mut plan = match &session.meta.plan_path {
            Some(p) if Path::new(p).exists() => {
                if session.meta.plan_written {
                    PlanState::Ready(PathBuf::from(p))
                } else {
                    PlanState::Drafting(PathBuf::from(p))
                }
            }
            Some(_) => {
                warnings.push(PLAN_FILE_MISSING_WARNING.into());
                PlanState::None
            }
            None => PlanState::None,
        };

        if mode == Mode::Plan {
            plan.allocate_path(storage);
        }

        let flow = FlowState {
            workstream_id: session.meta.flow_workstream_id.clone().unwrap_or_default(),
            stage: session
                .meta
                .flow_stage
                .as_deref()
                .and_then(TurnType::parse)
                .or(Some(TurnType::General)),
        };
        let flow = if mode == Mode::Flow && flow.workstream_id.is_empty() {
            FlowState {
                workstream_id: super::mode::new_workstream_id(),
                ..flow
            }
        } else {
            flow
        };

        let token_usage = session.token_usage;
        let context_size = session.meta.context_size;
        let context_window_overrides = session.meta.context_window_overrides.clone();
        let fast = session.meta.fast && model.supports_fast();
        let cost = settle_session(&token_usage, session.usage_by_model_mut(), &model, fast);

        Self {
            thinking: session
                .meta
                .thinking
                .map(Into::into)
                .filter(|_| model.supports_thinking())
                .unwrap_or_default(),
            fast,
            session: Arc::new(session),
            model,
            token_usage,
            cost,
            context_size,
            mode,
            plan,
            flow,
            warnings,
            context_window_overrides,
        }
    }

    pub fn session_mut(&mut self) -> &mut AppSession {
        Arc::make_mut(&mut self.session)
    }

    /// Everything the session mirrors from live state, built field by field so
    /// a new `SessionMeta` field forces a decision here. Called every frame,
    /// so it stays cheap: an idle UI has an empty draft, queue and rule list,
    /// and an empty `Vec` does not allocate.
    pub fn build_meta(&self, permissions: &PermissionManager) -> SessionMeta {
        SessionMeta {
            schema_version: self.session.meta.schema_version,
            mode: Some(self.mode.into()),
            plan_path: self.plan.path().map(|p| p.to_string_lossy().into_owned()),
            plan_written: self.plan.is_ready(),
            session_rules: rules_to_stored(&permissions.session_rules_snapshot()),
            context_size: self.context_size,
            input_draft: None,
            queued_messages: Vec::new(),
            thinking: Some(self.thinking.into()),
            fast: self.fast,
            goal: self.session.meta.goal.clone(),
            goal_criteria: self.session.meta.goal_criteria.clone(),
            flow_workstream_id: (self.mode == Mode::Flow && !self.flow.workstream_id.is_empty())
                .then(|| self.flow.workstream_id.clone()),
            flow_stage: self.flow.stage.map(|s| s.as_str().to_string()),
            context_window_overrides: self.context_window_overrides.clone(),
            yolo: permissions.persisted_yolo(),
        }
    }

    pub fn update_model(&mut self, model: &Model) {
        if !model.supports_thinking() {
            self.thinking = ThinkingConfig::Off;
        }
        if !model.supports_fast() {
            self.fast = false;
        }
        self.session_mut().set_model(model.spec());
        self.model = model.clone();
    }
}

/// Apply a stored context-window override to `model` for its spec, clamped by
/// the floor guard. Returns true when the model's `context_window` changed.
/// `reserve_tokens` and `compaction_buffer` come from the resolved agent
/// config so the UI does not duplicate that lookup.
pub(crate) fn apply_context_window_override(
    model: &mut Model,
    overrides: &HashMap<String, u32>,
    reserve_tokens: u32,
    compaction_buffer: u32,
) -> bool {
    let Some(&requested) = overrides.get(&model.spec()) else {
        return false;
    };
    let effective =
        craft_config::effective_context_window(requested, reserve_tokens, compaction_buffer);
    if model.context_window == effective {
        return false;
    }
    model.context_window = effective;
    true
}

impl From<Mode> for StoredMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Build => StoredMode::Build,
            Mode::Plan => StoredMode::Plan,
            Mode::Flow => StoredMode::Flow,
        }
    }
}

pub(crate) fn rules_to_stored(rules: &[craft_config::PermissionRule]) -> Vec<StoredRule> {
    rules
        .iter()
        .map(|r| {
            let effect = match r.effect {
                Effect::Allow => StoredEffect::Allow,
                Effect::Deny => StoredEffect::Deny,
            };
            StoredRule {
                tool: r.tool.to_string(),
                scope: r.scope.clone(),
                effect,
            }
        })
        .collect()
}

/// Migrate old stored tool key formats to `ToolKey`.
/// Handles `"mcp:server__tool"` (pre-PR1 format) -> `McpTool`.
/// All other formats go through `ToolKey::parse` (current format: `server.tool`).
fn migrate_stored_tool_key(s: &str) -> Option<craft_config::ToolKey> {
    if let Some(rest) = s.strip_prefix("mcp:")
        && let Some((server, tool)) = rest.split_once("__")
    {
        let new_form = format!("{server}.{tool}");
        return craft_config::ToolKey::parse(&new_form)
            .map_err(
                |e| tracing::warn!(key = s, error = %e, "malformed stored tool key — skipping"),
            )
            .ok();
    }
    match craft_config::ToolKey::parse(s) {
        Ok(key) => Some(key),
        Err(e) => {
            tracing::error!(key = s, error = %e, "malformed stored tool key — rule DROPPED; a deny rule may have been lost");
            None
        }
    }
}

pub(crate) fn stored_to_rules(stored: &[StoredRule]) -> Vec<craft_config::PermissionRule> {
    stored
        .iter()
        .filter_map(|r| {
            let tool = match migrate_stored_tool_key(&r.tool) {
                Some(t) => t,
                None => {
                    if matches!(r.effect, StoredEffect::Deny) {
                        tracing::error!(
                            key = %r.tool,
                            "SECURITY: stored DENY rule dropped — tool may now be accessible. \
                             Re-add this rule manually in permissions.toml"
                        );
                    }
                    return None;
                }
            };
            let effect = match r.effect {
                StoredEffect::Allow => Effect::Allow,
                StoredEffect::Deny => Effect::Deny,
            };
            Some(craft_config::PermissionRule {
                tool,
                scope: r.scope.clone(),
                effect,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{test_model, test_pricing};
    use craft_providers::{FastPricing, ModelPricing};
    use craft_storage::sessions::StoredThinking;
    use test_case::test_case;

    const RECORDED_COST: f64 = 0.42;
    /// A round million, so a per-million rate reads straight off the bill.
    const MILLION_INPUT: TokenUsage = TokenUsage {
        input: 1_000_000,
        output: 0,
        cache_creation: 0,
        cache_read: 0,
    };
    /// [`MILLION_INPUT`] at `test_pricing`'s standard input rate.
    const LIST_PRICE: f64 = 3.0;
    /// Twice the standard rate, so a resume that ignores `fast` bills half.
    const FAST_INPUT_RATE: f64 = 6.0;
    const UNRESOLVABLE_MODEL: &str = "a-model-no-table-has-ever-heard-of";
    const FAST_FLAG_LOST: &str = "the model has fast pricing, so the flag must survive as stored";

    fn resumed(session: AppSession, model: &Model) -> SessionState {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        SessionState::from_session(session, model, &storage, &ModelPolicy::default())
    }

    /// An old session: counters, no per-model breakdown.
    fn session_with_counters() -> AppSession {
        let mut session = AppSession::new("test-model", "/tmp");
        session.token_usage = MILLION_INPUT;
        session
    }

    /// A resumed session opens on the bill it ran up, not on its counters
    /// re-priced with whatever model is selected now. The model that recorded
    /// this one prices to nothing, so only the recorded cost can answer.
    #[test]
    fn resumed_session_opens_on_the_cost_its_turns_recorded() {
        let mut session = session_with_counters();
        session.add_model_usage(
            UNRESOLVABLE_MODEL,
            session.token_usage.billed(Some(RECORDED_COST)),
        );
        let state = resumed(session, &test_model());
        assert_eq!(state.cost, Some(RECORDED_COST));
    }

    /// Older sessions kept counters only, and those are priced with the
    /// session's own clamped `fast` flag. A hardcoded `false` would open a
    /// resumed fast session on half its bill.
    #[test_case(false => Some(LIST_PRICE)     ; "standard_rates")]
    #[test_case(true  => Some(FAST_INPUT_RATE) ; "fast_rates")]
    fn resume_without_a_breakdown_prices_the_counters(fast: bool) -> Option<f64> {
        let mut session = session_with_counters();
        session.meta.fast = fast;
        let model = Model {
            pricing: ModelPricing {
                fast: Some(FastPricing {
                    input: FAST_INPUT_RATE,
                    output: test_pricing().output,
                }),
                ..test_pricing()
            },
            ..test_model()
        };

        let state = resumed(session, &model);

        assert_eq!(state.fast, fast, "{FAST_FLAG_LOST}");
        state.cost
    }

    fn make_plan_session(mode: Option<StoredMode>, plan_path: Option<String>) -> AppSession {
        let mut session = AppSession::new("test-model", "/tmp");
        session.meta.mode = mode;
        session.meta.plan_path = plan_path;
        session
    }

    #[test]
    fn plan_mode_without_path_allocates_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session = make_plan_session(Some(StoredMode::Plan), None);
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert_eq!(state.mode, Mode::Plan);
        assert!(state.plan.path().is_some(), "plan path should be allocated");
    }

    #[test]
    fn plan_mode_with_missing_file_allocates_new_path_and_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session =
            make_plan_session(Some(StoredMode::Plan), Some("/nonexistent/plan.md".into()));
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert_eq!(state.mode, Mode::Plan);
        let path = state.plan.path().expect("plan path should be allocated");
        assert_ne!(path, Path::new("/nonexistent/plan.md"));
        assert_eq!(state.warnings.len(), 1);
        assert_eq!(state.warnings[0], PLAN_FILE_MISSING_WARNING);
    }

    #[test]
    fn plan_mode_with_existing_file_preserves_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let plan_file = tmp.path().join("existing-plan.md");
        std::fs::write(&plan_file, "# Plan").unwrap();
        let session = make_plan_session(
            Some(StoredMode::Plan),
            Some(plan_file.to_string_lossy().into_owned()),
        );
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert_eq!(state.mode, Mode::Plan);
        assert_eq!(state.plan.path(), Some(plan_file.as_path()));
    }

    #[test]
    fn build_mode_does_not_allocate_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session = make_plan_session(Some(StoredMode::Build), None);
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert_eq!(state.mode, Mode::Build);
        assert!(state.plan.path().is_none());
    }

    #[test]
    fn disallowed_restored_model_uses_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let fallback = test_model();
        let mut session = make_plan_session(Some(StoredMode::Build), None);
        session.model = "openai/gpt-5".into();
        let raw: craft_config::RawConfig = serde_json::from_value(serde_json::json!({
            "provider": {"allowed_models": [fallback.spec()]}
        }))
        .unwrap();
        let policy = raw.into_config(&[]).unwrap().provider.model_policy;

        let state = SessionState::from_session(session, &fallback, &storage, &policy);

        assert_eq!(state.model.spec(), fallback.spec());
        assert_eq!(state.session.model, fallback.spec());
    }

    #[test]
    fn from_session_loads_context_window_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut session = AppSession::new("test-model", "/tmp");
        session
            .meta
            .context_window_overrides
            .insert("test-model".into(), 150_000);
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert_eq!(
            state.context_window_overrides.get("test-model"),
            Some(&150_000)
        );
    }

    #[test]
    fn build_meta_writes_back_context_window_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session = AppSession::new("test-model", "/tmp");
        let mut state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        state
            .context_window_overrides
            .insert("test-model".into(), 180_000);
        let permissions = PermissionManager::new(
            craft_config::PermissionsConfig::default(),
            std::path::PathBuf::from("/tmp"),
            Arc::default(),
        );
        state.session_mut().meta = state.build_meta(&permissions);
        assert_eq!(
            state
                .session
                .meta
                .context_window_overrides
                .get("test-model"),
            Some(&180_000)
        );
    }

    #[test]
    fn apply_context_window_override_sets_window_for_matching_spec() {
        let mut model = test_model();
        let mut overrides = HashMap::new();
        overrides.insert("anthropic/test-model".into(), 120_000);
        assert!(apply_context_window_override(
            &mut model, &overrides, 4_096, 32_768
        ));
        assert_eq!(model.context_window, 120_000);
    }

    #[test]
    fn apply_context_window_override_clamps_below_floor() {
        let mut model = test_model();
        let mut overrides = HashMap::new();
        overrides.insert("anthropic/test-model".into(), 10_000);
        assert!(apply_context_window_override(
            &mut model, &overrides, 32_768, 4_096
        ));
        assert_eq!(model.context_window, 36_864);
    }

    #[test]
    fn apply_context_window_override_noop_without_entry() {
        let mut model = test_model();
        let overrides = HashMap::new();
        assert!(!apply_context_window_override(
            &mut model, &overrides, 4_096, 32_768
        ));
        assert_eq!(model.context_window, crate::components::TEST_CONTEXT_WINDOW);
    }

    #[test]
    fn from_session_applies_provider_adjust_model() {
        // SAFETY: this test runs single-threaded; no other thread reads the env.
        unsafe { std::env::set_var("APERTURE_HOST", "https://example.com") };
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut session = AppSession::new("aperture/mistral/glm-5-2", "/tmp");
        session.meta.thinking = Some(StoredThinking::Adaptive);
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert!(
            state.model.supports_thinking(),
            "resumed aperture/mistral/glm-5-2 should inherit thinking support from adjust_model",
        );
        assert_eq!(
            state.thinking,
            ThinkingConfig::Adaptive,
            "resumed thinking config should be preserved when the model supports it",
        );
    }
}
