//! Request layer: the per-step request-view pipeline, in the order the old
//! loop applied it — recency tail, in-run read lifecycle and output
//! compression, protocol shaping, the output-token clamp, and the sampling
//! options. The system prompt rides here too, so its bytes stay exactly the
//! preamble the surfaces always built. Stored history is never touched:
//! every rewrite exists because the protocol rejects the shape animist
//! passes through, or covers results the current run produced (everything
//! older arrives pre-altered from history, 08).

use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use async_trait::async_trait;

use animist::agent::{Agent, LayerImpl, Result, RunContext, StepRequest};
use animist::history::StoredMessage;
use animist::provider::{RequestOptions, StreamResponse};

use crate::compaction::estimate::RequestOverhead;
use crate::compression::CompressionConfig;
use crate::provider::ModelHandle;
use crate::run::SharedCompactionState;
use crate::run::read_lifecycle;
use crate::run::recency::{self, RecencySource};
use crate::run::retry::hop_max_tokens;
use crate::run::view;
use crate::thinking::ThinkingConfig;

use super::conversation::TurnSettings;
use super::recovery::RecoveryHandle;

/// Inputs fixed for the conversation's lifetime; everything here would
/// otherwise be re-read from config on every request.
pub(crate) struct RequestLayerInputs {
    pub model: Arc<ArcSwap<ModelHandle>>,
    pub settings: Arc<ArcSwap<TurnSettings>>,
    /// The recovery state: its model override (a fallback hop or a reauth
    /// swap) serves requests while set (09).
    pub recovery: RecoveryHandle,
    pub recency: Option<Arc<dyn RecencySource>>,
    /// The workspace root as a display string, anchoring read-lifecycle
    /// path normalization.
    pub write_root: Option<String>,
    pub compression: CompressionConfig,
    pub compaction: SharedCompactionState,
    /// `config.agent.preamble` followed by the discovered instructions.
    pub preamble_base: String,
    pub cwd: String,
}

/// Per-run state, snapshotted at `turn_start`.
struct RequestRunState {
    /// The system bytes for this run; rebuilt from the settings so a mode
    /// switch changes the prompt without rebuilding the agent.
    system: String,
    /// Requests served this run; the recency tail's turn counter.
    prepares: u32,
    /// Where this run started in the history view: messages before it
    /// arrived pre-altered from history, so the in-run request rewrites
    /// (read lifecycle, compression) only touch the suffix.
    run_start: usize,
}

pub(crate) struct RequestLayer {
    inputs: RequestLayerInputs,
    run: Mutex<RequestRunState>,
}

impl RequestLayer {
    pub(crate) fn new(inputs: RequestLayerInputs) -> Self {
        Self {
            inputs,
            run: Mutex::new(RequestRunState {
                system: String::new(),
                prepares: 0,
                run_start: 0,
            }),
        }
    }

    /// The system prompt for the current settings; identical bytes to the
    /// preamble the old `run_policy` built for the same inputs.
    fn system_prompt(settings: &TurnSettings, base: &str, cwd: &str) -> String {
        crate::prompt::build_system_prompt(
            &crate::prompt::Vars::new()
                .set("{cwd}", cwd)
                .set("{platform}", std::env::consts::OS)
                .set("{date}", crate::prompt::today_utc()),
            base,
            &crate::prompt::ResolvedSlots::default(),
            settings.mode.plan_path(),
        )
    }
}

#[async_trait]
impl LayerImpl for RequestLayer {
    async fn turn_start(
        &self,
        agent: &Agent,
        _run: &RunContext,
        message: &StoredMessage,
    ) -> Result<StoredMessage> {
        let settings = self.inputs.settings.load_full();
        let system = Self::system_prompt(&settings, &self.inputs.preamble_base, &self.inputs.cwd);
        // The run's own messages start after the current view (the input is
        // admitted after this hook); everything before it arrived
        // pre-altered from history by the between-runs passes.
        let run_start = agent.history().current().await.len();
        let mut run = self.run.lock().unwrap_or_else(|poison| poison.into_inner());
        run.system = system;
        run.prepares = 0;
        run.run_start = run_start;
        Ok(message.clone())
    }

    async fn prepare_request(
        &self,
        _agent: &Agent,
        _run: &RunContext,
        mut request: StepRequest,
    ) -> Result<StepRequest> {
        let settings = self.inputs.settings.load_full();
        // The recovery layer's model override (a fallback hop or a reauth
        // swap) serves the request while set; otherwise the conversation's
        // model. Every attempt re-prepares, so a hop struck between
        // attempts takes effect on the very next request.
        let (serving, hop) = {
            let state = self
                .inputs
                .recovery
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            (state.model_override.clone(), state.is_fallback_hop)
        };
        let handle = serving
            .clone()
            .unwrap_or_else(|| Arc::unwrap_or_clone(self.inputs.model.load_full()));
        let (system, prepares, run_start) = {
            let mut run = self.run.lock().unwrap_or_else(|poison| poison.into_inner());
            run.prepares += 1;
            (run.system.clone(), run.prepares, run.run_start)
        };
        request.system = system;
        // Volatile recency facts ride only this request.
        if let Some(source) = &self.inputs.recency {
            recency::attach_recency_tail_wire(&mut request.messages, source, prepares);
        }
        read_lifecycle::apply_to_request_from(
            &mut request.messages,
            self.inputs.write_root.as_deref(),
            run_start,
        );
        view::compress_request_view_from(
            &mut request.messages,
            &self.inputs.compression,
            run_start,
        );
        // Request overhead (preamble + tool schemas) is fixed per run, and
        // both gauges must measure the rewritten view before protocol
        // shaping, the way the estimator always has. The measured prompt
        // floor lives in the compaction state, so overflow recovery's
        // post-compaction clear reaches it.
        let overhead = RequestOverhead::from_parts(Some(request.system.as_str()), &request.tools);
        let measured = {
            let mut compaction = self.inputs.compaction.lock().await;
            compaction.set_request_overhead(overhead.tokens());
            compaction.measured_prompt()
        };
        let estimate = crate::compaction::estimate::estimate_messages(&request.messages);
        let prompt_tokens = estimate.saturating_add(overhead.tokens()).max(measured);
        request.model = handle.model.clone();
        request.messages = view::shape_messages(request.messages, handle.relocates_tool_images());
        // A serving hop carries its own budgets (hardening/06); the primary
        // and a same-model reauth swap keep the plain window clamp.
        let clamped = if hop {
            hop_max_tokens(settings.max_tokens, &handle, prompt_tokens)
        } else {
            crate::run::clamped_max_tokens(
                handle.context_length(),
                prompt_tokens,
                settings.max_tokens,
            )
        };
        request.options = request_options(
            &handle,
            settings.thinking,
            settings.fast,
            clamped,
            settings.temperature,
        );
        Ok(request)
    }

    async fn response(
        &self,
        _agent: &Agent,
        _run: &RunContext,
        response: StreamResponse,
    ) -> Result<StreamResponse> {
        // Zero is the missing-report sentinel, kept at the previous value.
        let measured = u64::from(response.usage.input) + u64::from(response.usage.cache_read);
        if measured > 0 {
            // The provider's own count of the prompt is the best floor the
            // clamp and the compaction fill check can have.
            self.inputs
                .compaction
                .lock()
                .await
                .set_measured_prompt(measured);
        }
        Ok(response)
    }
}

/// Sampling and transport options for one request: the thinking preference,
/// the fast tier, sampling, the output cap, and the system-message policy
/// the target protocol needs.
pub(crate) fn request_options(
    handle: &ModelHandle,
    thinking: ThinkingConfig,
    fast: bool,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
) -> RequestOptions {
    let mut options = RequestOptions::builder()
        .thinking(thinking.to_animist())
        .fast(fast)
        .build();
    // bon makes the optional members' setters take the inner type, so the
    // `None` cases simply leave the fields unset.
    if let Some(cap) = max_tokens {
        options.max_output_tokens = Some(cap.min(u64::from(u32::MAX)) as u32);
    }
    options.temperature = temperature;
    if handle.moves_system_to_preamble() {
        options.system_message_policy = animist::provider::SystemMessagePolicy::MoveToPreamble;
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layer's system prompt must be byte-identical to the one the old
    /// `run_policy` produced for the same inputs.
    #[test]
    fn system_prompt_bytes_match_the_old_policy_output() {
        // Literal byte invariants, not a call to the same builder the
        // layer uses: the env section is threaded verbatim (cwd, platform,
        // date), the base follows it, and the plan prompt only rides plan
        // mode. The template itself is prompt.rs's own tested surface.
        let settings = |mode| TurnSettings {
            mode,
            thinking: ThinkingConfig::default(),
            fast: false,
            temperature: None,
            max_tokens: None,
        };
        let base = "preamble text";
        let env_section = format!(
            "\n\nEnvironment:\n- Working directory: /project\n- Platform: {}\n- Date: {}",
            std::env::consts::OS,
            crate::prompt::today_utc(),
        );
        let build = RequestLayer::system_prompt(
            &settings(crate::run::mode::AgentMode::Build),
            base,
            "/project",
        );
        assert_eq!(build.matches(&env_section).count(), 1);
        let after_env = build
            .split_once(&env_section)
            .expect("the env section leads the instructions");
        assert!(
            after_env.1.starts_with(base),
            "the preamble+instructions base follows the env section verbatim"
        );
        assert!(!build.contains("{cwd}"), "no template var leaks: {build:?}");
        let plan = RequestLayer::system_prompt(
            &settings(crate::run::mode::AgentMode::Plan(std::path::PathBuf::from(
                "/project/plan.md",
            ))),
            base,
            "/project",
        );
        assert!(
            plan.len() > build.len() && plan.contains("/project/plan.md"),
            "plan mode appends the plan prompt with the resolved path"
        );
    }
}
