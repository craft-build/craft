//! Agent modes (C.17, plan subset): the surface tells the run which mode
//! it is in, and Plan mode restricts writes to the allocated plan file.
//! Flow mode is deliberately absent (ported last, task 99).

use std::path::{Path, PathBuf};

/// Tool-result message returned when a write outside the plan file is
/// attempted in plan mode. Ported verbatim from the reference
/// (`craft-agent/src/tools/mod.rs::PLAN_WRITE_RESTRICTED`).
pub const PLAN_WRITE_RESTRICTED: &str = "write restricted to plan file in plan mode";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum AgentMode {
    #[default]
    Build,
    Plan(PathBuf),
}

impl AgentMode {
    pub fn plan_path(&self) -> Option<&Path> {
        match self {
            Self::Build => None,
            Self::Plan(p) => Some(p),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_path_is_some_only_in_plan_mode() {
        assert_eq!(AgentMode::Build.plan_path(), None);
        let plan = AgentMode::Plan(PathBuf::from("/tmp/plan.md"));
        assert_eq!(plan.plan_path(), Some(Path::new("/tmp/plan.md")));
        assert_eq!(AgentMode::default(), AgentMode::Build);
    }
}
