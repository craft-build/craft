//! Request layer: the per-step request-view pipeline, in the order the old
//! loop applied it — recency tail, read lifecycle, output compression,
//! protocol shaping, the output-token clamp, and the sampling options. The
//! system prompt rides here too, so its bytes stay exactly the preamble the
//! surfaces always built. Stored history is never touched: every rewrite
//! exists because the protocol rejects the shape animist passes through.

use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use async_trait::async_trait;

use animist::agent::{Agent, LayerImpl, Result, RunContext, StepRequest};
use animist::history::StoredMessage;
use animist::provider::{RequestOptions, StreamResponse};

use crate::compaction::estimate::RequestOverhead;
use crate::compression::CompressionConfig;
use crate::compression::store::SharedCompressionStore;
use crate::provider::ModelHandle;
use crate::run::SharedCompactionState;
use crate::run::read_lifecycle;
use crate::run::recency::{self, RecencySource};
use crate::run::view;
use crate::thinking::ThinkingConfig;

use super::conversation::TurnSettings;

/// Inputs fixed for the conversation's lifetime; everything here would
/// otherwise be re-read from config on every request.
pub(crate) struct RequestLayerInputs {
    pub model: Arc<ArcSwap<ModelHandle>>,
    pub settings: Arc<ArcSwap<TurnSettings>>,
    pub recency: Option<Arc<dyn RecencySource>>,
    pub compression_store: Option<SharedCompressionStore>,
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
    /// Measured input tokens from the last completed stream; feeds the
    /// output-token clamp a number better than the chars/4 estimate.
    measured_prompt_tokens: u64,
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
                measured_prompt_tokens: 0,
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
        _agent: &Agent,
        _run: &RunContext,
        message: &StoredMessage,
    ) -> Result<StoredMessage> {
        let settings = self.inputs.settings.load_full();
        let system = Self::system_prompt(&settings, &self.inputs.preamble_base, &self.inputs.cwd);
        let mut run = self.run.lock().unwrap_or_else(|poison| poison.into_inner());
        run.system = system;
        run.prepares = 0;
        run.measured_prompt_tokens = 0;
        Ok(message.clone())
    }

    async fn prepare_request(
        &self,
        _agent: &Agent,
        _run: &RunContext,
        mut request: StepRequest,
    ) -> Result<StepRequest> {
        let settings = self.inputs.settings.load_full();
        let handle = self.inputs.model.load_full();
        let (system, prepares, measured) = {
            let mut run = self.run.lock().unwrap_or_else(|poison| poison.into_inner());
            run.prepares += 1;
            (run.system.clone(), run.prepares, run.measured_prompt_tokens)
        };
        request.system = system;
        // Volatile recency facts ride only this request.
        if let Some(source) = &self.inputs.recency {
            recency::attach_recency_tail_wire(&mut request.messages, source, prepares);
        }
        read_lifecycle::apply_to_request(
            &mut request.messages,
            self.inputs.compression_store.as_ref(),
            self.inputs.write_root.as_deref(),
        );
        view::compress_request_view(&mut request.messages, &self.inputs.compression);
        // Request overhead (preamble + tool schemas) is fixed per run, and
        // both gauges must measure the rewritten view before protocol
        // shaping, the way the estimator always has.
        let overhead = RequestOverhead::from_parts(Some(request.system.as_str()), &request.tools);
        if let Ok(mut guard) = self.inputs.compaction.lock() {
            guard.set_request_overhead(overhead.tokens());
        }
        let estimate = crate::compaction::estimate::estimate_messages(&request.messages);
        let prompt_tokens = estimate.saturating_add(overhead.tokens()).max(measured);
        request.messages = view::shape_messages(request.messages, handle.relocates_tool_images());
        let clamped = crate::run::clamped_max_tokens(
            handle.context_length(),
            prompt_tokens,
            settings.max_tokens,
        );
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
            self.run
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .measured_prompt_tokens = measured;
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
