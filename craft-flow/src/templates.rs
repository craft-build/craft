//! Default prompt templates for each Flow stage. The bodies live in
//! `craft-agent/src/prompts/flow/<stage>.md` (shared with the TUI Flow path)
//! and are re-exported here so the CLI and tests reach them through
//! `craft_flow::templates`. `.craft/flow/<stage>.md` overrides take precedence
//! when present (mirrors skill override resolution).
//!
//! Templates use `{name}` braces for substitution.

use std::path::Path;

pub const SCOUT: &str = craft_agent::prompt::FLOW_STAGE_SCOUT;
pub const TPM: &str = craft_agent::prompt::FLOW_STAGE_TPM;
pub const PLAN: &str = craft_agent::prompt::FLOW_STAGE_PLAN;
pub const REQ: &str = craft_agent::prompt::FLOW_STAGE_REQ;
pub const EXECUTE: &str = craft_agent::prompt::FLOW_STAGE_EXECUTE;
pub const REVIEW: &str = craft_agent::prompt::FLOW_STAGE_REVIEW;
pub const QA: &str = craft_agent::prompt::FLOW_STAGE_QA;
pub const INTEGRATOR: &str = craft_agent::prompt::FLOW_STAGE_INTEGRATOR;
pub const VERIFIER: &str = craft_agent::prompt::FLOW_STAGE_VERIFIER;

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

#[cfg(test)]
mod tests {
    use super::*;
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
}
