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

    /// The plan-mode block reason for a call, or `None` when the call may
    /// run. In Plan mode only the allocated plan file may be written; write
    /// calls that name no path fail closed. `write_root` anchors path
    /// normalization the same way the dedup cache does.
    pub fn plan_block_reason(
        &self,
        write_root: Option<&Path>,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Option<String> {
        let plan = self.plan_path()?;
        if !crate::harness::dedup::ToolDedupCache::is_write(name) {
            return None;
        }
        let root = write_root.map(|p| p.to_string_lossy().into_owned());
        let plan_key = crate::harness::dedup::normalize_write_path_with_root(
            root.as_deref(),
            &plan.display().to_string(),
        );
        let paths = crate::harness::dedup::extract_write_paths(name, arguments);
        let blocked = paths.is_empty()
            || paths.iter().any(|p| {
                crate::harness::dedup::normalize_write_path_with_root(root.as_deref(), p)
                    != plan_key
            });
        blocked.then(|| PLAN_WRITE_RESTRICTED.to_owned())
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
