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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;

use crate::paths;
use crate::storage::StorageError;
use crate::storage::atomic::atomic_write;

/// Tree-sitter bash parsing that splits compound commands into per-command
/// scopes (task B.2).
pub mod bash;

pub const DEFAULT_DENY_GUIDANCE: &str =
    "Do not retry. Try a different approach or ask the user for guidance.";

pub(crate) const ASK_TIMEOUT: Duration = Duration::from_secs(60 * 30);

/// Tests assert on this exact prefix; a wording tweak here updates them in one place.
pub const PERMISSION_DENIED_PREFIX: &str = "Permission denied for";

pub const BOUNDARY_UNVERIFIABLE_PREFIX: &str = "Cannot verify project boundary for";

pub const PERMISSIONS_FILE: &str = "permissions.toml";

const PROJECT_DIR: &str = ".craft";

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

/// Words that open a block the bash parser keeps as one scope. Their first
/// token names no program, so wildcarding it would cover every command the
/// block can hold. See [`generalize_bash_segment`].
const SHELL_KEYWORDS: [&str; 15] = [
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "select", "function", "time",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone)]
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
    scope: String,
    guidance: Option<String>,
}

impl std::fmt::Display for PermissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} `{}` ({}).",
            PERMISSION_DENIED_PREFIX, self.tool, self.scope
        )?;
        if let Some(g) = &self.guidance {
            write!(f, " User guidance: {g}")
        } else {
            write!(f, " {DEFAULT_DENY_GUIDANCE}")
        }
    }
}

impl PermissionError {
    pub(crate) fn new(tool: &str, scope: &str) -> Self {
        Self {
            tool: tool.to_string(),
            scope: scope.to_string(),
            guidance: None,
        }
    }

    pub(crate) fn with_guidance(tool: &str, scope: &str, guidance: String) -> Self {
        Self {
            tool: tool.to_string(),
            scope: scope.to_string(),
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
    pub fn fork(&self) -> Self {
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

    pub fn session_rules_snapshot(&self) -> Vec<PermissionRule> {
        self.session_rules().clone()
    }

    pub fn load_session_rules(&self, rules: Vec<PermissionRule>) {
        *self.session_rules() = rules;
    }

    /// Outside-cwd paths are not blocked here. They flow through the normal
    /// permission prompt (which uses the same canonicalization via
    /// [`scope_matches`]). Only unresolvable boundaries are hard-blocked.
    pub fn boundary_block_reason(&self, path: &Path) -> Option<String> {
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

/// Whether a rule reaches this tool at all.
fn rule_reaches(rule_key: &ToolKey, actual: &ToolKey) -> bool {
    match (rule_key, actual) {
        (ToolKey::Wildcard, _) => true,
        (ToolKey::McpServer { server: rs }, ToolKey::McpTool { server: as_, .. }) => rs == as_,
        (ToolKey::Native(a), ToolKey::Native(b)) => a == b,
        (ToolKey::McpServer { server: rs }, ToolKey::McpServer { server: as_ }) => rs == as_,
        (
            ToolKey::McpTool {
                server: rs,
                tool: rt,
            },
            ToolKey::McpTool {
                server: as_,
                tool: at,
            },
        ) => rs == as_ && rt == at,
        _ => false,
    }
}

fn rule_matches_scope(rule: &PermissionRule, scope: &str) -> bool {
    match &rule.scope {
        None => true,
        Some(pattern) => scope_matches(pattern, scope),
    }
}

/// Absolutize first, then resolve symlinks in the leading components that
/// exist and append the rest as written. The order matters: a relative rule
/// like `dist/**` has to match before the dir exists, and
/// `incremental_canonicalize` leaves a relative path relative when the leading
/// component is missing.
fn normalize_scope_prefix(path: &str) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| PathBuf::from(path));
    paths::incremental_canonicalize(&abs).unwrap_or_else(|| paths::normalize_path(&abs))
}

/// A pattern with nothing left once its trailing glob is taken off covers
/// every scope: `*` and `**`, but also `/*` and `/**`, which reduce to a
/// prefix every absolute path starts with.
pub fn is_universal_scope(pattern: &str) -> bool {
    match pattern.strip_suffix("/**") {
        Some(prefix) => is_root(&normalize_scope_prefix(prefix)),
        None => {
            let stem = pattern.trim_end_matches('*');
            stem.len() < pattern.len() && matches!(stem, "" | "/")
        }
    }
}

fn is_root(path: &Path) -> bool {
    path.parent().is_none()
}

/// For the `/**` path pattern, `Path::starts_with` is used to compare
/// components rather than characters, which handles both `/` and `\\`
/// transparently on all platforms.
pub fn scope_matches(pattern: &str, value: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/**") {
        let norm_prefix = normalize_scope_prefix(prefix);
        // A root prefix covers every scope, bash commands included. Those are
        // not paths, so a plain prefix test would miss them.
        if is_root(&norm_prefix) {
            return true;
        }
        let norm_value = normalize_scope_prefix(value);
        return norm_value == norm_prefix || norm_value.starts_with(&norm_prefix);
    }
    if is_universal_scope(pattern) {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(" *") {
        return value == prefix || value.starts_with(&format!("{prefix} "));
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    pattern == value
}

pub fn normalize_scope_path(path: &str) -> String {
    paths::normalize_path(&normalize_scope_prefix(path))
        .to_string_lossy()
        .into_owned()
}

pub fn physical_boundary_check(parent: &Path, child: &Path) -> Option<bool> {
    let parent_canon = paths::incremental_canonicalize(parent)?;
    let child_canon = paths::incremental_canonicalize(child).unwrap_or_else(|| child.to_path_buf());
    Some(child_canon.starts_with(&parent_canon))
}

/// A `<cmd> *` rule is only safe when `<cmd>` names one program. Block forms
/// arrive as one scope and their first token is a shell keyword; wildcarding
/// that hands out every loop the model can write. Those scopes stay literal:
/// remembering one exact command is worth little, but it is never a blank
/// cheque.
fn generalize_bash_segment(segment: &str) -> String {
    let first_token = segment.split_whitespace().next().unwrap_or(segment);
    let is_command_word = !first_token.is_empty()
        && first_token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.:/@+-".contains(c))
        && !SHELL_KEYWORDS.contains(&first_token);
    if is_command_word {
        format!("{first_token} *")
    } else {
        segment.to_string()
    }
}

pub fn generalized_scopes(tool: &ToolKey, scopes: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    scopes
        .iter()
        .map(|s| generalize_scope(tool, s))
        .filter(|g| seen.insert(g.clone()))
        .collect()
}

fn generalize_scope(tool: &ToolKey, scope: &str) -> String {
    match tool {
        ToolKey::Native(name) if name.as_ref() == "bash" => generalize_bash_segment(scope),
        ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()) => {
            let p = Path::new(scope);
            match p.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => {
                    format!("{}/**", parent.display())
                }
                _ => "**".to_string(),
            }
        }
        // MCP tool calls have a scope equal to the JSON-stringified input.
        // "Allow always" should whitelist the tool regardless of its
        // arguments; the rule's `tool` field still gates which MCP tool it
        // applies to, keeping distinct tools distinct.
        ToolKey::McpTool { .. } | ToolKey::McpServer { .. } => "*".to_string(),
        _ => scope.to_string(),
    }
}

// ---------------------------------------------------------------------------
// permissions.toml file format
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ToolPermissions {
    allow: Option<ScopeSet>,
    deny: Option<ScopeSet>,
    default: Option<DefaultEffect>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScopeSet {
    All(bool),
    Scopes(Vec<String>),
}

#[derive(Default)]
struct PermissionsFileConfig {
    default: Option<DefaultEffect>,
    tools: HashMap<String, ToolPermissions>,
    mcp_rules: Vec<PermissionRule>,
    mcp_defaults: HashMap<ToolKey, DefaultEffect>,
}

impl PermissionsFileConfig {
    fn merge(&mut self, overlay: PermissionsFileConfig) {
        if overlay.default.is_some() {
            self.default = overlay.default;
        }
        self.tools.extend(overlay.tools);
        self.mcp_rules.extend(overlay.mcp_rules);
        self.mcp_defaults.extend(overlay.mcp_defaults);
    }
}

impl<'de> Deserialize<'de> for PermissionsFileConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let table = toml::Table::deserialize(deserializer)?;
        let default = table
            .get("default")
            .and_then(|v| DefaultEffect::deserialize(v.clone()).ok())
            .or_else(|| {
                table
                    .get("allow_all")?
                    .as_bool()?
                    .then_some(DefaultEffect::Allow)
            });

        let mut tools = HashMap::new();
        let mut mcp_rules = Vec::new();
        let mut mcp_defaults = HashMap::new();

        for (k, v) in table.iter() {
            if k.is_empty() || k == "allow_all" || k == "default" {
                continue;
            }
            if k == "mcp" {
                if let Some(mcp_table) = v.as_table() {
                    for (server_name, server_value) in mcp_table {
                        if let Some(server_table) = server_value.as_table() {
                            parse_mcp_server_table(
                                server_name,
                                server_table,
                                &mut mcp_rules,
                                &mut mcp_defaults,
                            );
                        } else {
                            eprintln!("permissions: [mcp.{server_name}] is not a table — skipping");
                        }
                    }
                } else {
                    eprintln!("permissions: [mcp] is not a table — skipping");
                }
            } else if let Ok(tp) = v.clone().try_into::<ToolPermissions>() {
                if k.contains('.') {
                    eprintln!(
                        "permissions: tool section [{k}] contains a dot — did you mean [mcp.{}]? Skipping.",
                        k.split('.').next().unwrap_or(k)
                    );
                } else {
                    tools.insert(k.clone(), tp);
                }
            }
        }

        Ok(Self {
            default,
            tools,
            mcp_rules,
            mcp_defaults,
        })
    }
}

fn parse_mcp_server_table(
    server_name: &str,
    table: &toml::Table,
    rules: &mut Vec<PermissionRule>,
    mcp_defaults: &mut HashMap<ToolKey, DefaultEffect>,
) {
    if !is_valid_server_name(server_name) {
        eprintln!(
            "permissions: skipping [mcp.{server_name}] — invalid server name; must contain only alphanumeric characters and hyphens"
        );
        return;
    }

    for (key, value) in table {
        match key.as_str() {
            "allow" | "deny" => {
                let effect = if key == "allow" {
                    Effect::Allow
                } else {
                    Effect::Deny
                };
                match value {
                    toml::Value::Array(arr) => {
                        for item in arr {
                            if let Some(tool_name) = item.as_str() {
                                if tool_name == "*" {
                                    rules.push(PermissionRule {
                                        tool: ToolKey::McpServer {
                                            server: server_name.into(),
                                        },
                                        scope: None,
                                        effect,
                                    });
                                } else if is_valid_wire_name(tool_name) {
                                    rules.push(PermissionRule {
                                        tool: ToolKey::McpTool {
                                            server: server_name.into(),
                                            tool: tool_name.into(),
                                        },
                                        scope: None,
                                        effect,
                                    });
                                } else {
                                    eprintln!(
                                        "permissions: skipping invalid MCP tool name {server_name}/{tool_name}"
                                    );
                                }
                            }
                        }
                    }
                    toml::Value::Boolean(false) => {}
                    _ => {
                        eprintln!(
                            "permissions: [mcp.{server_name}] {key} must be an array of tool names or false — ignoring"
                        );
                    }
                }
            }
            "default" => {
                if let Ok(d) = DefaultEffect::deserialize(value.clone()) {
                    mcp_defaults.insert(
                        ToolKey::McpServer {
                            server: server_name.into(),
                        },
                        d,
                    );
                }
            }
            _ => eprintln!("permissions: unknown key {key} in [mcp.{server_name}] — ignoring"),
        }
    }
}

fn push_rules(
    rules: &mut Vec<PermissionRule>,
    tools: &HashMap<String, ToolPermissions>,
    effect: Effect,
) {
    for (tool, perms) in tools {
        let scope_set = match effect {
            Effect::Deny => &perms.deny,
            Effect::Allow => &perms.allow,
        };
        let Some(scope_set) = scope_set else {
            continue;
        };
        match scope_set {
            ScopeSet::All(true) => rules.push(PermissionRule {
                tool: ToolKey::native(tool),
                scope: None,
                effect,
            }),
            ScopeSet::Scopes(scopes) => {
                for s in scopes {
                    rules.push(PermissionRule {
                        tool: ToolKey::native(tool),
                        scope: Some(s.clone()),
                        effect,
                    });
                }
            }
            ScopeSet::All(false) => {}
        }
    }
}

fn build_permissions(
    global: PermissionsFileConfig,
    project: PermissionsFileConfig,
) -> PermissionsConfig {
    let global_default = global.default.unwrap_or(DefaultEffect::Prompt);
    let default = match project.default {
        Some(DefaultEffect::Allow) => global_default,
        Some(d) => d,
        None => global_default,
    };

    let mut tool_defaults = HashMap::new();
    for (tool, perms) in &global.tools {
        if let Some(d) = perms.default {
            let key = ToolKey::native(tool);
            if matches!(key, ToolKey::Wildcard) {
                eprintln!(
                    "permissions: ignoring [\"*\"].default — use the top-level `default` field instead for global fallback behavior"
                );
            } else {
                tool_defaults.insert(key, d);
            }
        }
    }
    for (key, d) in &global.mcp_defaults {
        tool_defaults.insert(key.clone(), *d);
    }
    for (tool, perms) in &project.tools {
        if let Some(d) = perms.default
            && d != DefaultEffect::Allow
        {
            let key = ToolKey::native(tool);
            if matches!(key, ToolKey::Wildcard) {
                eprintln!(
                    "permissions: ignoring project [\"*\"].default — use the top-level `default` field instead"
                );
            } else {
                tool_defaults.insert(key, d);
            }
        }
    }
    for (key, d) in &project.mcp_defaults {
        if *d != DefaultEffect::Allow {
            tool_defaults.insert(key.clone(), *d);
        }
    }

    let mut rules = Vec::new();
    for rule in &global.mcp_rules {
        if rule.effect == Effect::Deny {
            rules.push(rule.clone());
        }
    }
    for rule in &global.mcp_rules {
        if rule.effect == Effect::Allow {
            rules.push(rule.clone());
        }
    }
    for tools in [&global.tools, &project.tools] {
        push_rules(&mut rules, tools, Effect::Deny);
        push_rules(&mut rules, tools, Effect::Allow);
    }
    for rule in &project.mcp_rules {
        if rule.effect == Effect::Deny {
            rules.push(rule.clone());
        }
    }
    for rule in &project.mcp_rules {
        if rule.effect == Effect::Allow {
            rules.push(rule.clone());
        }
    }
    PermissionsConfig {
        default,
        tool_defaults,
        rules,
    }
}

fn read_permissions_file(path: &Path) -> Option<PermissionsFileConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    match toml::from_str(&content) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("permissions: failed to parse {}: {e}", path.display());
            None
        }
    }
}

/// Load permission rules from the global config search dirs plus the
/// project's `.craft/permissions.toml`. A missing file anywhere behaves as
/// no rules from that source.
pub fn load_permissions(cwd: &Path) -> PermissionsConfig {
    let mut global_perms = PermissionsFileConfig::default();
    for dir in paths::config_search_dirs() {
        if let Some(p) = read_permissions_file(&dir.join(PERMISSIONS_FILE)) {
            global_perms.merge(p);
        }
    }

    let project_perms =
        read_permissions_file(&cwd.join(PROJECT_DIR).join(PERMISSIONS_FILE)).unwrap_or_default();

    build_permissions(global_perms, project_perms)
}

// ---------------------------------------------------------------------------
// Write-back
// ---------------------------------------------------------------------------

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

fn child_table<'a>(
    table: &'a mut toml_edit::Table,
    key: &str,
) -> Result<&'a mut toml_edit::Table, PermissionWriteError> {
    table
        .entry(key)
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or(PermissionWriteError::NotATable {
            tool: key.to_string(),
        })
}

fn push_unique(
    table: &mut toml_edit::Table,
    key: &str,
    value: &str,
) -> Result<(), PermissionWriteError> {
    let arr = table
        .entry(key)
        .or_insert_with(|| toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new())))
        .as_array_mut()
        .ok_or(PermissionWriteError::NotAnArray {
            tool: String::new(),
            key: key.to_string(),
        })?;
    if !arr.iter().any(|v| v.as_str() == Some(value)) {
        arr.push(value);
        arr.set_trailing("\n");
        arr.set_trailing_comma(true);
        for item in arr.iter_mut() {
            item.decor_mut().set_prefix("\n    ");
        }
    }
    Ok(())
}

fn insert_permission_entry(
    doc: &mut toml_edit::DocumentMut,
    tool_key: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
) -> Result<(), PermissionWriteError> {
    let key = match effect {
        Effect::Allow => "allow",
        Effect::Deny => "deny",
    };

    match tool_key {
        // MCP scopes are always wildcarded, so `scope` is ignored for MCP keys.
        ToolKey::McpTool { server, tool } => {
            let server_table = child_table(child_table(doc.as_table_mut(), "mcp")?, server)?;
            push_unique(server_table, key, tool)?;
        }
        ToolKey::McpServer { server } => {
            let server_table = child_table(child_table(doc.as_table_mut(), "mcp")?, server)?;
            server_table.insert("default", toml_edit::value(key));
        }
        ToolKey::Wildcard => {
            // Wildcard rules are config-only; runtime never writes them.
            return Err(PermissionWriteError::NotATable {
                tool: "*".to_string(),
            });
        }
        ToolKey::Native(name) => {
            let tool_table = child_table(doc.as_table_mut(), name)?;
            match scope {
                Some(s) => push_unique(tool_table, key, s)?,
                None => {
                    tool_table.insert(key, toml_edit::value(true));
                }
            }
        }
    }
    Ok(())
}

/// Append a rule to `permissions.toml`, preserving the existing file's
/// comments and formatting (`toml_edit`).
pub fn append_permission_rule(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    target: &PermissionTarget,
) -> Result<(), PermissionWriteError> {
    let path = match target {
        PermissionTarget::Global => paths::config_search_dirs()
            .into_iter()
            .next_back()
            .map(|dir| dir.join(PERMISSIONS_FILE))
            .ok_or(PermissionWriteError::Io(std::io::Error::other(
                "no config directory available",
            )))?,
        PermissionTarget::Project(cwd) => cwd.join(PROJECT_DIR).join(PERMISSIONS_FILE),
    };
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e: toml_edit::TomlError| PermissionWriteError::Parse(e.to_string()))?;

    insert_permission_entry(&mut doc, tool, scope, effect)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&path, doc.to_string().as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow_rule(tool: &str, scope: Option<&str>) -> PermissionRule {
        PermissionRule {
            tool: ToolKey::native(tool),
            scope: scope.map(str::to_string),
            effect: Effect::Allow,
        }
    }

    fn deny_rule(tool: &str, scope: Option<&str>) -> PermissionRule {
        PermissionRule {
            tool: ToolKey::native(tool),
            scope: scope.map(str::to_string),
            effect: Effect::Deny,
        }
    }

    fn mgr_with(cwd: &Path, rules: Vec<PermissionRule>) -> PermissionManager {
        PermissionManager::new(
            PermissionsConfig {
                rules,
                ..Default::default()
            },
            cwd.to_path_buf(),
        )
    }

    fn needs_prompt(check: &PermissionCheck) -> bool {
        matches!(check, PermissionCheck::NeedsPrompt { .. })
    }

    #[test]
    fn auto_review_toggles_and_forks() {
        let mgr = mgr_with(std::env::temp_dir().as_ref(), Vec::new());
        assert!(!mgr.is_auto_review());
        assert!(mgr.toggle_auto_review());
        assert!(mgr.is_auto_review());
        assert!(!mgr.toggle_auto_review());
        assert!(!mgr.is_auto_review());
        mgr.toggle_auto_review();
        assert!(mgr.fork().is_auto_review(), "fork carries the mode");
    }

    #[test]
    fn apply_auto_review_allow_records_allow_rule() {
        let mgr = mgr_with(std::env::temp_dir().as_ref(), Vec::new());
        let tool = ToolKey::native("write");
        let scopes = vec!["/tmp/a".to_string()];
        assert!(mgr.apply_auto_review(&tool, &scopes, true));
        assert!(
            matches!(mgr.check(&tool, &scopes), PermissionCheck::Allowed),
            "an allow decision must not prompt again"
        );
    }

    #[test]
    fn apply_auto_review_deny_blocks_later_calls() {
        let mgr = mgr_with(std::env::temp_dir().as_ref(), Vec::new());
        let tool = ToolKey::native("bash");
        let scopes = vec!["rm -rf /".to_string()];
        assert!(!mgr.apply_auto_review(&tool, &scopes, false));
        assert!(matches!(mgr.check(&tool, &scopes), PermissionCheck::Denied));
    }

    #[test]
    fn check_multi_force_prompt_skips_allow_rules() {
        let mgr = mgr_with(
            Path::new("/tmp"),
            vec![
                allow_rule("bash", Some("cargo *")),
                allow_rule("bash", Some("git *")),
            ],
        );
        let tool = ToolKey::native("bash");
        let scopes = vec!["cargo test".to_string(), "git push".to_string()];
        assert!(matches!(
            mgr.check_multi(&tool, &scopes, false),
            PermissionCheck::Allowed
        ));
        match mgr.check_multi(&tool, &scopes, true) {
            PermissionCheck::NeedsPrompt {
                scopes: s,
                force_prompt,
                ..
            } => {
                assert_eq!(s, vec!["cargo test", "git push"]);
                assert!(force_prompt);
            }
            other => panic!("expected NeedsPrompt, got {other:?}"),
        }
    }

    #[test]
    fn check_multi_deny_wins_over_force_prompt() {
        let mgr = mgr_with(Path::new("/tmp"), vec![deny_rule("bash", Some("rm *"))]);
        assert!(matches!(
            mgr.check_multi(&ToolKey::native("bash"), &["rm -rf /".to_string()], true),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn check_multi_partial_coverage_prompts_uncovered() {
        let mgr = mgr_with(Path::new("/tmp"), vec![allow_rule("bash", Some("cargo *"))]);
        match mgr.check_multi(
            &ToolKey::native("bash"),
            &[
                "cargo test".to_string(),
                "git push".to_string(),
                "ls".to_string(),
            ],
            false,
        ) {
            PermissionCheck::NeedsPrompt { scopes, .. } => {
                assert_eq!(scopes, vec!["git push", "ls"]);
            }
            other => panic!("expected NeedsPrompt, got {other:?}"),
        }
    }

    #[test]
    fn scope_matches_prefix_token_boundary() {
        assert!(scope_matches("git *", "git diff"));
        assert!(scope_matches("git *", "git"));
        assert!(!scope_matches("pwd *", "pwdx /"));
        assert!(scope_matches("prefix*", "prefix-thing"));
        assert!(scope_matches("exact", "exact"));
        assert!(!scope_matches("exact", "exactly"));
    }

    #[test]
    fn universal_scopes_match_everything() {
        for pattern in ["*", "**", "/*", "/**"] {
            assert!(is_universal_scope(pattern), "{pattern}");
            assert!(scope_matches(pattern, "/anything/at/all"), "{pattern}");
            assert!(scope_matches(pattern, "git push"), "{pattern}");
        }
        assert!(!is_universal_scope("/tmp/**"));
        assert!(scope_matches("/tmp/**", "/tmp/a/b"));
        assert!(!scope_matches("/tmp/**", "/var/tmp"));
    }

    #[test]
    fn dir_glob_matches_before_dir_exists() {
        // Absolutization happens before symlink resolution, so a rule for a
        // directory that does not exist yet still matches paths under it.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("dist");
        assert!(scope_matches(
            &format!("{}/**", base.display()),
            &base.join("foo.txt").display().to_string()
        ));
    }

    #[test]
    fn deny_beats_allow_and_session_beats_default() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = mgr_with(
            tmp.path(),
            vec![
                allow_rule("bash", Some("git *")),
                deny_rule("bash", Some("git push *")),
            ],
        );
        let tool = ToolKey::native("bash");
        assert!(matches!(
            mgr.check(&tool, &["git diff".to_string()]),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(&tool, &["git push origin main".to_string()]),
            PermissionCheck::Denied
        ));

        // Unknown scope falls to the default (prompt).
        assert!(needs_prompt(&mgr.check(&tool, &["rm -rf /".to_string()])));
    }

    #[test]
    fn tool_default_changes_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = PermissionManager::new(
            PermissionsConfig {
                tool_defaults: HashMap::from([(ToolKey::native("glob"), DefaultEffect::Allow)]),
                ..Default::default()
            },
            tmp.path().to_path_buf(),
        );
        let tool = ToolKey::native("glob");
        assert!(matches!(
            mgr.check(&tool, &["**/*.rs".to_string()]),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn session_rules_allow_without_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = mgr_with(tmp.path(), vec![]);
        let tool = ToolKey::native("write");
        let scopes = vec!["src/lib.rs".to_string()];
        assert!(needs_prompt(&mgr.check(&tool, &scopes)));

        mgr.apply_decision(&tool, &scopes, &PermissionAnswer::AllowSession);
        assert!(matches!(
            mgr.check(&tool, &scopes),
            PermissionCheck::Allowed
        ));

        // A different directory still prompts: the session grant generalized
        // to the parent dir (`src/**`), not to every file.
        assert!(needs_prompt(
            &mgr.check(&tool, &["docs/other.rs".to_string()])
        ));
    }

    #[test]
    fn apply_decision_persists_always_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = mgr_with(tmp.path(), vec![]);
        let tool = ToolKey::native("write");
        let scopes = vec!["/abs/proj/src/lib.rs".to_string()];

        let persist = mgr.apply_decision(&tool, &scopes, &PermissionAnswer::AllowAlwaysGlobal);
        assert_eq!(persist.len(), 1);
        assert_eq!(persist[0].0, tool);
        assert_eq!(persist[0].2, Effect::Allow);
        assert!(matches!(persist[0].3, PermissionTarget::Global));

        // The generalization for write tools is the parent dir glob.
        assert_eq!(persist[0].1.as_deref(), Some("/abs/proj/src/**"));

        let deny = mgr.apply_decision(&tool, &scopes, &PermissionAnswer::DenyAlwaysLocal);
        assert_eq!(deny.len(), 1);
        assert!(matches!(deny[0].3, PermissionTarget::Project(_)));
        assert_eq!(deny[0].2, Effect::Deny);

        let once = mgr.apply_decision(&tool, &scopes, &PermissionAnswer::AllowOnce);
        assert!(once.is_empty());
    }

    #[test]
    fn answer_encode_decode_roundtrip() {
        let answers = [
            PermissionAnswer::AllowOnce,
            PermissionAnswer::AllowSession,
            PermissionAnswer::AllowAlwaysLocal,
            PermissionAnswer::AllowAlwaysGlobal,
            PermissionAnswer::Deny,
            PermissionAnswer::DenyWithGuidance("use git instead".into()),
            PermissionAnswer::DenyAlwaysLocal,
            PermissionAnswer::DenyAlwaysGlobal,
        ];
        for a in &answers {
            assert_eq!(PermissionAnswer::decode(&a.encode()).as_ref(), Some(a));
        }
        assert_eq!(PermissionAnswer::decode("nope"), None);
        assert_eq!(
            PermissionAnswer::decode("deny:"),
            Some(PermissionAnswer::Deny)
        );
    }

    #[test]
    fn bash_generalization_respects_shell_keywords() {
        let tool = ToolKey::native("bash");
        assert_eq!(
            generalized_scopes(&tool, &["git push origin main".to_string()]),
            vec!["git *".to_string()]
        );
        // Shell keywords stay literal: no blank cheque over every loop body.
        assert_eq!(
            generalized_scopes(&tool, &["for f in *.rs; do wc -l $f; done".to_string()]),
            vec!["for f in *.rs; do wc -l $f; done".to_string()]
        );
        // A command word with odd-but-legal chars still generalizes.
        assert_eq!(
            generalized_scopes(&tool, &["cargo-nextest run".to_string()]),
            vec!["cargo-nextest *".to_string()]
        );
    }

    #[test]
    fn boundary_check_detects_escapes_and_unresolvable_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let inside = tmp.path().join("src/lib.rs");
        assert_eq!(physical_boundary_check(tmp.path(), &inside), Some(true));
        let outside = std::env::temp_dir().join("elsewhere.txt");
        assert_eq!(physical_boundary_check(tmp.path(), &outside), Some(false));

        // An empty parent path cannot be resolved: the boundary is
        // unverifiable, which is the hard-block case.
        assert_eq!(physical_boundary_check(Path::new(""), &inside), None);

        // Resolvable boundaries produce a verdict rather than a block; an
        // outside-cwd path flows through the normal prompt instead.
        let mgr = mgr_with(tmp.path(), vec![]);
        assert_eq!(mgr.boundary_block_reason(&inside), None);
        assert_eq!(mgr.boundary_block_reason(&outside), None);
    }

    #[test]
    fn write_back_preserves_comments_and_appends_unique() {
        let tmp = tempfile::tempdir().unwrap();
        let project = PermissionTarget::Project(tmp.path().to_path_buf());
        let path = tmp.path().join(PROJECT_DIR).join(PERMISSIONS_FILE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "# my carefully written comment\n[write]\nallow = [\n    \"src/**\",\n]\n",
        )
        .unwrap();

        append_permission_rule(
            &ToolKey::native("write"),
            Some("docs/**"),
            Effect::Allow,
            &project,
        )
        .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("# my carefully written comment"),
            "{content}"
        );
        assert!(content.contains("\"docs/**\""), "{content}");

        // Appending an existing scope is a no-op.
        append_permission_rule(
            &ToolKey::native("write"),
            Some("src/**"),
            Effect::Allow,
            &project,
        )
        .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.matches("\"src/**\"").count(), 1, "{content}");
    }

    #[test]
    fn write_back_creates_missing_file_and_denies() {
        let tmp = tempfile::tempdir().unwrap();
        append_permission_rule(
            &ToolKey::native("bash"),
            Some("git *"),
            Effect::Deny,
            &PermissionTarget::Project(tmp.path().to_path_buf()),
        )
        .unwrap();
        let content =
            std::fs::read_to_string(tmp.path().join(PROJECT_DIR).join(PERMISSIONS_FILE)).unwrap();
        assert!(content.contains("[bash]"), "{content}");
        assert!(content.contains("deny = [\n    \"git *\",\n]"), "{content}");
    }

    #[test]
    fn write_back_never_writes_wildcard() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            append_permission_rule(
                &ToolKey::Wildcard,
                None,
                Effect::Allow,
                &PermissionTarget::Project(tmp.path().to_path_buf()),
            ),
            Err(PermissionWriteError::NotATable { .. })
        ));
    }

    #[test]
    fn load_permissions_reads_global_and_project_files() {
        let tmp = tempfile::tempdir().unwrap();
        let craft_dir = tmp.path().join(PROJECT_DIR);
        std::fs::create_dir_all(&craft_dir).unwrap();
        std::fs::write(
            craft_dir.join(PERMISSIONS_FILE),
            "[bash]\ndeny = [\"rm *\"]\n\n[write]\nallow = true\n",
        )
        .unwrap();

        let config = load_permissions_inner_for_test(tmp.path());
        let bash = ToolKey::native("bash");
        let mgr = PermissionManager::new(config, tmp.path().to_path_buf());
        assert!(matches!(
            mgr.check(&bash, &["rm -rf /tmp/x".to_string()]),
            PermissionCheck::Denied
        ));
        let write = ToolKey::native("write");
        assert!(matches!(
            mgr.check(&write, &["anything.rs".to_string()]),
            PermissionCheck::Allowed
        ));
    }

    /// Test seam: project file only (global dirs come from the environment).
    fn load_permissions_inner_for_test(cwd: &Path) -> PermissionsConfig {
        let project = read_permissions_file(&cwd.join(PROJECT_DIR).join(PERMISSIONS_FILE))
            .unwrap_or_default();
        build_permissions(PermissionsFileConfig::default(), project)
    }

    #[test]
    fn missing_permissions_file_is_empty_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config = load_permissions_inner_for_test(tmp.path());
        assert!(config.rules.is_empty());
        assert_eq!(config.default, DefaultEffect::Prompt);
    }

    #[test]
    fn corrupt_permissions_file_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let craft_dir = tmp.path().join(PROJECT_DIR);
        std::fs::create_dir_all(&craft_dir).unwrap();
        std::fs::write(craft_dir.join(PERMISSIONS_FILE), "not [ valid toml").unwrap();
        let config = load_permissions_inner_for_test(tmp.path());
        assert!(config.rules.is_empty());
    }

    #[test]
    fn fork_starts_with_empty_session_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = mgr_with(tmp.path(), vec![allow_rule("write", None)]);
        mgr.add_session_rule(allow_rule("write", Some("a.rs")));
        let forked = mgr.fork();
        assert!(forked.session_rules_snapshot().is_empty());
        // Config rules survive the fork.
        let tool = ToolKey::native("write");
        assert!(matches!(
            forked.check(&tool, &["a.rs".to_string()]),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn mcp_rules_match_by_server_and_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = PermissionManager::new(
            PermissionsConfig {
                rules: vec![PermissionRule {
                    tool: ToolKey::McpServer {
                        server: "github".into(),
                    },
                    scope: None,
                    effect: Effect::Allow,
                }],
                ..Default::default()
            },
            tmp.path().to_path_buf(),
        );
        let tool = ToolKey::McpTool {
            server: "github".into(),
            tool: "create_issue".into(),
        };
        assert!(matches!(
            mgr.check(&tool, &["{}".to_string()]),
            PermissionCheck::Allowed
        ));
        let other = ToolKey::McpTool {
            server: "gitlab".into(),
            tool: "create_issue".into(),
        };
        assert!(needs_prompt(&mgr.check(&other, &["{}".to_string()])));
    }

    #[test]
    fn permission_error_display_has_prefix_and_guidance() {
        let e = PermissionError::new("bash", "rm -rf /");
        let msg = e.to_string();
        assert!(msg.starts_with(PERMISSION_DENIED_PREFIX), "{msg}");
        assert!(msg.contains(DEFAULT_DENY_GUIDANCE), "{msg}");

        let e = PermissionError::with_guidance("bash", "rm -rf /", "use git clean".into());
        assert!(e.to_string().contains("User guidance: use git clean"));
    }
}
