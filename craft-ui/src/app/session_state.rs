use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use craft_agent::TurnType;
use craft_agent::permissions::PermissionManager;
use craft_config::{Effect, ModelPolicy};
use craft_providers::{Model, ThinkingConfig, TokenUsage};
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
        let model = model_policy
            .allows(&session.model)
            .then(|| Model::from_spec(&session.model))
            .transpose()
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                session.model = fallback_model.spec();
                fallback_model.clone()
            });

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
        let cost = model.cost_of(&token_usage, fast);

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
    use crate::components::test_model;

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
        let policy = raw.into_config(false).unwrap().provider.model_policy;

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
}
