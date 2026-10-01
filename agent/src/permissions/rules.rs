//! Rule-engine internals: scope matching, scope generalization, the
//! `permissions.bml` file format, and write-back.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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

pub(super) fn rule_matches_scope(root: &Path, rule: &PermissionRule, scope: &str) -> bool {
    match &rule.scope {
        None => true,
        Some(pattern) => scope_matches(root, pattern, scope),
    }
}

/// Absolutize against the permission root first, then resolve symlinks in
/// the leading components that exist and append the rest as written. The
/// order matters: a relative rule like `dist/**` has to match before the dir
/// exists, and `incremental_canonicalize` leaves a relative path relative
/// when the leading component is missing. The root is the manager's
/// configured cwd, not the process cwd — under ACP the two differ.
fn normalize_scope_prefix(root: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else if path.is_empty() {
        root.to_path_buf()
    } else {
        root.join(p)
    };
    paths::incremental_canonicalize(&abs).unwrap_or_else(|| paths::normalize_path(&abs))
}

/// Whether a scope value looks like a filesystem path rather than bash
/// command text, a bare word, or a non-path scope (`task:...`, `*`).
/// Conservative: absolute and dot/tilde-relative spellings always count, and
/// a whitespace-free value containing a separator does too. Bash segments
/// like `rm -rf /` carry whitespace and never count, so a dir-scoped rule
/// cannot match a command line.
fn is_path_shaped(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("~/")
        || (value.contains('/') && !value.chars().any(char::is_whitespace))
}

/// A pattern with nothing left once its trailing glob is taken off covers
/// every scope: `*` and `**`, but also `/*` and `/**`, which reduce to a
/// prefix every absolute path starts with.
pub fn is_universal_scope(root: &Path, pattern: &str) -> bool {
    match pattern.strip_suffix("/**") {
        Some(prefix) => prefix.is_empty() || is_root(&normalize_scope_prefix(root, prefix)),
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
pub fn scope_matches(root: &Path, pattern: &str, value: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/**") {
        // `/**` itself: an empty prefix is the filesystem root and covers
        // every scope, bash commands included.
        let norm_prefix = if prefix.is_empty() {
            PathBuf::from("/")
        } else {
            normalize_scope_prefix(root, prefix)
        };
        // Only a genuinely root prefix (`/**`) covers every scope. A
        // directory pattern never matches a non-path value: bash command text
        // is not a location under any directory, and normalizing it against
        // the root would let dir-scoped rules silently cover commands (or,
        // after a cwd change, wrongly fail real paths).
        if is_root(&norm_prefix) {
            return true;
        }
        if !is_path_shaped(value) {
            return false;
        }
        let norm_value = normalize_scope_prefix(root, value);
        return norm_value == norm_prefix || norm_value.starts_with(&norm_prefix);
    }
    if is_universal_scope(root, pattern) {
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
// permissions.bml file format
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct ToolPermissions {
    allow: Option<ScopeSet>,
    deny: Option<ScopeSet>,
    default: Option<DefaultEffect>,
}

#[derive(serde::Deserialize)]
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
    /// Parse the BarkML permissions shape: `default = "..."`, tool blocks
    /// (`bash { allow = [...] }`), and `mcp "server" { ... }` blocks.
    pub(super) fn from_statement(doc: &barkml::Statement) -> Self {
        let mut out = Self::default();
        for child in doc.children() {
            match child.id.as_str() {
                "default" => {
                    if let Some(value) = child.get_value()
                        && let Ok(d) = serde_json::from_value(crate::bml::value_json(value))
                    {
                        out.default = Some(d);
                    }
                }
                "allow_all" => {
                    if child
                        .get_value()
                        .and_then(|v| v.as_bool())
                        .is_some_and(|b| *b)
                    {
                        out.default = Some(DefaultEffect::Allow);
                    }
                }
                "mcp" => {
                    let Some((labels, _)) = child.get_labeled() else {
                        eprintln!("permissions: mcp block requires a server label — skipping");
                        continue;
                    };
                    let Some(server_name) = labels.first().and_then(|l| l.as_string().cloned())
                    else {
                        continue;
                    };
                    parse_mcp_server_block(
                        &server_name,
                        child,
                        &mut out.mcp_rules,
                        &mut out.mcp_defaults,
                    );
                }
                _ => {
                    let json = if child.is_container() {
                        crate::bml::container_json(child)
                    } else if let Some(value) = child.get_value() {
                        crate::bml::value_json(value)
                    } else {
                        continue;
                    };
                    let Ok(tp) = serde_json::from_value::<ToolPermissions>(json) else {
                        continue;
                    };
                    if child.id.contains('.') {
                        eprintln!(
                            "permissions: tool section [{}] contains a dot — did you mean an mcp block? Skipping.",
                            child.id
                        );
                    } else {
                        out.tools.insert(child.id.clone(), tp);
                    }
                }
            }
        }
        out
    }
}

fn parse_mcp_server_block(
    server_name: &str,
    block: &barkml::Statement,
    rules: &mut Vec<PermissionRule>,
    mcp_defaults: &mut HashMap<ToolKey, DefaultEffect>,
) {
    if !is_valid_server_name(server_name) {
        eprintln!(
            "permissions: skipping [mcp.{server_name}] — invalid server name; must contain only alphanumeric characters and hyphens"
        );
        return;
    }

    for child in block.children() {
        match child.id.as_str() {
            "allow" | "deny" => {
                let effect = if child.id == "allow" {
                    Effect::Allow
                } else {
                    Effect::Deny
                };
                if let Some(value) = child.get_value() {
                    match value.as_array() {
                        Some(arr) => {
                            for item in arr {
                                if let Some(tool_name) = item.as_string() {
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
                                                tool: tool_name.as_str().into(),
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
                        None if value.as_bool() == Some(&false) => {}
                        None => {
                            eprintln!(
                                "permissions: [mcp.{server_name}] {} must be an array of tool names or false — ignoring",
                                child.id
                            );
                        }
                    }
                }
            }
            "default" => {
                if let Some(value) = child.get_value()
                    && let Ok(d) =
                        serde_json::from_value::<DefaultEffect>(crate::bml::value_json(value))
                {
                    mcp_defaults.insert(
                        ToolKey::McpServer {
                            server: server_name.into(),
                        },
                        d,
                    );
                }
            }
            _ => eprintln!(
                "permissions: unknown key {} in [mcp.{server_name}] — ignoring",
                child.id
            ),
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
    match crate::bml::parse_file_or_empty(path) {
        Ok(doc) => Some(PermissionsFileConfig::from_statement(&doc)),
        Err(e) => {
            eprintln!("permissions: {e}");
            None
        }
    }
}

/// Load permission rules from the merged global craft document's
/// `permissions` block plus the project's `.craft/permissions.bml`. A
/// missing source anywhere behaves as no rules from that source.
pub fn load_permissions(cwd: &Path) -> PermissionsConfig {
    let loaded = crate::bml::load_global();
    for (path, error) in &loaded.errors {
        eprintln!("permissions: failed to parse {}: {error}", path.display());
    }
    let mut global_perms = PermissionsFileConfig::default();
    match loaded.doc {
        Some(doc) => {
            if let Some(block) = doc.get_child("permissions", &[]) {
                global_perms = PermissionsFileConfig::from_statement(block);
            }
        }
        None => crate::bml::warn_legacy_toml(false),
    }

    let project_perms =
        read_permissions_file(&cwd.join(PROJECT_DIR).join(PERMISSIONS_FILE)).unwrap_or_default();

    build_permissions(global_perms, project_perms)
}

// ---------------------------------------------------------------------------
// Write-back
// ---------------------------------------------------------------------------

fn insert_permission_entry(
    doc: &mut barkml::Statement,
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
            let server_block = crate::bml::ensure_labeled_block(doc, "mcp", server);
            crate::bml::push_unique_string(server_block, key, tool);
        }
        ToolKey::McpServer { server } => {
            let server_block = crate::bml::ensure_labeled_block(doc, "mcp", server);
            crate::bml::upsert_assign(server_block, "default", crate::bml::string_value(key));
        }
        ToolKey::Wildcard => {
            // Wildcard rules are config-only; runtime never writes them.
            return Err(PermissionWriteError::NotATable {
                tool: "*".to_string(),
            });
        }
        ToolKey::Native(name) => {
            let tool_block =
                crate::bml::ensure_block(doc, name).ok_or(PermissionWriteError::NotATable {
                    tool: name.to_string(),
                })?;
            match scope {
                Some(s) => crate::bml::push_unique_string(tool_block, key, s),
                None => {
                    crate::bml::upsert_assign(tool_block, key, crate::bml::bool_value(true));
                }
            }
        }
    }
    Ok(())
}

/// Append a rule to `permissions.bml`. The file is regenerated from the
/// parsed AST; comments are not preserved.
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
    let mut doc = crate::bml::parse_file_or_empty(&path).map_err(PermissionWriteError::Parse)?;

    insert_permission_entry(&mut doc, tool, scope, effect)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&path, crate::bml::to_text(&doc).as_bytes())?;
    Ok(())
}
