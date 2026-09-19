//! Permission rule engine.
//!
//! Every tool call is checked against, in order: session rules (grants and
//! denies made this session), persistent rules from `permissions.toml`
//! (global config dir and project `.craft/`), then per-tool/global defaults.
//! Any matching deny wins; unclaimed scopes fall through to the default
//! effect (`prompt` unless configured otherwise). "Always allow" answers are
//! written back comment-preservingly via `toml_edit`.
//!
//! Ported from the reference `craft-agent/src/permissions.rs` +
//! `craft-config` permission layer. Deviations: no plugin rule store (no
//! plugin subsystem yet), no yolo toggle, no plan mode, and no builtin
//! allow for in-project writes — this repo's approval gate is
//! ask-by-default for mutations, which the engine preserves. Auto-review
//! (E.7) toggles live here; its reviewer model call lives in
//! [`crate::auto_review`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::storage::StorageError;

mod compound;
mod rules;

use rules::{rule_matches_scope, rule_reaches};

/// Tree-sitter bash parsing that splits compound commands into per-command
/// scopes (task B.2).
pub mod bash {
    pub use super::compound::{BashScopes, permission_scopes};
}

pub use rules::{
    append_permission_rule, generalized_scopes, is_universal_scope, load_permissions,
    physical_boundary_check, scope_matches,
};

pub const DEFAULT_DENY_GUIDANCE: &str =
    "Do not retry. Try a different approach or ask the user for guidance.";

pub(crate) const ASK_TIMEOUT: Duration = Duration::from_secs(60 * 30);

/// Tests assert on this exact prefix; a wording tweak here updates them in one place.
pub const PERMISSION_DENIED_PREFIX: &str = "Permission denied for";

pub const BOUNDARY_UNVERIFIABLE_PREFIX: &str = "Cannot verify project boundary for";

pub const PERMISSIONS_FILE: &str = "permissions.toml";

pub(super) const PROJECT_DIR: &str = ".craft";

/// Tools whose scope is a file path the call touches.
pub const FILE_WRITE_TOOLS: &[&str] = &[
    "write",
    "edit",
    "edit_lines",
    "insert_lines",
    "multiedit",
    "delete",
    "apply_patch",
    "move",
];

/// Read-only tools that run without an explicit user decision unless a deny
/// rule says otherwise. Mirrors the gate allowlist this engine replaces.
pub const READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "list",
    "inspect",
    "retrieve",
    "list_tools",
    "bash_status",
    "bash_watch",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultEffect {
    Allow,
    Deny,
    #[default]
    Prompt,
}

#[derive(Debug, Clone)]
pub enum PermissionTarget {
    Global,
    Project(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKey {
    Wildcard,
    Native(Arc<str>),
    McpServer { server: Arc<str> },
    McpTool { server: Arc<str>, tool: Arc<str> },
}

/// Check if a name matches the LLM wire format: `^[a-zA-Z0-9_-]{1,64}$`.
pub fn is_valid_wire_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn is_valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl ToolKey {
    pub fn native(name: &str) -> Self {
        match name {
            "*" => Self::Wildcard,
            _ => {
                assert!(!name.is_empty(), "native tool name must not be empty");
                assert!(
                    !name.contains('.'),
                    "native tool name must not contain dots: {name:?} - use ToolKey::parse for MCP tools"
                );
                Self::Native(name.into())
            }
        }
    }

    pub fn is_mcp(&self) -> bool {
        matches!(self, Self::McpServer { .. } | Self::McpTool { .. })
    }
}

impl std::fmt::Display for ToolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wildcard => write!(f, "*"),
            Self::Native(name) => write!(f, "{name}"),
            Self::McpServer { server } => write!(f, "{server}.*"),
            Self::McpTool { server, tool } => write!(f, "{server}.{tool}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub tool: ToolKey,
    pub scope: Option<String>,
    pub effect: Effect,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionsConfig {
    pub default: DefaultEffect,
    pub tool_defaults: HashMap<ToolKey, DefaultEffect>,
    pub rules: Vec<PermissionRule>,
}

#[derive(Debug)]
pub enum PermissionCheck {
    Allowed,
    Denied,
    NeedsPrompt {
        tool: ToolKey,
        scopes: Vec<String>,
        /// Set when the caller could not confidently determine what the
        /// scopes are (e.g. an unsplittable bash compound command): the
        /// prompt must be shown even though an allow rule matches.
        force_prompt: bool,
    },
}

#[derive(Debug)]
pub struct PermissionError {
    tool: String,
    scopes: String,
    guidance: Option<String>,
}

impl std::fmt::Display for PermissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} `{}` ({}).",
            PERMISSION_DENIED_PREFIX, self.tool, self.scopes
        )?;
        if let Some(g) = &self.guidance {
            write!(f, " User guidance: {g}")
        } else {
            write!(f, " {DEFAULT_DENY_GUIDANCE}")
        }
    }
}

impl PermissionError {
    pub(crate) fn new(tool: &str, scopes: &[String]) -> Self {
        Self {
            tool: tool.to_string(),
            scopes: scopes.join("; "),
            guidance: None,
        }
    }

    pub(crate) fn with_guidance(tool: &str, scopes: &[String], guidance: String) -> Self {
        Self {
            tool: tool.to_string(),
            scopes: scopes.join("; "),
            guidance: Some(guidance),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionAnswer {
    AllowOnce,
    AllowSession,
    AllowAlwaysLocal,
    AllowAlwaysGlobal,
    Deny,
    DenyWithGuidance(String),
    DenyAlwaysLocal,
    DenyAlwaysGlobal,
}

impl PermissionAnswer {
    pub fn is_allow(&self) -> bool {
        matches!(
            self,
            Self::AllowOnce | Self::AllowSession | Self::AllowAlwaysLocal | Self::AllowAlwaysGlobal
        )
    }

    pub fn encode(&self) -> String {
        match self {
            Self::AllowOnce => "allow".to_string(),
            Self::AllowSession => "allow_session".to_string(),
            Self::AllowAlwaysLocal => "allow_always_local".to_string(),
            Self::AllowAlwaysGlobal => "allow_always_global".to_string(),
            Self::Deny => "deny".to_string(),
            Self::DenyWithGuidance(g) => format!("deny:{g}"),
            Self::DenyAlwaysLocal => "deny_always_local".to_string(),
            Self::DenyAlwaysGlobal => "deny_always_global".to_string(),
        }
    }

    pub fn decode(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Self::AllowOnce),
            "allow_session" => Some(Self::AllowSession),
            "allow_always_local" => Some(Self::AllowAlwaysLocal),
            "allow_always_global" => Some(Self::AllowAlwaysGlobal),
            "deny" => Some(Self::Deny),
            "deny_always_local" => Some(Self::DenyAlwaysLocal),
            "deny_always_global" => Some(Self::DenyAlwaysGlobal),
            _ if s.starts_with("deny:") => {
                let guidance = s.strip_prefix("deny:").unwrap();
                if guidance.is_empty() {
                    Some(Self::Deny)
                } else {
                    Some(Self::DenyWithGuidance(guidance.to_string()))
                }
            }
            _ => None,
        }
    }

    pub fn guidance(&self) -> Option<&str> {
        match self {
            Self::DenyWithGuidance(g) => Some(g),
            _ => None,
        }
    }
}

pub struct PermissionManager {
    session_rules: Mutex<Vec<PermissionRule>>,
    config_rules: Vec<PermissionRule>,
    default: DefaultEffect,
    tool_defaults: HashMap<ToolKey, DefaultEffect>,
    cwd: PathBuf,
    auto_review: AtomicBool,
}

impl PermissionManager {
    pub fn new(config: PermissionsConfig, cwd: PathBuf) -> Self {
        let has_wildcard_deny = config
            .rules
            .iter()
            .any(|r| matches!(r.tool, ToolKey::Wildcard) && r.effect == Effect::Deny);
        if has_wildcard_deny {
            eprintln!(
                "permissions: wildcard deny detected — this blocks ALL tools including \
                 builtins (write/edit/multiedit). Use per-tool rules \
                 instead if you want selective access."
            );
        }
        let has_wildcard_allow = config
            .rules
            .iter()
            .any(|r| matches!(r.tool, ToolKey::Wildcard) && r.effect == Effect::Allow);
        if has_wildcard_allow {
            eprintln!(
                "permissions: wildcard allow detected — this permits ALL tools including \
                 write/edit/multiedit. Use per-tool rules \
                 instead if you want selective access."
            );
        }

        let mut tool_defaults = config.tool_defaults;
        for name in READ_ONLY_TOOLS {
            tool_defaults
                .entry(ToolKey::native(name))
                .or_insert(DefaultEffect::Allow);
        }

        Self {
            session_rules: Mutex::new(Vec::new()),
            config_rules: config.rules,
            default: config.default,
            tool_defaults,
            cwd,
            auto_review: AtomicBool::new(false),
        }
    }

    /// Project root relative paths in tool scopes are resolved against.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Fresh manager for a new session runtime: shares config rules but owns
    /// empty session rules so restoring one session never clobbers another's
    /// grants.
    #[cfg(test)]
    pub(crate) fn fork(&self) -> Self {
        Self {
            session_rules: Mutex::new(Vec::new()),
            config_rules: self.config_rules.clone(),
            default: self.default,
            tool_defaults: self.tool_defaults.clone(),
            cwd: self.cwd.clone(),
            auto_review: AtomicBool::new(self.is_auto_review()),
        }
    }

    fn session_rules(&self) -> std::sync::MutexGuard<'_, Vec<PermissionRule>> {
        self.session_rules.lock().unwrap_or_else(|e| {
            eprintln!("permissions: mutex was poisoned, recovering");
            e.into_inner()
        })
    }

    /// The order of the checks below is the policy itself, not an accident of
    /// how it was written: denies first, then explicit allows, then the
    /// defaults. Moving one moves the rules.
    fn check_inner(&self, tool: &ToolKey, scopes: &[&str], force_prompt: bool) -> PermissionCheck {
        let session = self.session_rules();

        // Any matching deny wins, however broadly it was aimed. Only allows
        // are outranked by `force_prompt`.
        let mut unclaimed_scopes: Vec<&str> = Vec::with_capacity(scopes.len());

        for scope in scopes {
            let mut has_allow = false;
            for r in session.iter().chain(&self.config_rules) {
                if !rule_reaches(&r.tool, tool) {
                    continue;
                }
                if !rule_matches_scope(r, scope) {
                    continue;
                }
                match r.effect {
                    Effect::Deny => {
                        return PermissionCheck::Denied;
                    }
                    Effect::Allow => has_allow = true,
                }
            }
            if !has_allow && !force_prompt {
                unclaimed_scopes.push(scope);
            }
        }

        let pending: Vec<&str> = if force_prompt {
            scopes.to_vec()
        } else {
            unclaimed_scopes
        };
        if pending.is_empty() {
            return PermissionCheck::Allowed;
        }

        let eff = self
            .tool_defaults
            .get(tool)
            .copied()
            .or_else(|| {
                let server = match tool {
                    ToolKey::McpTool { server, .. } => server,
                    _ => return None,
                };
                self.tool_defaults
                    .get(&ToolKey::McpServer {
                        server: server.clone(),
                    })
                    .copied()
            })
            .unwrap_or(self.default);
        match eff {
            DefaultEffect::Deny => PermissionCheck::Denied,
            DefaultEffect::Allow if !force_prompt => PermissionCheck::Allowed,
            DefaultEffect::Allow | DefaultEffect::Prompt => PermissionCheck::NeedsPrompt {
                tool: tool.clone(),
                scopes: pending.into_iter().map(|s| s.to_string()).collect(),
                force_prompt,
            },
        }
    }

    pub fn check(&self, tool: &ToolKey, scopes: &[String]) -> PermissionCheck {
        let refs: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
        self.check_inner(tool, &refs, false)
    }

    /// Multi-scope check used by compound bash commands: every scope must be
    /// allowed or the uncovered ones are prompted for together. With
    /// `force_prompt` the allow rules are skipped entirely — the user must
    /// still see the command.
    pub fn check_multi(
        &self,
        tool: &ToolKey,
        scopes: &[String],
        force_prompt: bool,
    ) -> PermissionCheck {
        let refs: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
        self.check_inner(tool, &refs, force_prompt)
    }

    /// Toggle LLM auto-review mode; returns the new state.
    pub fn toggle_auto_review(&self) -> bool {
        let prev = self.auto_review.fetch_xor(true, Ordering::Relaxed);
        !prev
    }

    pub fn is_auto_review(&self) -> bool {
        self.auto_review.load(Ordering::Relaxed)
    }

    /// Persist an auto-review decision as session rules covering the exact
    /// scopes reviewed. Returns the `allow` flag for callers chaining it
    /// straight into a run/skip decision. Unlike [`Self::apply_decision`],
    /// scopes are not generalized — the reviewer only saw the literal scopes.
    pub fn apply_auto_review(&self, tool: &ToolKey, scopes: &[String], allow: bool) -> bool {
        let effect = if allow { Effect::Allow } else { Effect::Deny };
        for s in scopes {
            self.add_session_rule(PermissionRule {
                tool: tool.clone(),
                scope: Some(s.clone()),
                effect,
            });
        }
        allow
    }

    pub fn add_session_rule(&self, rule: PermissionRule) {
        let mut rules = self.session_rules();
        let exists = rules
            .iter()
            .any(|r| r.tool == rule.tool && r.scope == rule.scope && r.effect == rule.effect);
        if !exists {
            rules.push(rule);
        }
    }

    #[cfg(test)]
    pub(crate) fn session_rules_snapshot(&self) -> Vec<PermissionRule> {
        self.session_rules().clone()
    }

    /// Outside-cwd paths are not blocked here. They flow through the normal
    /// permission prompt (which uses the same canonicalization via
    /// [`scope_matches`]). Only unresolvable boundaries are hard-blocked.
    #[cfg(test)]
    pub(crate) fn boundary_block_reason(&self, path: &Path) -> Option<String> {
        match physical_boundary_check(&self.cwd, path) {
            Some(_) => None,
            None => Some(format!(
                "{BOUNDARY_UNVERIFIABLE_PREFIX} {} \
                 (project root could not be resolved)",
                path.display()
            )),
        }
    }

    /// Record a user answer against the tool/scopes it was about, returning
    /// the rules that must be persisted to disk (empty for session-only or
    /// one-shot answers).
    pub fn apply_decision(
        &self,
        tool: &ToolKey,
        scopes: &[String],
        answer: &PermissionAnswer,
    ) -> Vec<(ToolKey, Option<String>, Effect, PermissionTarget)> {
        let mut persist = Vec::new();
        let resolved = if answer.is_allow() || tool.is_mcp() {
            generalized_scopes(tool, scopes)
        } else {
            scopes.to_vec()
        };

        match answer {
            PermissionAnswer::AllowOnce
            | PermissionAnswer::Deny
            | PermissionAnswer::DenyWithGuidance(_) => {}
            PermissionAnswer::AllowSession => {
                for s in &resolved {
                    self.add_session_rule(PermissionRule {
                        tool: tool.clone(),
                        scope: Some(s.clone()),
                        effect: Effect::Allow,
                    });
                }
            }
            PermissionAnswer::AllowAlwaysLocal
            | PermissionAnswer::AllowAlwaysGlobal
            | PermissionAnswer::DenyAlwaysLocal
            | PermissionAnswer::DenyAlwaysGlobal => {
                let effect = if answer.is_allow() {
                    Effect::Allow
                } else {
                    Effect::Deny
                };
                let target = match answer {
                    PermissionAnswer::AllowAlwaysLocal | PermissionAnswer::DenyAlwaysLocal => {
                        PermissionTarget::Project(self.cwd.clone())
                    }
                    _ => PermissionTarget::Global,
                };
                for s in &resolved {
                    self.add_session_rule(PermissionRule {
                        tool: tool.clone(),
                        scope: Some(s.clone()),
                        effect,
                    });
                    persist.push((tool.clone(), Some(s.clone()), effect, target.clone()));
                }
            }
        }
        persist
    }
}

#[derive(Debug)]
pub enum PermissionWriteError {
    Parse(String),
    NotATable { tool: String },
    NotAnArray { tool: String, key: String },
    Io(std::io::Error),
}

impl std::fmt::Display for PermissionWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "failed to parse permissions file: {e}"),
            Self::NotATable { tool } => {
                write!(f, "`{tool}` is not a table in the permissions file")
            }
            Self::NotAnArray { tool, key } => {
                write!(f, "`{tool}.{key}` is not an array in the permissions file")
            }
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PermissionWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PermissionWriteError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<StorageError> for PermissionWriteError {
    fn from(e: StorageError) -> Self {
        Self::Io(std::io::Error::other(e.to_string()))
    }
}

#[cfg(test)]
mod tests;
