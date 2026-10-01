//! Permission rule engine.
//!
//! Every tool call is checked against, in order: session rules (grants and
//! denies made this session), persistent rules from `permissions.bml`
//! (global config dir and project `.craft/`), then per-tool/global defaults.
//! Any matching deny wins; unclaimed scopes fall through to the default
//! effect (`prompt` unless configured otherwise). "Always allow" answers are
//! written back to `permissions.bml` (regenerated; comments not kept).
//!
//! Ported from the reference `craft-agent/src/permissions.rs` +
//! `craft-config` permission layer. Deviations: no plugin rule store (no
//! plugin subsystem yet), no yolo toggle, no plan mode, and no plugin rules.
//! The reference's `builtin_rules(cwd)` are preserved and extended: every
//! path-scoped mutation except `delete` is pre-approved inside the project
//! root, and `task` everywhere. Auto-review
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

pub const PERMISSIONS_FILE: &str = "permissions.bml";

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
    "question",
    "todo_write",
    "view_image",
    "websearch",
    "webfetch",
    // Phase 5: MCP resource reads — read-only against the remote server,
    // deny rules can still block them.
    "mcp_read",
];

/// Builtins allowed without a decision, matching the reference's
/// `builtin_rules` non-file-write allow. `task` is not read-only (a
/// `general` subagent writes), so it lives here, not in [`READ_ONLY_TOOLS`];
/// the child's own tool calls still pass through this same gate.
pub const BUILTIN_ALLOW_TOOLS: &[&str] = &["task", "review"];

/// Every argosy tool native name, auto-allowed as the integration plan
/// requests — except the project-writing code tools (`astgrep` with
/// `apply`, `conflicts` with `resolve`), which stay on the approval
/// seam. A user deny rule still outranks these (the rule loop returns on
/// the first deny). Keyed per-tool on the native name, so a
/// user-configured MCP server named `argosy` is unaffected.
pub const ARGOSY_ALLOW_TOOLS: &[&str] = &[
    "search",
    "list_skills",
    "get_skill",
    "search_rules",
    "read_memory",
    "read_document",
    "write_memory",
    "delete_memory",
    "write_rule",
    "delete_rule",
    "write_document",
    "delete_document",
    "promote",
    "ask",
    "outline",
    "zoom",
    "inspect",
    "callgraph",
    "repomap",
    "start_review",
    "review_diff",
    "report_finding",
    "review_findings",
];

/// File-write tools the builtin rules pre-approve inside the project root.
/// Covers every mutation the engine scopes by path except `delete`, which is
/// deliberately left to prompt: destructive removal should always be an
/// explicit decision. Writes outside the project root still prompt, as do the
/// gated shell and network tools. Scope-based, so it cannot live in
/// `tool_defaults`.
pub const BUILTIN_WRITE_ALLOW_TOOLS: &[&str] = &[
    "write",
    "edit",
    "multiedit",
    "edit_lines",
    "insert_lines",
    "apply_patch",
    "move",
];

/// Builtin allow rules, modelled on the reference's `builtin_rules(cwd)`.
/// Each file-write tool is allowed only under the project root (`cwd/**`); a
/// deny rule still outranks these because the rule loop returns on the first
/// deny.
fn builtin_rules(cwd: &Path) -> Vec<PermissionRule> {
    let cwd_glob = format!("{}/**", cwd.display());
    BUILTIN_WRITE_ALLOW_TOOLS
        .iter()
        .map(|tool| PermissionRule {
            tool: ToolKey::native(tool),
            scope: Some(cwd_glob.clone()),
            effect: Effect::Allow,
        })
        .collect()
}

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

    /// Tool key for a call-site (wire) name: MCP tools arrive from the model
    /// as `server__tool` wire names, everything else is native. Keying the
    /// call correctly is what lets `mcp "server"` rules and defaults apply
    /// instead of falling through to the wildcard/default path.
    pub fn parse(name: &str) -> Self {
        match name {
            "*" => Self::Wildcard,
            _ if let Some((server, tool)) = name.split_once(crate::mcp::WIRE_SEPARATOR) => {
                if crate::permissions::is_valid_server_name(server)
                    && !tool.is_empty()
                    && is_valid_wire_name(tool)
                {
                    Self::McpTool {
                        server: server.into(),
                        tool: tool.into(),
                    }
                } else {
                    Self::native(name)
                }
            }
            _ => Self::native(name),
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

#[derive(Debug, PartialEq, Eq)]
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
        /// The tool carries `readOnlyHint: true` and no destructive hint:
        /// still a prompt (hints never auto-allow), but the UI can phrase it
        /// in a neutral, low-risk tone.
        low_risk: bool,
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

/// Server-declared MCP tool annotations (Phase 3). Hints, never grants.
#[derive(Debug, Clone, Copy, Default)]
struct McpHints {
    read_only: Option<bool>,
    destructive: Option<bool>,
}

/// Per-manager annotation table, rebuilt whenever the MCP tool snapshot's
/// generation changes so stale tools do not linger.
#[derive(Debug, Default, Clone)]
struct McpAnnotationTable {
    generation: Option<u64>,
    hints: HashMap<(Arc<str>, Arc<str>), McpHints>,
}

pub struct PermissionManager {
    session_rules: Mutex<Vec<PermissionRule>>,
    config_rules: Vec<PermissionRule>,
    /// Reference-style builtin allows (in-project file writes). Checked with
    /// the other rules, before defaults, so a deny still wins.
    builtin_rules: Vec<PermissionRule>,
    default: DefaultEffect,
    tool_defaults: HashMap<ToolKey, DefaultEffect>,
    cwd: PathBuf,
    auto_review: AtomicBool,
    /// MCP tool annotations (Phase 3): `server__tool` → declared hints.
    mcp_annotations: Mutex<McpAnnotationTable>,
    /// G.1 `--yolo` (`--dangerously-skip-permissions`): when set, every
    /// check short-circuits to allowed before rules are consulted.
    yolo: AtomicBool,
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
                 builtins (write/edit/multiedit/task). Use per-tool rules \
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
                 builtins (write/edit/multiedit/task). Use per-tool rules \
                 instead if you want selective access."
            );
        }

        let mut tool_defaults = config.tool_defaults;
        for name in READ_ONLY_TOOLS {
            tool_defaults
                .entry(ToolKey::native(name))
                .or_insert(DefaultEffect::Allow);
        }
        for name in BUILTIN_ALLOW_TOOLS {
            tool_defaults
                .entry(ToolKey::native(name))
                .or_insert(DefaultEffect::Allow);
        }
        // Argosy tools are natives now: one per-tool allow per name. The
        // repo-mutating code tools (`astgrep` with `apply`, `conflicts`
        // with `resolve`) are deliberately absent, so they stay on the
        // approval seam; a user-written per-tool or server deny rule
        // still wins.
        for name in crate::knowledge::tool_definitions() {
            if crate::permissions::ARGOSY_ALLOW_TOOLS.contains(&name.native) {
                tool_defaults
                    .entry(ToolKey::native(name.native))
                    .or_insert(DefaultEffect::Allow);
            }
        }
        let builtin_rules = builtin_rules(&cwd);

        Self {
            session_rules: Mutex::new(Vec::new()),
            config_rules: config.rules,
            builtin_rules,
            default: config.default,
            tool_defaults,
            cwd,
            auto_review: AtomicBool::new(false),
            mcp_annotations: Mutex::new(McpAnnotationTable::default()),
            yolo: AtomicBool::new(false),
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
            builtin_rules: self.builtin_rules.clone(),
            default: self.default,
            tool_defaults: self.tool_defaults.clone(),
            cwd: self.cwd.clone(),
            auto_review: AtomicBool::new(self.is_auto_review()),
            mcp_annotations: Mutex::new(
                self.mcp_annotations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            ),
            yolo: AtomicBool::new(self.is_yolo()),
        }
    }

    /// G.1 `--yolo`: skip every permission prompt and allow every check.
    /// Explicit deny rules in `permissions.bml` are also bypassed, matching
    /// the reference's "allow everything" semantics.
    pub fn set_yolo(&self, yes: bool) {
        self.yolo.store(yes, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_yolo(&self) -> bool {
        self.yolo.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Start the session with auto-review on (`craft -A`, G.1).
    pub fn set_auto_review(&self, yes: bool) {
        self.auto_review
            .store(yes, std::sync::atomic::Ordering::Relaxed);
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
        if self.is_yolo() {
            return PermissionCheck::Allowed;
        }
        // MCP annotations (Phase 3): consulted after the rule loop below —
        // deny rules and explicit user rules always win, hints never grant.
        // A destructive hint mirrors the unsplittable-bash precedent: the
        // user must see the call even if an allow rule covers it.
        let (read_only_hint, destructive_hint) = match tool {
            ToolKey::McpTool { server, tool } => {
                let table = self
                    .mcp_annotations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                table
                    .hints
                    .get(&(Arc::clone(server), Arc::clone(tool)))
                    .map(|h| (h.read_only, h.destructive))
                    .unwrap_or((None, None))
            }
            _ => (None, None),
        };
        let force_prompt = force_prompt || matches!(destructive_hint, Some(true));
        let session = self.session_rules();

        // Any matching deny wins, however broadly it was aimed. Only allows
        // are outranked by `force_prompt`.
        let mut unclaimed_scopes: Vec<&str> = Vec::with_capacity(scopes.len());

        for scope in scopes {
            let mut has_allow = false;
            for r in session
                .iter()
                .chain(&self.config_rules)
                .chain(&self.builtin_rules)
            {
                if !rule_reaches(&r.tool, tool) {
                    continue;
                }
                if !rule_matches_scope(&self.cwd, r, scope) {
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

        let tool_eff = self.tool_defaults.get(tool).copied().or_else(|| {
            let server = match tool {
                ToolKey::McpTool { server, .. } => server,
                _ => return None,
            };
            self.tool_defaults
                .get(&ToolKey::McpServer {
                    server: server.clone(),
                })
                .copied()
        });
        let eff = tool_eff.unwrap_or(self.default);
        // `readOnlyHint: true` (without a destructive hint) keeps the prompt
        // but flags it low-risk for the UI; it never upgrades Allow to silent.
        // A user-written tool default is an explicit mcp rule, so it stands.
        let low_risk = tool_eff.is_none()
            && matches!(read_only_hint, Some(true))
            && !matches!(destructive_hint, Some(true));
        match eff {
            DefaultEffect::Deny => PermissionCheck::Denied,
            DefaultEffect::Allow if !force_prompt && !low_risk => PermissionCheck::Allowed,
            DefaultEffect::Allow | DefaultEffect::Prompt => PermissionCheck::NeedsPrompt {
                tool: tool.clone(),
                scopes: pending.into_iter().map(|s| s.to_string()).collect(),
                force_prompt,
                low_risk,
            },
        }
    }

    /// Record one MCP tool's declared annotations (Phase 3). Called when the
    /// tool snapshot is (re)published; `sync_mcp_annotations` is the bulk form.
    pub fn register_mcp_annotations(
        &self,
        server: &str,
        tool: &str,
        read_only: Option<bool>,
        destructive: Option<bool>,
    ) {
        let mut table = self
            .mcp_annotations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        table.hints.insert(
            (server.into(), tool.into()),
            McpHints {
                read_only,
                destructive,
            },
        );
    }

    /// Rebuild the annotation table from the published MCP descriptors when
    /// the snapshot generation moved, so per-turn callers can call this
    /// cheaply without re-registering every tool every turn.
    pub fn sync_mcp_annotations(&self, handle: &crate::mcp::McpHandle) {
        let generation = handle.generation();
        let mut table = self
            .mcp_annotations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if table.generation == Some(generation) {
            return;
        }
        table.hints.clear();
        for d in handle.tool_descriptors() {
            let Some((server, tool)) = d.qualified_name.split_once(crate::mcp::SEPARATOR) else {
                continue;
            };
            if d.read_only_hint.is_some() || d.destructive_hint.is_some() {
                table.hints.insert(
                    (server.into(), tool.into()),
                    McpHints {
                        read_only: d.read_only_hint,
                        destructive: d.destructive_hint,
                    },
                );
            }
        }
        table.generation = Some(generation);
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

/// The scope a tool call is about: the touched path for file tools,
/// per-command scopes for bash (task B.2), `*` otherwise (scope-less rules
/// still match it; everything else falls to the default). Shared by the
/// TUI's approval gate and the ACP permission gate so the two frontends
/// prompt for exactly the same things. The flag is the bash parser's
/// `force_prompt`: the scopes could not be derived confidently, so allow
/// rules must not silence the prompt.
pub fn scope_for_call(root: &Path, name: &str, args: &serde_json::Value) -> (Vec<String>, bool) {
    if name == "mcp_read"
        && let (Some(server), Some(uri)) = (
            args.get("server").and_then(|v| v.as_str()),
            args.get("uri").and_then(|v| v.as_str()),
        )
    {
        return (vec![format!("mcp:{server}:{uri}")], false);
    }
    if name == "task" {
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return (vec![format!("task:{description}")], false);
    }
    if name == "bash"
        && let Some(command) = args.get("command").and_then(|v| v.as_str())
        && let Some(scopes) = bash::permission_scopes(command)
    {
        return (scopes.scopes, scopes.force_prompt);
    }
    if name == "apply_patch"
        && let Some(patch) = args.get("patch_text").and_then(|v| v.as_str())
    {
        return (
            crate::tools::patch_paths(patch)
                .iter()
                .map(|p| resolve_scope_path(root, p))
                .collect(),
            false,
        );
    }
    if name == "move"
        && let (Some(source), Some(destination)) = (
            args.get("source").and_then(|v| v.as_str()),
            args.get("destination").and_then(|v| v.as_str()),
        )
    {
        return (
            [source, destination]
                .iter()
                .map(|p| resolve_scope_path(root, p))
                .collect(),
            false,
        );
    }
    if FILE_WRITE_TOOLS.contains(&name) {
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            return (vec![resolve_scope_path(root, path)], false);
        }
        if let Some(files) = args.get("files").and_then(|v| v.as_array()) {
            return (
                files
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|p| resolve_scope_path(root, p))
                    .collect(),
                false,
            );
        }
    }
    (vec!["*".to_string()], false)
}

/// Resolve a tool-supplied path against the project root for scope matching.
pub fn resolve_scope_path(root: &Path, path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        path.to_string()
    } else {
        root.join(p).display().to_string()
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
