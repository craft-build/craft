//! craft-acp's vendor extension surface (`_craft/*` JSON-RPC methods).
//!
//! The ACP spec reserves method names starting with `_` for vendor extensions
//! (`ExtRequest` / `ExtNotification`); clients that don't understand them MUST
//! ignore unknown methods. All craft-specific methods live under the `_craft/`
//! prefix so third-party ACP clients keep working.
//!
//! The registry here is the single source of truth for what the desktop
//! command palette can show: it enumerates builtins (mirroring the TUI's
//! `BUILTIN_COMMANDS`) plus project/user custom commands discovered from the
//! session cwd. Handlers for the methods themselves live in [`handlers`] and
//! are wired into `server::handle_request`.

use std::path::Path;

use agent_client_protocol_schema::Error as AcpError;
use craft_agent::AgentInput;
use craft_agent::command::{self, CommandScope};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::server::{self, SessionState};
use crate::translate;

/// How the desktop client should dispatch a given command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStrategy {
    /// Maps to a standard ACP method (`session/set_mode`, etc.).
    AcpStandard,
    /// Maps to a `_craft/*` request.
    CraftRequest,
    /// Unknown `/foo`: send the raw text as `session/prompt`.
    Passthrough,
    /// Desktop-only client action (`/help`, `/clear`); never reaches the server.
    Client,
}

/// One entry in the palette. Mirrors the shape of `craft-ui`'s
/// `BuiltinCommand` plus the strategy + category metadata the desktop needs
/// to route without re-implementing command parsing.
///
/// Serialized `camelCase` to match the desktop TS interface.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub max_args: usize,
    pub strategy: CommandStrategy,
    pub category: &'static str,
}

/// A discovered project/user custom command, ready for the palette.
/// Subset of [`CustomCommand`] with the routing metadata filled in.
///
/// Serialized `camelCase` to match the desktop TS interface.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomCommandDescriptor {
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub accepts_args: bool,
    pub scope: &'static str,
}

const CATEGORY_SESSION: &str = "session";
const CATEGORY_HISTORY: &str = "history";
const CATEGORY_NAV: &str = "navigation";
const CATEGORY_TOOLS: &str = "tools";
const CATEGORY_CONFIG: &str = "config";
const CATEGORY_KNOWLEDGE: &str = "knowledge";
const CATEGORY_CLIENT: &str = "client";

/// The builtin command table. This is a **desktop-relevant subset** of
/// `craft-ui/src/components/command.rs::BUILTIN_COMMANDS` — the two surfaces
/// have different command sets (the TUI exposes `/tasks`, `/usage`, `/theme`,
/// etc. that are TUI-only; the desktop reaches things through ACP instead).
/// When adding a command that should appear in both surfaces, add it to both
/// tables; when adding a desktop-only or TUI-only command, only touch the
/// relevant one.
///
/// Note: `/memory` is special. In the TUI it is a Lua-plugin dispatch (see
/// `plugins/memory.lua`); on the desktop it routes through the server as
/// `_craft/memory/list`. They are intentionally distinct implementations
/// sharing only the slash name.
///
/// `strategy` records how the desktop dispatches each one. Commands marked
/// `Client` never reach the server; the rest map to either a standard ACP
/// call or a `_craft/*` request (see [`method_for`]).
const BUILTIN_COMMANDS: &[CommandDescriptor] = &[
    CommandDescriptor {
        name: "/compact",
        description: "Summarize and compact conversation history",
        max_args: 0,
        strategy: CommandStrategy::CraftRequest,
        category: CATEGORY_HISTORY,
    },
    CommandDescriptor {
        name: "/btw",
        description: "Ask a quick question (no tools, no history pollution)",
        max_args: usize::MAX,
        strategy: CommandStrategy::CraftRequest,
        category: CATEGORY_HISTORY,
    },
    CommandDescriptor {
        name: "/new",
        description: "Start a new session",
        max_args: 0,
        strategy: CommandStrategy::AcpStandard,
        category: CATEGORY_SESSION,
    },
    CommandDescriptor {
        name: "/help",
        description: "Show keybindings",
        max_args: 0,
        strategy: CommandStrategy::Client,
        category: CATEGORY_CLIENT,
    },
    CommandDescriptor {
        name: "/clear",
        description: "Clear the current conversation view",
        max_args: 0,
        strategy: CommandStrategy::Client,
        category: CATEGORY_CLIENT,
    },
    CommandDescriptor {
        name: "/cd",
        description: "Change working directory",
        max_args: 1,
        strategy: CommandStrategy::CraftRequest,
        category: CATEGORY_NAV,
    },
    CommandDescriptor {
        name: "/mode",
        description: "Switch agent mode (build, plan)",
        max_args: 1,
        strategy: CommandStrategy::AcpStandard,
        category: CATEGORY_CONFIG,
    },
    CommandDescriptor {
        name: "/yolo",
        description: "Toggle YOLO mode (skip all permission prompts)",
        max_args: 0,
        strategy: CommandStrategy::AcpStandard,
        category: CATEGORY_CONFIG,
    },
    CommandDescriptor {
        name: "/model",
        description: "Switch model",
        max_args: 0,
        strategy: CommandStrategy::AcpStandard,
        category: CATEGORY_CONFIG,
    },
    CommandDescriptor {
        name: "/dream",
        description: "Consolidate and curate project memory",
        max_args: 0,
        strategy: CommandStrategy::CraftRequest,
        category: CATEGORY_KNOWLEDGE,
    },
    CommandDescriptor {
        name: "/distill",
        description: "Discover reusable workflows and propose skills",
        max_args: 0,
        strategy: CommandStrategy::CraftRequest,
        category: CATEGORY_KNOWLEDGE,
    },
    CommandDescriptor {
        name: "/checkpoint",
        description: "Write a session checkpoint for smooth resume",
        max_args: 0,
        strategy: CommandStrategy::CraftRequest,
        category: CATEGORY_KNOWLEDGE,
    },
    CommandDescriptor {
        name: "/wiki",
        description: "Init the project wiki, ingest a file, list entries, or show a page",
        max_args: usize::MAX,
        strategy: CommandStrategy::CraftRequest,
        category: CATEGORY_KNOWLEDGE,
    },
    CommandDescriptor {
        name: "/map",
        description: "Show the current repo map (ranked symbol context)",
        max_args: 0,
        strategy: CommandStrategy::CraftRequest,
        category: CATEGORY_KNOWLEDGE,
    },
    CommandDescriptor {
        name: "/map-refresh",
        description: "Force rebuild the repo map cache",
        max_args: 0,
        strategy: CommandStrategy::CraftRequest,
        category: CATEGORY_KNOWLEDGE,
    },
    CommandDescriptor {
        name: "/map-toggle",
        description: "Toggle repo map injection on/off",
        max_args: 0,
        strategy: CommandStrategy::CraftRequest,
        category: CATEGORY_KNOWLEDGE,
    },
    CommandDescriptor {
        name: "/memory",
        description: "List craft memory notes for this project",
        max_args: 0,
        strategy: CommandStrategy::CraftRequest,
        category: CATEGORY_KNOWLEDGE,
    },
    CommandDescriptor {
        name: "/recipe",
        description: "Browse and run a recipe",
        max_args: 0,
        strategy: CommandStrategy::CraftRequest,
        category: CATEGORY_TOOLS,
    },
];

/// `_craft/listCommands` -> `{ commands: [...], custom: [...] }`.
///
/// `commands` is the builtin table; `custom` enumerates the project/user
/// commands discovered from `cwd` (so the client can render `/project:foo`
/// entries without re-implementing discovery).
pub fn list_commands(cwd: &Path) -> Value {
    json!({
        "commands": builtins_with_methods(),
        "custom": custom_commands(cwd),
    })
}

/// Discover project/user custom commands for `cwd`, shaped for the palette.
/// Shared by `_craft/listCommands` and `_craft/command/list`.
fn custom_commands(cwd: &Path) -> Vec<CustomCommandDescriptor> {
    command::discover_commands(cwd)
        .into_iter()
        .map(|c| CustomCommandDescriptor {
            display_name: c.display_name(),
            accepts_args: c.has_args(),
            name: c.name.clone(),
            description: c.description.clone(),
            scope: match c.scope {
                CommandScope::Project => "project",
                CommandScope::User => "user",
            },
        })
        .collect()
}

/// Map a user-typed builtin name (e.g. `/compact`) to the `_craft/*` method
/// the desktop should call. Returns `None` for builtins that don't have a
/// server-side `_craft/*` target (those are `AcpStandard` or `Client`).
///
/// The `method` field on the serialized [`CommandDescriptor`] carries this
/// same mapping inline so the desktop can dispatch purely from
/// `_craft/listCommands` output without re-implementing the table.
pub fn method_for(name: &str) -> Option<&'static str> {
    Some(match name {
        "/compact" => "_craft/session/compact",
        "/btw" => "_craft/session/btw",
        "/cd" => "_craft/session/cd",
        "/clear" => "_craft/session/clear",
        "/dream" | "/distill" | "/checkpoint" | "/wiki" => "_craft/meta/prompt",
        "/map" => "_craft/map/show",
        "/map-refresh" => "_craft/map/refresh",
        "/map-toggle" => "_craft/map/toggle",
        "/memory" => "_craft/memory/list",
        "/recipe" => "_craft/recipe/list",
        _ => return None,
    })
}

/// For meta-prompt builtins (all routed through `_craft/meta/prompt`), the
/// `kind` value the server expects in params. Mirrors the keys of
/// [`META_PROMPTS`]. The serialized descriptor carries this as `meta_kind` so
/// the client doesn't need its own copy of the name-to-kind table.
fn meta_kind_for(name: &str) -> Option<&'static str> {
    Some(match name {
        "/dream" => "dream",
        "/distill" => "distill",
        "/checkpoint" => "checkpoint",
        "/wiki" => "wiki_init",
        _ => return None,
    })
}

/// Serialize a `CommandDescriptor` with its `_craft/*` method (and, for
/// meta-prompt commands, the `kind`) attached, so the client can route
/// without keeping its own copy of the [`method_for`] / [`meta_kind_for`]
/// tables in sync.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandDescriptorWithMethod<'a> {
    #[serde(flatten)]
    inner: &'a CommandDescriptor,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    meta_kind: Option<&'static str>,
}

/// Builtins table, exposed for tests and for callers that want to enumerate
/// without the custom-command discovery side effect.
pub fn builtins() -> &'static [CommandDescriptor] {
    BUILTIN_COMMANDS
}

/// Builtins with the `_craft/*` method each one routes to, for serialization
/// to the desktop client. The client uses `method` (and `meta_kind` for
/// `_craft/meta/prompt` routes) directly instead of re-implementing
/// [`method_for`] / [`meta_kind_for`].
fn builtins_with_methods() -> Vec<CommandDescriptorWithMethod<'static>> {
    BUILTIN_COMMANDS
        .iter()
        .map(|inner| CommandDescriptorWithMethod {
            inner,
            method: method_for(inner.name),
            meta_kind: meta_kind_for(inner.name),
        })
        .collect()
}

/// Params for `_craft/command/run`. `name` is the display name
/// (`/project:foo` or `/user:foo`).
#[derive(Debug, Deserialize)]
pub struct CommandRunParams {
    pub name: String,
    #[serde(default)]
    pub args: String,
}

/// Look up a custom command by display name and substitute `$ARGUMENTS`.
/// Returns the expanded prompt text ready to feed to `session/prompt`.
pub fn expand_custom(cwd: &Path, name: &str, args: &str) -> Option<String> {
    let cmds = command::discover_commands(cwd);
    cmds.into_iter()
        .find(|c| c.display_name() == name)
        .map(|c| c.render(args))
}

// ---------------------------------------------------------------------------
// Dispatch + handlers
// ---------------------------------------------------------------------------

const META_PROMPTS: &[(&str, &str)] = &[
    ("dream", craft_agent::prompt::DREAM_PROMPT),
    ("distill", craft_agent::prompt::DISTILL_PROMPT),
    ("checkpoint", craft_agent::prompt::CHECKPOINT_PROMPT),
    ("wiki_init", craft_agent::prompt::WIKI_INIT_PROMPT),
];

// Params for `_craft/*` methods. The desktop sends `{ sessionId: string }`
// alongside any method-specific fields; `sessionId` is intentionally not
// modeled because the server already tracks the active session — serde
// ignores the unknown field.

#[derive(Debug, Deserialize)]
pub struct CdParams {
    pub cwd: String,
}

#[derive(Debug, Deserialize)]
pub struct MetaPromptParams {
    pub kind: String,
}

#[derive(Debug, Deserialize)]
pub struct WikiShowParams {
    pub slug: String,
}

/// Entry point for every `_craft/*` request. The server's `handle_request`
/// routes any method whose name starts with `_craft/` here.
pub(crate) async fn dispatch(
    srv: &mut server::Server,
    method: &str,
    raw: &Value,
) -> Result<Value, AcpError> {
    match method {
        "_craft/listCommands" => Ok(list_commands(server_session_cwd(srv))),
        "_craft/command/run" => handle_command_run(srv, raw),
        "_craft/session/clear" => Ok(json!({ "ok": true })),
        "_craft/session/cd" => handle_cd(srv, raw),
        "_craft/session/compact" => Err(unimplemented(
            method,
            "compact requires a headless API addition; tracked for v2",
        )),
        "_craft/session/btw" => Err(unimplemented(
            method,
            "btw requires a headless API addition; tracked for v2",
        )),
        "_craft/meta/prompt" => handle_meta_prompt(srv, raw),
        "_craft/wiki/list" => Ok(wiki_list(server_session_cwd(srv))),
        "_craft/wiki/show" => handle_wiki_show(srv, raw),
        "_craft/map/show" => Ok(map_show(server_session_cwd(srv))),
        "_craft/map/refresh" => Ok(map_refresh(server_session_cwd(srv))),
        "_craft/map/toggle" => {
            Ok(json!({ "ok": true, "note": "map toggle is client-state in v1" }))
        }
        "_craft/memory/list" => Ok(memory_list(server_session_cwd(srv))),
        "_craft/memory/read" => Err(unimplemented(method, "deferred to v2")),
        "_craft/recipe/list" => Ok(recipe_list(server_session_cwd(srv))),
        _ => Err(AcpError::method_not_found()),
    }
}

fn server_session_cwd(srv: &server::Server) -> &Path {
    srv.session_cwd().unwrap_or_else(|| Path::new("."))
}

fn unimplemented(method: &str, detail: &str) -> AcpError {
    AcpError::internal_error().data(json!(format!(
        "{method} not implemented over ACP: {detail}"
    )))
}

fn handle_command_run(srv: &mut server::Server, raw: &Value) -> Result<Value, AcpError> {
    let params: CommandRunParams = server::parse_params(raw)?;
    let cwd = server_session_cwd(srv).to_path_buf();
    let expanded = expand_custom(&cwd, &params.name, &params.args).ok_or_else(|| {
        AcpError::invalid_params().data(json!(format!("unknown custom command: {}", params.name)))
    })?;

    let out_tx = srv.out_tx.clone();
    let session = srv.session_mut().ok_or_else(server::no_session)?;
    send_prompt_input(session, expanded, &out_tx)?;
    Ok(json!({ "ok": true }))
}

/// Change the session's working directory.
///
/// Threat model: like the TUI's `/cd` (see `craft-ui::app::cmd_cd`), this
/// accepts any existing directory — there is no project-root allowlist. The
/// session `cwd` scopes `craft-sandbox`, so a caller that can drive this
/// method effectively moves the sandbox boundary. The method is therefore
/// only safe to expose to trusted clients (the desktop), which forward only
/// user-typed paths. If a future code path lets the model emit
/// `_craft/session/cd`, revisit this before shipping that path.
fn handle_cd(srv: &mut server::Server, raw: &Value) -> Result<Value, AcpError> {
    let params: CdParams = server::parse_params(raw)?;
    let resolved = craft_storage::paths::canonicalize_clean(Path::new(&params.cwd));
    if !resolved.is_dir() {
        return Err(AcpError::invalid_params()
            .data(json!(format!("not a directory: {}", resolved.display()))));
    }
    if let Some(session) = srv.session_mut() {
        session.cwd = resolved.clone();
    }
    Ok(json!({ "cwd": resolved }))
}

fn handle_meta_prompt(srv: &mut server::Server, raw: &Value) -> Result<Value, AcpError> {
    let params: MetaPromptParams = server::parse_params(raw)?;
    let prompt = META_PROMPTS
        .iter()
        .find(|(k, _)| *k == params.kind.as_str())
        .map(|(_, p)| *p)
        .ok_or_else(|| {
            AcpError::invalid_params()
                .data(json!(format!("unknown meta prompt kind: {}", params.kind)))
        })?;

    let out_tx = srv.out_tx.clone();
    let session = srv.session_mut().ok_or_else(server::no_session)?;
    send_prompt_input(session, prompt.to_string(), &out_tx)?;
    Ok(json!({ "ok": true }))
}

/// Push `text` into the agent's input channel, mirroring what `handle_prompt`
/// does for `session/prompt`. The desktop observes the resulting assistant
/// turn through the normal `session/update` stream — no separate event flow.
///
/// Mirrors `handle_prompt`'s two invariants: reject if another prompt is
/// already in flight for this session, and surface a closed input channel as
/// `internal_error("session ended")` instead of silently dropping the input.
fn send_prompt_input(
    session: &mut SessionState,
    text: String,
    out_tx: &flume::Sender<Value>,
) -> Result<(), AcpError> {
    if session
        .pending_prompt
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
    {
        return Err(AcpError::invalid_request()
            .data(json!("a prompt is already in progress for this session")));
    }

    let sid = agent_client_protocol_schema::SessionId::from(session.handle.session_id.to_string());
    server::session_update(out_tx, &sid, translate::user_message_chunk(&text));

    let input = AgentInput {
        message: text,
        mode: session.current_mode.clone(),
        images: Vec::new(),
        ..Default::default()
    };
    session
        .handle
        .input_tx
        .send(input)
        .map_err(|_| AcpError::internal_error().data(json!("session ended")))?;
    Ok(())
}

fn wiki_list(cwd: &Path) -> Value {
    match craft_storage::wiki::WikiStore::open(cwd) {
        Ok(store) => match store.list() {
            Ok(list) => json!({ "entries": list }),
            Err(e) => json!({ "error": e.to_string() }),
        },
        Err(e) => json!({ "error": e.to_string() }),
    }
}

fn handle_wiki_show(srv: &mut server::Server, raw: &Value) -> Result<Value, AcpError> {
    let params: WikiShowParams = server::parse_params(raw)?;
    let store = craft_storage::wiki::WikiStore::open(server_session_cwd(srv))
        .map_err(|e| AcpError::internal_error().data(json!(e.to_string())))?;
    let body = store.read_page(&params.slug).map_err(|e| {
        AcpError::resource_not_found(Some(params.slug.clone())).data(json!(e.to_string()))
    })?;
    Ok(json!({ "slug": params.slug, "body": body }))
}

fn map_show(cwd: &Path) -> Value {
    let Some(root) = git_root(cwd) else {
        return json!({ "body": "", "note": "no .git root found for repo map" });
    };
    let repo = craft_repomap::RepoMap::new(root);
    let body = repo.get_repo_map(&[], &[], "");
    json!({ "body": body })
}

fn map_refresh(cwd: &Path) -> Value {
    if let Some(root) = git_root(cwd) {
        craft_repomap::RepoMap::new(root).force_refresh();
    }
    json!({ "ok": true })
}

/// Walk up from `cwd` looking for the first directory containing a `.git`
/// marker. Mirrors `craft_repomap::RepoMap::try_from_cwd` but rooted at an
/// arbitrary path rather than the process cwd.
fn git_root(cwd: &Path) -> Option<std::path::PathBuf> {
    for ancestor in cwd.ancestors() {
        if ancestor.join(".git").exists() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

fn memory_list(cwd: &Path) -> Value {
    let dir = cwd.join(".craft").join("memory");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "md") {
                    p.file_name()
                        .map(|n| n.to_string_lossy().trim_end_matches(".md").to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };
    json!({ "entries": entries, "dir": dir })
}

fn recipe_list(cwd: &Path) -> Value {
    use craft_agent::discovery::Scope;
    let discovery = craft_agent::discovery::Discovery::new(
        cwd.to_path_buf(),
        craft_storage::paths::home(),
        craft_storage::paths::config_dir().ok(),
    );
    let files = discovery.discover_files("recipes", &["yaml", "yml", "json"]);
    let entries: Vec<Value> = files
        .into_iter()
        .map(|f| {
            let scope = match f.scope {
                Scope::Project(_) => "project",
                Scope::Global => "global",
                Scope::Builtin => "builtin",
            };
            json!({
                "name": f.path.file_stem().map(|s| s.to_string_lossy().into_owned()),
                "path": f.path,
                "scope": scope,
            })
        })
        .collect();
    json!({ "entries": entries })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    #[test]
    fn list_commands_contains_core_builtins() {
        let tmp = TempDir::new().unwrap();
        let v = list_commands(tmp.path());
        let names: Vec<&str> = v["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        for required in [
            "/compact", "/btw", "/help", "/clear", "/cd", "/mode", "/yolo",
        ] {
            assert!(names.contains(&required), "missing builtin {required}");
        }
    }

    #[test]
    fn list_commands_serializes_camel_case_with_method_and_meta_kind() {
        let tmp = TempDir::new().unwrap();
        let v = list_commands(tmp.path());
        let cmds = v["commands"].as_array().unwrap();

        // Serialized camelCase so the desktop TS interface lines up.
        let compact = cmds
            .iter()
            .find(|c| c["name"] == "/compact")
            .expect("/compact present");
        assert!(compact["maxArgs"].is_number());
        assert_eq!(compact["strategy"], "craft_request");
        assert_eq!(compact["method"], "_craft/session/compact");
        assert!(compact.get("metaKind").is_none());

        // Meta-prompt builtins carry both method and metaKind.
        let dream = cmds
            .iter()
            .find(|c| c["name"] == "/dream")
            .expect("/dream present");
        assert_eq!(dream["method"], "_craft/meta/prompt");
        assert_eq!(dream["metaKind"], "dream");

        // Non-craft builtins carry neither.
        let mode = cmds
            .iter()
            .find(|c| c["name"] == "/mode")
            .expect("/mode present");
        assert!(mode.get("method").is_none());
        assert!(mode.get("metaKind").is_none());
    }

    #[test]
    fn list_commands_includes_discovered_custom_commands() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".craft/commands");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("review.md"),
            "---\ndescription: Code review\nargument-hint: <file>\n---\nReview $ARGUMENTS",
        )
        .unwrap();

        let v = list_commands(tmp.path());
        let custom = v["custom"].as_array().unwrap();
        assert_eq!(custom.len(), 1);
        assert_eq!(
            custom[0]["displayName"].as_str().unwrap(),
            "/project:review"
        );
        assert!(custom[0]["acceptsArgs"].as_bool().unwrap());
        assert_eq!(custom[0]["scope"].as_str().unwrap(), "project");
    }

    #[test_case("/compact", "_craft/session/compact" ; "compact")]
    #[test_case("/btw", "_craft/session/btw" ; "btw")]
    #[test_case("/cd", "_craft/session/cd" ; "cd")]
    #[test_case("/dream", "_craft/meta/prompt" ; "dream")]
    #[test_case("/map", "_craft/map/show" ; "map")]
    fn method_for_builtins(name: &str, expected: &str) {
        assert_eq!(method_for(name), Some(expected));
    }

    #[test]
    fn method_for_returns_none_for_client_or_standard_commands() {
        assert_eq!(method_for("/help"), None);
        assert_eq!(method_for("/mode"), None);
        assert_eq!(method_for("/unknown"), None);
    }

    #[test]
    fn expand_custom_substitutes_arguments() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".craft/commands");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("greet.md"), "Hello $ARGUMENTS").unwrap();

        let expanded = expand_custom(tmp.path(), "/project:greet", "world").unwrap();
        assert_eq!(expanded, "Hello world");
    }

    #[test]
    fn expand_custom_returns_none_for_unknown_name() {
        let tmp = TempDir::new().unwrap();
        assert!(expand_custom(tmp.path(), "/project:missing", "x").is_none());
    }

    #[test]
    fn builtin_strategies_are_consistent() {
        for cmd in BUILTIN_COMMANDS {
            match cmd.strategy {
                CommandStrategy::CraftRequest => {
                    assert!(
                        method_for(cmd.name).is_some(),
                        "{} is CraftRequest but has no _craft method",
                        cmd.name,
                    );
                }
                CommandStrategy::Client => {
                    // `/clear` is dispatched client-side (drops in-memory items)
                    // but also has a no-op server ack so the palette has a
                    // server-confirmed entry. Other Client commands (`/help`)
                    // have no server endpoint at all.
                    if cmd.name != "/clear" {
                        assert_eq!(
                            method_for(cmd.name),
                            None,
                            "{} is Client but maps to a _craft method",
                            cmd.name,
                        );
                    }
                }
                CommandStrategy::AcpStandard | CommandStrategy::Passthrough => {
                    assert_eq!(
                        method_for(cmd.name),
                        None,
                        "{} is {:?} but maps to a _craft method",
                        cmd.name,
                        cmd.strategy,
                    );
                }
            }
        }
    }
}
