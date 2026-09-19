//! Rule-engine internals: scope matching, scope generalization, the
//! `permissions.toml` file format, and comment-preserving write-back.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::FILE_WRITE_TOOLS;
use super::PERMISSIONS_FILE;
use super::PROJECT_DIR;
use super::{DefaultEffect, Effect, PermissionRule, PermissionTarget};
use super::{PermissionWriteError, PermissionsConfig, ToolKey};
use super::{is_valid_server_name, is_valid_wire_name};
use crate::paths;
use crate::storage::atomic::atomic_write;

/// Words that open a block the bash parser keeps as one scope. Their first
/// token names no program, so wildcarding it would cover every command the
/// block can hold. See [`generalize_bash_segment`].
const SHELL_KEYWORDS: [&str; 15] = [
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "select", "function", "time",
];

/// Whether a rule reaches this tool at all.
pub(super) fn rule_reaches(rule_key: &ToolKey, actual: &ToolKey) -> bool {
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

pub(super) fn rule_matches_scope(rule: &PermissionRule, scope: &str) -> bool {
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
pub(super) struct PermissionsFileConfig {
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

pub(super) fn build_permissions(
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

pub(super) fn read_permissions_file(path: &Path) -> Option<PermissionsFileConfig> {
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
