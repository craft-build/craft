use craft_tool_macro::Tool;
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use tracing::info;
use uuid::Uuid;

use super::{DescriptionContext, FileReadTracker, ToolContext, ToolFilter, ToolRegistry};
use crate::agent;
use crate::template;
use crate::tools::ToolAudience;
use crate::types::{Finding, ToolOutput};
use crate::{Agent, AgentInput, AgentMode, AgentParams, AgentRunParams, EventSender};
use craft_config::ToolOutputLines;

const REVIEWER_PROMPT: &str = include_str!("../prompts/reviewer.md");

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Review {
    #[param(description = "What to review (e.g., 'Review the auth module for security issues')")]
    task: String,
    #[param(description = "Files to focus on (optional)")]
    focus_files: Option<Vec<String>>,
}

impl Review {
    pub const NAME: &str = "review";
    pub const DESCRIPTION: &str = "Spawn a code review subagent that reads files, checks against styleguide rules, and reports structured findings with priorities (P0-P3) and a verdict.";
    pub const EXAMPLES: Option<&str> = Some(
        r#"[{"task": "Review error handling in the agent module", "focus_files": ["craft-agent/src/agent/run.rs"]}]"#,
    );

    pub fn start_header(&self) -> String {
        format!("review({})", self.task)
    }

    pub async fn execute(&self, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let vars = template::env_vars();

        let prompt = match &self.focus_files {
            Some(files) => {
                format!(
                    "{}\n\nFocus files:\n{}",
                    self.task,
                    files
                        .iter()
                        .map(|f| format!("- {f}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            }
            None => self.task.clone(),
        };

        let cwd_owned = vars.apply("{cwd}").into_owned();
        let instructions =
            tokio::task::spawn_blocking(move || agent::load_instruction_text(&cwd_owned))
                .await
                .map_err(|e| format!("review failed: {e}"))?;

        let system = vars
            .apply(&crate::prompt::assemble_raw(
                REVIEWER_PROMPT,
                &ctx.prompt_slots,
                &instructions,
            ))
            .into_owned();

        let allowed = [
            "read",
            "grep",
            "styleguide_list",
            "styleguide_search",
            "styleguide_get",
            "report_finding",
            "batch",
        ];
        let snapshot = ToolRegistry::native().iter();
        let tool_names: Vec<String> = snapshot
            .iter()
            .filter(|e| {
                let name = e.name();
                e.tool.audience().contains(ToolAudience::RESEARCH_SUB) && allowed.contains(&name)
            })
            .map(|e| e.name().to_owned())
            .collect();

        let filter = ToolFilter::Only(tool_names);
        let ctx_desc = DescriptionContext { filter: &filter };
        let mut tools = ToolRegistry::native().definitions(
            &vars,
            &ctx_desc,
            ctx.model.supports_tool_examples(),
        );
        if let Some(ref mcp) = ctx.mcp {
            mcp.extend_tools(&mut tools);
        }

        let session_id = Uuid::new_v4().to_string();
        let (sub_tx, sub_rx) = flume::unbounded::<crate::Envelope>();
        let sub_event_tx = EventSender::new(sub_tx, ctx.event_tx.run_id());
        let parent_tx = ctx.event_tx.clone();

        let subagent_info = ctx.tool_use_id.as_ref().map(|id| crate::SubagentInfo {
            parent_tool_use_id: id.to_owned(),
            name: format!("review: {}", self.task),
            prompt: Some(prompt.clone()),
            model: Some(ctx.model.spec()),
            answer_tx: None,
        });

        let findings: Arc<Mutex<Vec<Finding>>> = Arc::new(Mutex::new(Vec::new()));
        let findings_clone = Arc::clone(&findings);

        tokio::spawn(async move {
            while let Ok(mut envelope) = sub_rx.recv_async().await {
                if !filter_subagent_envelope(&envelope, &findings_clone) {
                    continue;
                }
                envelope.subagent = subagent_info.clone();
                let _ = parent_tx.send_envelope(envelope);
            }
        });

        let (_child_trigger, child_cancel) = ctx.cancel.child();
        let input = AgentInput {
            message: prompt,
            mode: AgentMode::Build,
            ..Default::default()
        };

        let mut history = crate::History::new(Vec::new());
        let agent = Agent::new(
            AgentParams {
                provider: Arc::clone(&ctx.provider),
                model: (*ctx.model).clone(),
                config: ctx.config.clone(),
                tool_output_lines: ToolOutputLines::default(),
                permissions: Arc::clone(&ctx.permissions),
                session_id: Some(session_id),
                timeouts: ctx.timeouts,
                file_tracker: FileReadTracker::fresh(),
                prompt_slots: Arc::clone(&ctx.prompt_slots),
                compression: ctx.compression.clone(),
                findings_store: None,
                fs: Arc::new(crate::tools::LocalFs),
                doom: Arc::new(std::sync::Mutex::new(crate::DoomTracker::new())),
            },
            AgentRunParams {
                history: &mut history,
                system,
                event_tx: sub_event_tx,
                tools,
                promoted: crate::tools::PromotedTools::new(),
                tool_build: None,
                hooks: None,
            },
        )
        .with_cancel(child_cancel)
        .with_mcp(ctx.mcp.clone());

        info!(task = %self.task, "review subagent spawning");
        let result = agent.run(input).await;
        drop(_child_trigger);

        result.map_err(|e| format!("review sub-agent error: {e}"))?;

        let messages = history.into_vec();
        let text = messages
            .iter()
            .rev()
            .filter(|m| matches!(m.role, craft_providers::Role::Assistant))
            .flat_map(|m| m.content.iter())
            .find_map(|b| match b {
                craft_providers::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("(no response)")
            .to_string();

        if let Some(tool_use_id) = ctx.tool_use_id.clone() {
            let _ = ctx.event_tx.send(crate::AgentEvent::SubagentHistory {
                tool_use_id,
                messages,
            });
        }

        let findings = match Arc::try_unwrap(findings) {
            Ok(m) => m.into_inner().unwrap_or_default(),
            Err(arc) => arc.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        };

        if !findings.is_empty()
            && let Some(store) = ctx.findings_store.as_ref()
        {
            store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(&self.task, findings.iter().cloned());
        }

        Ok(ToolOutput::ReviewResult {
            findings,
            verdict: text,
        })
    }
}

super::impl_tool!(Review, audience = super::ToolAudience::MAIN, kind = "think");

impl super::ToolInvocation for Review {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Review::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Review::execute(&self, ctx).await.into() })
    }
}

/// Returns `true` if the envelope should be forwarded to the parent listener.
/// Returns `false` for events that the review tool handles internally (terminal events
/// and `ToolDone` whose findings get captured into `findings`).
fn filter_subagent_envelope(
    envelope: &crate::Envelope,
    findings: &Arc<Mutex<Vec<Finding>>>,
) -> bool {
    use crate::AgentEvent;
    match &envelope.event {
        AgentEvent::ToolDone(done) => {
            if let ToolOutput::Findings(f) = &done.output {
                findings
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(f.clone());
            }
            false
        }
        AgentEvent::Done { .. }
        | AgentEvent::Error { .. }
        | AgentEvent::ToolOutput { .. }
        | AgentEvent::ToolPending { .. }
        | AgentEvent::SubagentHistory { .. } => false,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolDoneEvent;
    use crate::types::{Finding, Priority};

    fn finding(title: &str) -> Finding {
        Finding {
            title: title.into(),
            body: "body".into(),
            priority: Priority::P1,
            confidence: 0.9,
            file_path: "f.rs".into(),
            line_start: 1,
            line_end: 1,
            rule_ids: vec![],
            suggestion: None,
        }
    }

    fn envelope(event: crate::AgentEvent) -> crate::Envelope {
        crate::Envelope {
            event,
            subagent: None,
            run_id: 0,
        }
    }

    #[test]
    fn tool_done_with_findings_captured_and_swallowed() {
        let bucket: Arc<Mutex<Vec<Finding>>> = Arc::new(Mutex::new(Vec::new()));
        let env = envelope(crate::AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            id: "t1".into(),
            tool: "report_finding".into(),
            output: ToolOutput::Findings(vec![finding("a"), finding("b")]),
            is_error: false,
            annotation: None,
            written_path: None,
        })));
        assert!(!filter_subagent_envelope(&env, &bucket));
        assert_eq!(bucket.lock().unwrap().len(), 2);
    }

    #[test]
    fn tool_done_without_findings_swallowed_no_capture() {
        let bucket: Arc<Mutex<Vec<Finding>>> = Arc::new(Mutex::new(Vec::new()));
        let env = envelope(crate::AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            id: "t1".into(),
            tool: "read".into(),
            output: ToolOutput::Plain("x".into()),
            is_error: false,
            annotation: None,
            written_path: None,
        })));
        assert!(!filter_subagent_envelope(&env, &bucket));
        assert!(bucket.lock().unwrap().is_empty());
    }

    #[test]
    fn text_delta_forwarded() {
        let bucket: Arc<Mutex<Vec<Finding>>> = Arc::new(Mutex::new(Vec::new()));
        let env = envelope(crate::AgentEvent::TextDelta { text: "hi".into() });
        assert!(filter_subagent_envelope(&env, &bucket));
    }
}
