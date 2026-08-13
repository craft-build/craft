use std::sync::Arc;

use craft_providers::{Message, StopReason};
use tracing::warn;

use crate::agent::transitions::{self, ResolvedTransition, TurnProposal};
use crate::{AgentError, AgentEvent, AgentMode};

use super::Agent;

const MAX_JUDGE_CONTINUATIONS: u8 = 5;
/// Pushed after a Flow narrow turn ends without a shift (`ShiftOut`). The
/// next request would otherwise trail an assistant message, which providers
/// like Anthropic reject as assistant prefill.
pub(super) const SHIFT_OUT_TO_GENERAL_PROMPT: &str = "Control returns to `general`. Re-derive the next step from the typed log: shift into the next narrow turn type the work needs, or end the run if the goal is met.";
const ADVISOR_FOLLOWUP_PROMPT: &str = "<advisor-note>\nA lightweight advisor reviewed your last turn and flagged a {severity}:\n{note}\n\nAddress this concern before finishing. Make the change; keep it minimal and do not narrate it in comments or prose. Only explain if it does not apply, in one sentence.\n</advisor-note>";

pub(super) struct AgentFlow {
    pub(super) flow_search: crate::tools::flow_search::FlowSearchHandle,
    pub(super) thread_id: crate::agent::typed_log::ThreadId,
    pub(super) thread_history:
        Option<Arc<std::sync::Mutex<crate::agent::typed_log::ThreadHistory>>>,
    pub(super) thread_manager: Option<Arc<std::sync::Mutex<crate::agent::threads::ThreadManager>>>,
    pub(super) flow_advisor: Option<Arc<dyn crate::agent::flow_loop::FlowAdvisor + Send + Sync>>,
    pub(super) flow_progress_tx: Option<flume::Sender<crate::agent::flow_loop::FlowProgress>>,
    pub(super) goal: Option<String>,
    pub(super) goal_criteria: Vec<String>,
    pub(super) judge_continuations: u8,
    pub(super) advisor_continuations: u32,
    pub(super) advisor_state: Option<crate::agent::advisor::AdvisorState>,
    pub(super) ttsr: Option<Arc<crate::agent::ttsr::TtsrManager>>,
    pub(super) mode: AgentMode,
    pub(super) turn_type: crate::agent::turn_type::TurnType,
    pub(super) pending_approval_stop: Option<StopReason>,
}

pub(super) fn parse_shift_output(text: &str) -> Option<crate::types::ToolOutput> {
    let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let shift = value.get("shift")?;
    let target_str = shift.get("target")?.as_str()?;
    let target = crate::agent::turn_type::TurnType::parse(target_str)?;
    let rationale = shift
        .get("rationale")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(crate::types::ToolOutput::ShiftTurnType { target, rationale })
}

pub(super) enum AdvisorTurnAction {
    Continue(crate::agent::advisor::AdvisorNote),
    Stop,
}

pub(super) fn advisor_turn_action(
    note: Option<crate::agent::advisor::AdvisorNote>,
    cfg: &craft_config::AdvisorConfig,
    pending_approval: bool,
    continuations: u32,
) -> AdvisorTurnAction {
    let Some(note) = note else {
        return AdvisorTurnAction::Stop;
    };
    if pending_approval
        || continuations >= cfg.max_act_turns
        || !crate::agent::advisor::should_act(note.severity, cfg.auto_act)
    {
        return AdvisorTurnAction::Stop;
    }
    AdvisorTurnAction::Continue(note)
}

pub(super) fn advisor_followup_message(note: &crate::agent::advisor::AdvisorNote) -> Message {
    Message::synthetic(
        ADVISOR_FOLLOWUP_PROMPT
            .replace("{severity}", note.severity.as_str())
            .replace("{note}", &note.message),
    )
}

impl<'h> Agent<'h> {
    pub(super) async fn run_advisor(&mut self) -> Option<crate::agent::advisor::AdvisorNote> {
        let state = self.flow.advisor_state.as_mut()?;
        let result = crate::agent::advisor::review(
            state,
            self.history.as_slice(),
            &self.config.advisor,
            &self.io.provider,
            &self.io.model,
            self.io.timeouts,
            self.io.session_id.as_ref(),
        )
        .await;
        match result {
            Ok(Some(note)) => {
                let _ = self.io.event_tx.send(AgentEvent::AdvisorNote {
                    severity: note.severity.as_str().to_string(),
                    message: note.message.clone(),
                });
                Some(note)
            }
            Ok(None) => None,
            Err(e) => {
                warn!(error = %e, "advisor review failed");
                None
            }
        }
    }

    /// Build an owned [`ExtractionCtx`] for the post-turn memory
    /// auto-extraction hook, or `None` when disabled: in tests, when the
    /// config flag is off, in Flow narrow stages, or when a provider cannot
    /// be resolved.
    pub(super) fn memory_extraction_ctx(
        &self,
    ) -> Option<crate::agent::memory_extraction::ExtractionCtx> {
        use crate::agent::memory_extraction::{self, ExtractionCtx};
        if cfg!(test) || !self.config.memory_extraction {
            return None;
        }
        if matches!(self.flow.mode, AgentMode::Flow(_))
            && self.flow.turn_type != crate::agent::turn_type::TurnType::General
        {
            return None;
        }
        let user_text = self
            .history
            .as_slice()
            .iter()
            .rev()
            .find(|m| matches!(m.role, craft_providers::Role::User))
            .and_then(|m| m.user_text())?
            .to_string();
        if user_text.trim().is_empty() {
            return None;
        }

        let project_root = memory_extraction::memory_project_root();
        let id = memory_extraction::project_id_for(&project_root);
        let state_dir = craft_storage::paths::state_dir().ok()?;
        let memory_dir = state_dir.join("projects").join(id).join("memories");

        Some(ExtractionCtx {
            project_root,
            memory_dir,
            user_text,
            provider: Arc::clone(&self.io.provider),
            model: self.io.model.as_ref().clone(),
            timeouts: self.io.timeouts,
            session_id: self.io.session_id.clone(),
        })
    }

    pub(super) fn last_shift_request(&self) -> Option<crate::types::ToolOutput> {
        if !matches!(self.flow.mode, AgentMode::Flow(_)) {
            return None;
        }
        let assistant =
            self.history.as_slice().iter().rev().find(|m| {
                matches!(m.role, craft_providers::Role::Assistant) && m.has_tool_calls()
            })?;
        let mut shift_ids: Vec<&str> = assistant
            .tool_uses()
            .filter(|(_, name, _)| *name == crate::tools::SHIFT_TOOL_NAME)
            .map(|(id, _, _)| id)
            .collect();
        let last_id = shift_ids.pop()?;
        let result_text = self.history.as_slice().iter().rev().find_map(|m| {
            m.content.iter().find_map(|b| match b {
                craft_providers::ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == last_id => Some(content.as_str()),
                _ => None,
            })
        })?;
        parse_shift_output(result_text)
    }

    /// Flow mode only: commit one distilled typed-log entry for the turn that
    /// just completed. No-op outside Flow mode or when no `ThreadHistory` is
    /// attached.
    pub(super) fn commit_turn_write(&mut self, turn_type: crate::agent::turn_type::TurnType) {
        let Some(hist) = self.flow.thread_history.as_ref() else {
            return;
        };
        let entry_type = turn_type.spec().write.entry;
        let content = self.last_turn_text();
        hist.lock().unwrap_or_else(|e| e.into_inner()).append(
            self.flow.thread_id.clone(),
            entry_type,
            content,
        );
    }

    /// The just-completed turn's final assistant text (verbatim).
    pub(super) fn last_turn_text(&self) -> String {
        self.history
            .as_slice()
            .iter()
            .rev()
            .find(|m| matches!(m.role, craft_providers::Role::Assistant))
            .and_then(|m| m.first_text_content())
            .unwrap_or("")
            .to_string()
    }

    /// Flow mode only: run the between-turn `FlowAdvisor`. Returns the
    /// Advisor's forced transition as a `TurnProposal` (target always
    /// `General`). `None` when there is no advisor, no override, or outside
    /// Flow mode.
    pub(super) async fn run_flow_advisor(&mut self) -> Option<TurnProposal> {
        if !matches!(self.flow.mode, AgentMode::Flow(_)) {
            return None;
        }
        let advisor = self.flow.flow_advisor.clone()?;
        let hist = self.flow.thread_history.clone()?;
        let mgr_snapshot = self
            .flow
            .thread_manager
            .as_ref()?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let thread_id = self.flow.thread_id.clone();
        let turn_type = self.flow.turn_type;
        let forced = advisor
            .review(hist, mgr_snapshot, thread_id.clone(), turn_type)
            .await;
        let forced = forced?;
        let note = crate::agent::flow_loop::record_advisor_note(
            self.flow.thread_history.as_ref().unwrap(),
            &thread_id,
            &forced,
        );
        if let Some(tx) = self.flow.flow_progress_tx.as_ref() {
            let _ = tx.send(crate::agent::flow_loop::FlowProgress::AdvisorNote {
                thread_id: thread_id.to_string(),
                addressed_to: thread_id.to_string(),
                severity: forced.severity,
                message: note,
            });
        }
        Some(TurnProposal::self_report(
            crate::agent::turn_type::TurnType::General,
            crate::agent::turn_type::ThreadAction::Advance,
            forced.note,
        ))
    }

    /// Flow mode only: the turn-boundary shift logic. Drains the last
    /// `shift` request, runs the current type's `TransitionRule` set through
    /// `transitions::resolve`, and either shifts or pushes a soft `Illegal`
    /// message and stays.
    pub(super) async fn apply_shift_if_requested(&mut self) -> Result<(), AgentError> {
        if !matches!(self.flow.mode, AgentMode::Flow(_)) {
            return Ok(());
        }
        let shift = self.last_shift_request();
        let advisor_override = self.run_flow_advisor().await;
        if shift.is_none() && advisor_override.is_none() {
            return Ok(());
        }
        let rules = self.flow.turn_type.spec().transitions;
        let proposal = shift.as_ref().map(|s| {
            let crate::types::ToolOutput::ShiftTurnType { target, rationale } = s else {
                unreachable!("last_shift_request returns only ShiftTurnType");
            };
            TurnProposal::self_report(
                *target,
                crate::agent::turn_type::ThreadAction::Advance,
                rationale.clone(),
            )
        });
        let resolved = transitions::resolve(
            &rules,
            proposal.as_ref().unwrap_or(&TurnProposal::self_report(
                self.flow.turn_type,
                crate::agent::turn_type::ThreadAction::Advance,
                String::new(),
            )),
            advisor_override.as_ref(),
        );
        match resolved {
            ResolvedTransition::Accepted { target, action } => {
                let from = self.flow.turn_type;
                // Goal-approval gate: the `Tpm -> Plan` transition ends the
                // run with `AwaitingGoalApproval` after emitting the goal doc.
                if from == crate::agent::turn_type::TurnType::Tpm
                    && target == crate::agent::turn_type::TurnType::Plan
                {
                    let goal_doc = self.last_turn_text();
                    self.commit_turn_write(from);
                    if let Some(tx) = self.flow.flow_progress_tx.as_ref() {
                        let _ =
                            tx.send(crate::agent::flow_loop::FlowProgress::GoalReady { goal_doc });
                    }
                    self.flow.pending_approval_stop = Some(StopReason::AwaitingGoalApproval);
                    return Ok(());
                }
                self.commit_turn_write(from);
                self.advance_turn_type(target, action);
            }
            ResolvedTransition::Illegal { proposed } => {
                self.history.push(Message::user(format!(
                    "Illegal shift from {} to {}; staying.",
                    self.flow.turn_type.as_str(),
                    proposed.as_str()
                )));
            }
        }
        Ok(())
    }

    /// Flow mode only: mutate `turn_type`, advance the `ThreadManager` (if
    /// attached), emit `FlowProgress::TurnTypeEntered`, and (for narrow types)
    /// push a stage brief. Single place `turn_type` changes after `run` seeds
    /// it to `General`.
    pub(super) fn advance_turn_type(
        &mut self,
        target: crate::agent::turn_type::TurnType,
        _action: crate::agent::turn_type::ThreadAction,
    ) {
        if let Some(mgr) = self.flow.thread_manager.as_ref() {
            mgr.lock()
                .unwrap_or_else(|e| e.into_inner())
                .advance(&self.flow.thread_id, target);
        }
        self.flow.turn_type = target;
        if let Some(tx) = self.flow.flow_progress_tx.as_ref() {
            let _ = tx.send(crate::agent::flow_loop::FlowProgress::TurnTypeEntered {
                thread_id: self.flow.thread_id.to_string(),
                turn_type: target,
            });
        }
        if target != crate::agent::turn_type::TurnType::General
            && let Some(brief) = self.stage_brief(target)
        {
            self.history.push(Message::synthetic(brief));
        }
    }

    /// Render the stage brief for a narrow `target` type: the write
    /// commitment, the resolved core-read entries inlined from the typed log,
    /// and the legal next shifts. Returns `None` only when no `ThreadHistory`
    /// is attached.
    pub(super) fn stage_brief(&self, target: crate::agent::turn_type::TurnType) -> Option<String> {
        let hist = self.flow.thread_history.as_ref()?;
        let spec = target.spec();
        let mut out = String::new();
        out.push_str(&format!(
            "You are now in the `{}` turn type of Flow workstream `{}`.\n\n",
            target.as_str(),
            self.flow.thread_id
        ));
        out.push_str("Begin this turn's work now. Do not just acknowledge the shift; produce or gather the artifact this turn type is responsible for, then either shift to the next type that the work needs, or back to `general` if the immediate question is answered.\n\n");
        out.push_str("## Write\n");
        out.push_str(&format!(
            "Commit one `{}` entry as your final reply this turn (prose or markdown, not JSON)",
            spec.write.entry.as_str()
        ));
        if let Some(guidance) = spec.write.guidance {
            out.push_str(". ");
            out.push_str(guidance);
        } else {
            out.push('.');
        }
        out.push('\n');

        let parent_id = self.parent_thread_id();
        let root_id = hist
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .root_thread_id()
            .clone();
        let mut inlined = false;
        for read in &spec.read.core {
            let scope_id = match read.level {
                crate::agent::turn_type::ThreadLevel::Own => Some(self.flow.thread_id.clone()),
                crate::agent::turn_type::ThreadLevel::Parent => parent_id.clone(),
                crate::agent::turn_type::ThreadLevel::Root => Some(root_id.clone()),
            };
            let Some(scope) = scope_id else {
                continue;
            };
            let entry = hist
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .projection(read.entry, &scope)
                .cloned();
            if let Some(entry) = entry {
                if !inlined {
                    out.push_str("\n## Context (from the typed log)\n");
                    inlined = true;
                }
                out.push_str(&format!(
                    "### {} ({} @ {})\n{}\n\n",
                    read.entry.as_str(),
                    match read.level {
                        crate::agent::turn_type::ThreadLevel::Own => "this thread",
                        crate::agent::turn_type::ThreadLevel::Parent => "parent thread",
                        crate::agent::turn_type::ThreadLevel::Root => "root thread",
                    },
                    scope,
                    entry.content.trim()
                ));
            }
        }
        if inlined {
            out.push_str("Fetch more with `flow_search` or `read path=\"flow://<path>\"`.\n");
        }

        if !spec.transitions.is_empty() {
            out.push_str("\n## Legal next shifts\n");
            out.push_str(
                "Pick the one the work needs. Returning to `general` is always fine when the \
                 narrow role has done its job; you are not forced down a fixed pipeline.\n",
            );
            for rule in &spec.transitions {
                out.push_str(&format!(
                    "- `{}`{}\n",
                    rule.target.as_str(),
                    match rule.action {
                        crate::agent::turn_type::ThreadAction::Spawn => " (spawn child thread)",
                        crate::agent::turn_type::ThreadAction::Exit => " (exit this thread)",
                        crate::agent::turn_type::ThreadAction::Advance => "",
                    },
                ));
            }
        }
        Some(out)
    }

    pub(super) fn parent_thread_id(&self) -> Option<crate::agent::typed_log::ThreadId> {
        let mgr = self.flow.thread_manager.as_ref()?;
        mgr.lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&self.flow.thread_id)
            .and_then(|t| t.parent.clone())
    }

    pub(super) async fn run_goal_judge(
        &mut self,
        goal: &str,
        goal_criteria: &[String],
        stop_reason: Option<StopReason>,
    ) -> Result<super::TurnOutcome, AgentError> {
        if self.flow.judge_continuations >= MAX_JUDGE_CONTINUATIONS {
            warn!(
                continuations = self.flow.judge_continuations,
                "judge continuation cap reached, allowing stop"
            );
            return Ok(super::TurnOutcome::Done(stop_reason));
        }
        if !goal_criteria.is_empty() {
            return self
                .run_criteria_judge(goal, goal_criteria, stop_reason)
                .await;
        }
        let outcome = crate::agent::judge::evaluate(
            goal,
            self.history.as_slice(),
            &self.io.provider,
            &self.io.model,
            self.config.judge_model.as_deref(),
            self.io.timeouts,
            self.io.session_id.as_ref(),
        )
        .await;
        match outcome {
            Ok(crate::agent::judge::JudgeOutcome::Done) => {
                self.io.event_tx.send(AgentEvent::Info {
                    message: "Goal met (verified by judge)".into(),
                })?;
                Ok(super::TurnOutcome::Done(stop_reason))
            }
            Ok(crate::agent::judge::JudgeOutcome::NotDone(reason)) => {
                self.flow.judge_continuations += 1;
                let note = format!(
                    "The judge evaluated that the goal is not yet fully met: {reason}. \
                     Continue working toward the goal: {goal}. Do not stop until it is done."
                );
                self.history.push(Message::synthetic(note));
                Ok(super::TurnOutcome::Continue)
            }
            Ok(crate::agent::judge::JudgeOutcome::Criteria { .. }) => {
                Ok(super::TurnOutcome::Done(stop_reason))
            }
            Err(e) => {
                warn!(error = %e, "judge evaluation failed, allowing stop (fail-open)");
                Ok(super::TurnOutcome::Done(stop_reason))
            }
        }
    }

    pub(super) async fn run_criteria_judge(
        &mut self,
        goal: &str,
        criteria: &[String],
        stop_reason: Option<StopReason>,
    ) -> Result<super::TurnOutcome, AgentError> {
        let outcome = crate::agent::judge::evaluate_criteria(
            criteria,
            self.history.as_slice(),
            &self.io.provider,
            &self.io.model,
            self.config.judge_model.as_deref(),
            self.io.timeouts,
            self.io.session_id.as_ref(),
        )
        .await;
        match outcome {
            Ok(crate::agent::judge::JudgeOutcome::Criteria { met, unmet }) => {
                if unmet.is_empty() {
                    self.io.event_tx.send(AgentEvent::Info {
                        message: format!("Goal met — all {} criteria verified", met.len()),
                    })?;
                    Ok(super::TurnOutcome::Done(stop_reason))
                } else {
                    self.flow.judge_continuations += 1;
                    let list = unmet
                        .iter()
                        .map(|c| format!("- {c}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let note = format!(
                        "Unmet criteria:\n{list}\n\nContinue working toward the goal: {goal}. \
                         Do not stop until all criteria are met."
                    );
                    self.history.push(Message::synthetic(note));
                    Ok(super::TurnOutcome::Continue)
                }
            }
            Ok(_) => Ok(super::TurnOutcome::Done(stop_reason)),
            Err(e) => {
                warn!(error = %e, "criteria judge evaluation failed, allowing stop (fail-open)");
                Ok(super::TurnOutcome::Done(stop_reason))
            }
        }
    }
}

#[cfg(test)]
pub(super) mod flow_tests;
