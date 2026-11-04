//! Request layer: the per-step request-view pipeline, in the order the old
//! loop applied it — in-run read lifecycle and output compression, protocol
//! shaping, the output-token clamp, and the sampling options. The system
//! prompt rides here too, so its bytes stay exactly the preamble the
//! surfaces always built. Stored history is never touched: every rewrite
//! exists because the protocol rejects the shape animist passes through, or
//! covers results the current run produced (everything older arrives
//! pre-altered from history, 08).

use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use async_trait::async_trait;

use animist::agent::{Agent, LayerImpl, Result, RunContext, StepRequest};
use animist::history::StoredMessage;
use animist::provider::{RequestOptions, StreamResponse};

use crate::compaction::SharedCompactionState;
use crate::compaction::estimate::RequestOverhead;
use crate::compression::CompressionConfig;
use crate::harness::read_lifecycle;
use crate::harness::retry::hop_max_tokens;
use crate::harness::view;
use crate::provider::ModelHandle;
use crate::thinking::ThinkingConfig;

use super::conversation::TurnSettings;
use super::recovery::RecoveryHandle;

/// Floor for the output-token clamp, never applied above the configured
/// cap. Servers like vLLM reject `prompt + max_tokens > window` even when
/// the prompt alone fits; the floor keeps the clamp from trading that
/// overflow for a too-small budget (reference `MIN_OUTPUT_TOKENS`).
pub(crate) const MIN_OUTPUT_TOKENS: u64 = 4096;

/// Reduce the request's output cap to what remains of the context window.
/// `None` leaves the request alone (no window or no configured cap: the
/// provider picks its own). `prompt_tokens` should be the larger of the
/// chars/4 estimate and the last measured input count — the estimate is a
/// floor that skips the clamp on exactly the long sessions it exists for.
pub(crate) fn clamped_max_tokens(
    window: Option<u32>,
    prompt_tokens: u64,
    configured: Option<u64>,
) -> Option<u64> {
    let window = match window {
        Some(window) => u64::from(window),
        // No known window: leave the configured cap alone.
        None => return configured,
    };
    let cap = configured?;
    let remaining = window.saturating_sub(prompt_tokens);
    // The `min` keeps the floor from raising the cap over what was
    // configured, since the provider would reject a number it never offered.
    if cap > remaining {
        Some(remaining.max(MIN_OUTPUT_TOKENS).min(cap))
    } else {
        Some(cap)
    }
}

/// Inputs fixed for the conversation's lifetime; everything here would
/// otherwise be re-read from config on every request.
pub(crate) struct RequestLayerInputs {
    pub model: Arc<ArcSwap<ModelHandle>>,
    pub settings: Arc<ArcSwap<TurnSettings>>,
    /// The recovery state: its model override (a fallback hop or a reauth
    /// swap) serves requests while set.
    pub recovery: RecoveryHandle,
    /// The workspace root as a display string, anchoring read-lifecycle
    /// path normalization.
    pub write_root: Option<String>,
    pub compression: CompressionConfig,
    pub compaction: SharedCompactionState,
    /// `config.agent.preamble` followed by the discovered instructions;
    /// becomes the `Instructions` section of the per-run prompt build.
    pub instructions: String,
    pub cwd: String,
    /// Extra sections appended after `Instructions` (a subagent's role
    /// section); `Plan mode` still lands last, per run.
    pub sections: Vec<animist::agent::Section>,
    /// The rendering surface every system prompt in this conversation uses.
    pub prompt_format: crate::prompt::PromptFormat,
}

/// Per-run state, snapshotted at `turn_start`.
struct RequestRunState {
    /// The system bytes for this run; rebuilt from the settings so a mode
    /// switch changes the prompt without rebuilding the agent.
    system: String,
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
                run_start: 0,
            }),
        }
    }

    /// The system prompt for the current settings, built per run (the
    /// date is run-scoped): the chat prompt, extra sections after
    /// `Instructions`, and `Plan mode` last when the mode carries a path.
    /// A free function so tests exercise exactly what `turn_start` sends.
    fn system_prompt(
        settings: &TurnSettings,
        cwd: &str,
        instructions: &str,
        extra_sections: &[animist::agent::Section],
        format: crate::prompt::PromptFormat,
    ) -> String {
        let date = crate::prompt::today_utc();
        let ctx = crate::prompt::Context {
            cwd,
            platform: std::env::consts::OS,
            date: &date,
            instructions,
        };
        let mut prompt = crate::prompt::system(&ctx, None);
        prompt.sections.extend(extra_sections.iter().cloned());
        if let Some(plan) = settings.mode.plan_path() {
            prompt.sections.push(crate::prompt::plan_section(plan));
        }
        format.render(&prompt)
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
        let system = Self::system_prompt(
            &settings,
            &self.inputs.cwd,
            &self.inputs.instructions,
            &self.inputs.sections,
            self.inputs.prompt_format,
        );
        // The run's own messages start after the current view (the input is
        // admitted after this hook); everything before it arrived
        // pre-altered from history by the between-runs passes.
        let run_start = agent.history().current().await.len();
        let mut run = self.run.lock().unwrap_or_else(|poison| poison.into_inner());
        run.system = system;
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
        let (system, run_start) = {
            let run = self.run.lock().unwrap_or_else(|poison| poison.into_inner());
            (run.system.clone(), run.run_start)
        };
        request.system = system;
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
        // A serving hop carries its own budgets; the primary
        // and a same-model reauth swap keep the plain window clamp.
        let clamped = if hop {
            hop_max_tokens(settings.max_tokens, &handle, prompt_tokens)
        } else {
            crate::harness::request::clamped_max_tokens(
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

    /// The layer's system prompt is exactly the conversation's base prompt,
    /// plus the `Plan mode` section only in plan mode, rendered in the
    /// configured format. Runs once per format: the request is the
    /// contract, both ways.
    #[test]
    fn system_prompt_bytes_match_the_old_policy_output() {
        let settings = |mode| TurnSettings {
            mode,
            thinking: ThinkingConfig::default(),
            fast: false,
            temperature: None,
            max_tokens: None,
        };
        let date = crate::prompt::today_utc();
        let ctx = crate::prompt::Context {
            cwd: "/project",
            platform: std::env::consts::OS,
            date: &date,
            instructions: "preamble text",
        };
        for format in [
            crate::prompt::PromptFormat::Markdown,
            crate::prompt::PromptFormat::Xml,
        ] {
            let build = RequestLayer::system_prompt(
                &settings(crate::harness::mode::AgentMode::Build),
                "/project",
                "preamble text",
                &[],
                format,
            );
            assert_eq!(
                build,
                format.render(&crate::prompt::system(&ctx, None)),
                "{format:?}"
            );
            assert!(!build.contains("/project/plan.md"), "{format:?}");
            let plan = RequestLayer::system_prompt(
                &settings(crate::harness::mode::AgentMode::Plan(
                    std::path::PathBuf::from("/project/plan.md"),
                )),
                "/project",
                "preamble text",
                &[],
                format,
            );
            assert_eq!(
                plan,
                format.render(&crate::prompt::system(
                    &ctx,
                    Some(std::path::Path::new("/project/plan.md")),
                )),
                "{format:?}"
            );
            assert!(plan.contains("/project/plan.md"), "{format:?}");
        }
    }
}
