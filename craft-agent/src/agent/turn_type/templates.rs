//! Prompt templates for the narrow turn types. The bodies live in
//! `craft-agent/src/prompts/flow/<stage>.md` (shared with the former TUI Flow
//! path) and are re-exported here so the flow loop and tests reach them
//! through `agent::turn_type::templates`. `.craft/flow/<stage>.md` overrides
//! take precedence when present (mirrors skill override resolution).
//!
//! Migrated from the former `craft-flow/src/templates.rs`. Templates use
//! `{name}` braces for substitution.

use std::path::Path;

pub const SCOUT: &str = crate::prompt::FLOW_STAGE_SCOUT;
pub const TPM: &str = crate::prompt::FLOW_STAGE_TPM;
pub const PLAN: &str = crate::prompt::FLOW_STAGE_PLAN;
pub const REQ: &str = crate::prompt::FLOW_STAGE_REQ;
pub const EXECUTE: &str = crate::prompt::FLOW_STAGE_EXECUTE;
pub const REVIEW: &str = crate::prompt::FLOW_STAGE_REVIEW;
pub const QA: &str = crate::prompt::FLOW_STAGE_QA;
pub const INTEGRATOR: &str = crate::prompt::FLOW_STAGE_INTEGRATOR;
pub const VERIFIER: &str = crate::prompt::FLOW_STAGE_VERIFIER;

/// Read a `.craft/flow/<stage>.md` override from `dir`, falling back to the
/// built-in default. `stage` is the file stem (e.g. `scout`, `tpm`).
pub fn resolve(dir: &Path, stage: &str, default: &'static str) -> String {
    let candidate = dir.join(".craft").join("flow").join(format!("{stage}.md"));
    std::fs::read_to_string(&candidate).unwrap_or_else(|_| default.to_string())
}

/// Substitute `{name}` placeholders in `template` from `vars`. Missing keys are
/// left as-is so a misconfigured template fails loudly rather than silently
/// dropping content.
pub fn substitute(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (key, value) in vars {
        let placeholder = format!("{{{key}}}");
        out = out.replace(&placeholder, value);
    }
    out
}

/// Build the prompt for one turn type by substituting the relevant template.
/// `input` is the upstream content (Scout's findings for TPM, the requirement
/// for Execute, etc.); `chunk_id` is set for per-thread turn types; `workstream_id`
/// is the root thread id (design §5: the root thread absorbs the workstream).
pub fn stage_prompt(
    turn: super::TurnType,
    input: &str,
    chunk_id: Option<&str>,
    workstream_id: &str,
) -> String {
    let cid = chunk_id.unwrap_or("");
    match turn {
        super::TurnType::Scout => substitute(
            SCOUT,
            &[("workstream_id", workstream_id), ("request", input)],
        ),
        super::TurnType::Tpm => substitute(
            TPM,
            &[
                ("workstream_id", workstream_id),
                ("findings", input),
                ("request", input),
            ],
        ),
        super::TurnType::Plan => substitute(
            PLAN,
            &[("workstream_id", workstream_id), ("findings", input)],
        ),
        super::TurnType::Req => substitute(
            REQ,
            &[
                ("workstream_id", workstream_id),
                ("chunk_id", cid),
                ("findings", input),
            ],
        ),
        super::TurnType::Execute => substitute(
            EXECUTE,
            &[
                ("workstream_id", workstream_id),
                ("chunk_id", cid),
                ("findings", input),
            ],
        ),
        super::TurnType::Review => substitute(
            REVIEW,
            &[("workstream_id", workstream_id), ("chunk_id", cid)],
        ),
        super::TurnType::Qa => substitute(
            QA,
            &[
                ("workstream_id", workstream_id),
                ("chunk_id", cid),
                ("findings", input),
            ],
        ),
        super::TurnType::Report => substitute(
            REQ,
            &[
                ("workstream_id", workstream_id),
                ("chunk_id", cid),
                ("findings", input),
            ],
        ),
        super::TurnType::Integrator => substitute(
            INTEGRATOR,
            &[("workstream_id", workstream_id), ("findings", input)],
        ),
        super::TurnType::Verifier => substitute(
            VERIFIER,
            &[("workstream_id", workstream_id), ("findings", input)],
        ),
        super::TurnType::General => input.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TurnType;
    use test_case::test_case;

    #[test_case("hello {name}", &[("name", "world")], "hello world" ; "single_sub")]
    #[test_case("{a}{b}", &[("a", "x"), ("b", "y")], "xy" ; "two_subs")]
    #[test_case("no vars here", &[], "no vars here" ; "no_placeholders")]
    #[test_case("missing {x}", &[], "missing {x}" ; "missing_key_left_as_is")]
    fn substitute_replaces_placeholders(template: &str, vars: &[(&str, &str)], expected: &str) {
        assert_eq!(substitute(template, vars), expected);
    }

    #[test]
    fn all_stage_templates_substitute_without_panic() {
        let templates = [
            SCOUT, TPM, PLAN, REQ, EXECUTE, REVIEW, QA, INTEGRATOR, VERIFIER,
        ];
        for t in templates {
            let filled = substitute(
                t,
                &[
                    ("workstream_id", "ws"),
                    ("chunk_id", "c1"),
                    ("findings", "f"),
                    ("request", "r"),
                ],
            );
            assert!(!filled.contains("{workstream_id}"));
        }
    }

    #[test]
    fn general_stage_prompt_passes_input_verbatim() {
        let prompt = stage_prompt(TurnType::General, "fix the login bug", None, "ws");
        assert_eq!(prompt, "fix the login bug");
    }
}
