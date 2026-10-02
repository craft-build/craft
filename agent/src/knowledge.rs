//! In-process argosy integration: the knowledge service (Phase 0 of the
//! argosy integration plan).
//!
//! The `argosy` crate is linked directly (no subprocess, no MCP transport):
//! its `McpState` sync handlers are called from portable builtin tools
//! registered as first-class native tools. One process-global service
//! owns the state (index/dedup) so the TUI, ACP, headless surfaces and every
//! subagent share it. Argosy keeps its own data under its own state dir
//! (`~/.local/state/argosy`, keyed by project root) — nothing is relocated
//! into craft's state dir.
//!
//! The module is named `knowledge` (not `argosy`) so it never shadows the
//! extern crate in path resolution.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde_json::Value;

/// The concrete store behind the service: lazy tract embeddings + SQLite.
pub type Store = argosy::mcp::McpState<
    argosy::index::tract::LazyTractProvider,
    argosy::index::sqlite::SqliteVecStore,
>;

/// Argosy tool names (their native wire names), in
/// `tool_definitions()` order. Pinned by a test against the crate so a new
/// upstream tool cannot register silently.
pub const TOOL_NAMES: &[&str] = &[
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
    // code-tools
    "outline",
    "zoom",
    "astgrep",
    "conflicts",
    "inspect",
    "callgraph",
    "repomap",
    "start_review",
    "review_diff",
    "report_finding",
    "review_findings",
];

/// The read-only subset (permission documentation + subagent budgets).
pub const READ_ONLY_TOOLS: &[&str] = &[
    "search",
    "list_skills",
    "get_skill",
    "search_rules",
    "read_memory",
    "read_document",
    "ask",
    "outline",
    "zoom",
    "inspect",
    "callgraph",
    "repomap",
    "review_diff",
    "review_findings",
];

/// The code-tools subset (handled by `CodeTools`, not the store).
const CODE_TOOLS: &[&str] = &[
    "outline",
    "zoom",
    "astgrep",
    "conflicts",
    "inspect",
    "callgraph",
    "repomap",
    "start_review",
    "review_diff",
    "report_finding",
    "review_findings",
];

/// Whether `tool` (without prefix) is routed to `CodeTools`.
fn is_code_tool(tool: &str) -> bool {
    CODE_TOOLS.contains(&tool)
}

/// The native name craft registers an argosy crate tool under: the
/// crate name unless it collides with a craft builtin.
fn native_name(crate_name: &str) -> &str {
    match crate_name {
        "read" => "read_document",
        other => other,
    }
}

/// Whether a tool name is one of ours.
pub fn is_argosy_tool(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

/// The argosy-visible skills for a project root: `(name, SKILL.md body)`
/// pairs from `list_skills` + `get_skill`. Empty when the service is down.
pub fn skill_bodies(cwd: &Path) -> Vec<(String, String)> {
    let service = ArgosyService::global();
    if service.init_error().is_some() {
        return Vec::new();
    }
    let Ok(report) = service.execute("list_skills", serde_json::json!({ "cwd": cwd })) else {
        return Vec::new();
    };
    let Some(names) = report.get("skills").and_then(Value::as_array) else {
        return Vec::new();
    };
    // The report types are Serialize-only upstream; read the two fields we
    // need off the JSON directly.
    names
        .clone()
        .into_iter()
        .filter(|s| !s.get("shadowed").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|s| {
            let name = s.get("name")?.as_str()?.to_string();
            let params = serde_json::json!({ "cwd": cwd, "name": name });
            service
                .execute("get_skill", params)
                .ok()
                .and_then(|v| v.get("content").and_then(Value::as_str).map(str::to_string))
                .map(|content| (name, content))
        })
        .collect()
}

/// The process-global service. Construction mirrors argosy's `cmd_mcp`:
/// load the user config, resolve the embedding model spec, open projects
/// lazily through a session factory, and attach the decision endpoint
/// provider when `decision.enabled` is set.
pub struct ArgosyService {
    store: std::sync::Mutex<Option<Store>>,
    decision: Arc<dyn argosy::decision::DecisionProvider>,
    code: Arc<argosy::codetools::CodeTools>,
    init_error: Option<String>,
}

fn build_store() -> Result<Store, String> {
    let config = argosy::config::Config::load().map_err(|e| format!("argosy config: {e:#}"))?;
    let default_k = config.index.mcp_default_k;
    let db_name = config.index_db_name().to_string();
    let embed_cache: Option<PathBuf> = config.embed_cache_dir();
    let spec = argosy::index::tract::ModelSpec::from_name(config.model_name())
        .ok_or_else(|| format!("unknown argosy embedding model `{}`", config.model_name()))?;
    // Projects open lazily by canonical root and stay cached for the
    // process lifetime; a failed reconcile degrades retrieval without
    // failing the open (mutating tools re-attempt it on every write).
    let factory: argosy::mcp::SessionFactory<
        argosy::index::tract::LazyTractProvider,
        argosy::index::sqlite::SqliteVecStore,
    > = Arc::new(move |root| {
        let context = argosy::context::ProjectContext::open_project(root)?;
        let store = argosy::index::sqlite::SqliteVecStore::open(
            argosy::pull::project_argosy_dir(root)?.join(&db_name),
        )?;
        let mut index = argosy::index::Index::new(
            argosy::index::tract::LazyTractProvider::new(spec, embed_cache.clone())?,
            store,
        );
        if let Err(err) = index.reconcile(&context) {
            tracing::warn!(
                root = %root.display(),
                error = %err,
                "argosy index reconcile failed; serving degraded"
            );
        }
        Ok(argosy::mcp::ProjectSession::new(context, index).with_default_k(default_k))
    });
    let mut state = Store::new(factory);
    let provider = argosy::decision::provider_from_config(&config.decision)
        .map_err(|e| format!("argosy decision config: {e:#}"))?;
    state = state.with_decision(Arc::from(provider));
    Ok(state)
}

impl ArgosyService {
    fn new() -> Self {
        let decision: Arc<dyn argosy::decision::DecisionProvider> =
            match argosy::config::Config::load() {
                Ok(config) => match argosy::decision::provider_from_config(&config.decision) {
                    Ok(provider) => Arc::from(provider),
                    Err(err) => {
                        tracing::warn!(error = %err, "argosy decision provider disabled");
                        Arc::new(argosy::decision::Disabled)
                    }
                },
                Err(_) => Arc::new(argosy::decision::Disabled),
            };
        match build_store() {
            Ok(store) => Self {
                store: std::sync::Mutex::new(Some(store)),
                decision,
                code: Arc::new(argosy::codetools::CodeTools::default()),
                init_error: None,
            },
            Err(err) => {
                tracing::warn!(error = %err, "argosy service failed to initialize");
                Self {
                    store: std::sync::Mutex::new(None),
                    decision,
                    code: Arc::new(argosy::codetools::CodeTools::default()),
                    init_error: Some(err),
                }
            }
        }
    }

    /// The process-global service, built on first use. Construction may
    /// build a `reqwest::blocking` client from the decision config, which
    /// panics inside an async context — so the first build hops to a plain
    /// thread when the caller is on a runtime.
    pub fn global() -> &'static Self {
        static SERVICE: OnceLock<ArgosyService> = OnceLock::new();
        if let Some(service) = SERVICE.get() {
            return service;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return SERVICE.get_or_init(Self::new);
        }
        std::thread::scope(|scope| {
            scope
                .spawn(|| SERVICE.get_or_init(Self::new))
                .join()
                .expect("argosy service init thread panicked")
        })
    }

    /// The configured decision endpoint provider (a `Disabled` no-op when
    /// `decision.enabled` is unset). Shared with the auto-approval gate.
    pub fn decision(&self) -> Arc<dyn argosy::decision::DecisionProvider> {
        self.decision.clone()
    }

    /// Why the store failed to initialize, when it did.
    pub fn init_error(&self) -> Option<&str> {
        self.init_error.as_deref()
    }

    /// Read one `argosy://` resource of the project at `root` (the
    /// builtin `argosy://` namespace of the read tool): any concept in
    /// any active argosy, plus the pseudo-resources (`_argosys`, the
    /// global `catalog`, `<name>/_index`). Blocking — run under
    /// `spawn_blocking`. Unlike tool calls, resources carry no `cwd`, so
    /// the read tool pins them to its workspace root rather than the
    /// process working directory.
    pub fn read_resource(&self, root: &Path, uri: &str) -> std::result::Result<String, String> {
        self.with_store(|store| {
            if uri == argosy::mcp::CATALOG_URI {
                // The catalog is global: it needs no project session.
                store.read_resource(uri)
            } else {
                store.session(root)?.read_resource(uri)
            }
        })
        .map(|body| body.text)
        .map_err(|error| format!("argosy: {error:#}"))
    }

    /// Execute one argosy tool call. `tool` is the native name. Blocking by
    /// nature — callers run it under `spawn_blocking`. The report types serialize to JSON.
    pub fn execute(&self, tool: &str, args: Value) -> Result<Value, String> {
        use argosy::mcp as m;

        macro_rules! call {
            ($method:ident, $ty:ty) => {{
                let params: $ty = serde_json::from_value(args)
                    .map_err(|e| format!("invalid arguments for `{tool}`: {e}"))?;
                let out = self
                    .with_store(|store| store.$method(params))
                    .map_err(|e| format!("{e:#}"))?;
                serde_json::to_value(out).map_err(|e| format!("serialize report: {e}"))
            }};
        }
        macro_rules! call_code {
            ($handler:expr, $ty:ty) => {{
                let params: $ty = serde_json::from_value(args)
                    .map_err(|e| format!("invalid arguments for `{tool}`: {e}"))?;
                let out = $handler(&self.code, params).map_err(|e| format!("{e:#}"))?;
                serde_json::to_value(out).map_err(|e| format!("serialize report: {e}"))
            }};
        }
        match tool {
            "search" => call!(search, m::SearchParams),
            "search_rules" => call!(search_rules, m::RulesParams),
            "ask" => call!(ask, m::AskParams),
            "list_skills" => call!(list_skills, m::ListSkillsParams),
            "get_skill" => call!(get_skill, m::GetSkillParams),
            "read_memory" => call!(read_memory, m::ReadPathParams),
            "read_document" => call!(read, m::ReadParams),
            "write_memory" => call!(write_memory, m::WriteParams),
            "delete_memory" => call!(delete_memory, m::ReadPathParams),
            "write_rule" => call!(write_rule, m::WriteParams),
            "delete_rule" => call!(delete_rule, m::ReadPathParams),
            "write_document" => call!(write_document, m::WriteParams),
            "delete_document" => call!(delete_document, m::ReadPathParams),
            "promote" => call!(promote, m::PromoteParams),
            "outline" => call_code!(
                argosy::codetools::outline::run,
                argosy::codetools::outline::OutlineParams
            ),
            "zoom" => call_code!(
                argosy::codetools::zoom::run,
                argosy::codetools::zoom::ZoomParams
            ),
            "astgrep" => call_code!(
                argosy::codetools::astgrep::run,
                argosy::codetools::astgrep::AstgrepParams
            ),
            "conflicts" => call_code!(
                argosy::codetools::conflicts::run,
                argosy::codetools::conflicts::ConflictsParams
            ),
            "inspect" => call_code!(
                argosy::codetools::inspect::run,
                argosy::codetools::inspect::InspectParams
            ),
            "callgraph" => call_code!(
                argosy::codetools::callgraph::run,
                argosy::codetools::callgraph::CallgraphParams
            ),
            "repomap" => call_code!(
                argosy::codetools::repomap::run,
                argosy::codetools::repomap::RepomapParams
            ),
            "start_review" => call_code!(
                argosy::codetools::review::start_review,
                argosy::codetools::review::StartReviewParams
            ),
            "review_diff" => call_code!(
                argosy::codetools::review::review_diff,
                argosy::codetools::review::ReviewDiffParams
            ),
            "report_finding" => call_code!(
                argosy::codetools::review::report_finding,
                argosy::codetools::review::ReportFindingParams
            ),
            "review_findings" => call_code!(
                argosy::codetools::review::review_findings,
                argosy::codetools::review::ReviewFindingsParams
            ),
            other => Err(format!("unknown argosy tool: {other}")),
        }
    }

    fn with_store<T>(&self, op: impl FnOnce(&mut Store) -> argosy::Result<T>) -> argosy::Result<T> {
        let mut guard = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(store) = guard.as_mut() else {
            return Err(argosy::Error::Validation {
                reason: format!(
                    "the argosy service failed to initialize{}",
                    self.init_error
                        .as_ref()
                        .map(|e| format!(": {e}"))
                        .unwrap_or_default()
                ),
            });
        };
        op(store)
    }
}

/// One registered argosy tool: wire name, description, and input schema.
pub struct ToolDef {
    /// Leaked once so per-turn registration can key on `&'static str`.
    pub native: &'static str,
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// The argosy tool definitions mapped to craft's registration shape,
/// registered under native names (crate tool `read` becomes `read_document`
/// to avoid the craft builtin). Cached: names are leaked once so
/// `builtin_tool_table` (rebuilt per turn) can hand out `&'static str` keys
/// without leaking per call.
pub fn tool_definitions() -> &'static Vec<ToolDef> {
    static DEFS: OnceLock<Vec<ToolDef>> = OnceLock::new();
    DEFS.get_or_init(|| {
        argosy::mcp::tool_definitions()
            .into_iter()
            .map(|tool| {
                let native = native_name(&tool.name);
                let description = if tool.name == "search" {
                    format!(
                        "Semantic search over the project's knowledge base \
(argosy: skills, memories, rules, documents). Not file content search — \
use grep/glob for that. {}",
                        tool.description.clone().unwrap_or_default()
                    )
                } else {
                    tool.description.clone().unwrap_or_default().to_string()
                };
                ToolDef {
                    native: Box::leak(native.to_string().into_boxed_str()),
                    name: native.to_string(),
                    description,
                    schema: serde_json::to_value(&tool.input_schema)
                        .unwrap_or_else(|_| serde_json::json!({"type": "object"})),
                }
            })
            .collect()
    })
}

/// `dream` prompt body (memory consolidation workflow), via the argosy
/// prompt resolver.
pub fn dream_prompt() -> Result<String, String> {
    prompt_body("dream", None)
}

/// `scan` prompt body (project documentation workflow).
pub fn scan_prompt() -> Result<String, String> {
    prompt_body("scan", None)
}

fn prompt_body(
    name: &str,
    arguments: Option<&serde_json::Map<String, Value>>,
) -> Result<String, String> {
    let result = argosy::mcp::get_prompt_result(name, arguments)
        .ok()
        .flatten()
        .ok_or_else(|| format!("unknown argosy prompt: {name}"))?;
    Ok(result
        .messages
        .into_iter()
        .map(|message| match message.content {
            rmcp::model::ContentBlock::Text(text) => text.text,
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n\n"))
}

/// The rendered `review` prompt: `base`/`commit` select the diff scope,
/// `focus_files` narrows the review.
pub fn review_prompt(
    base: Option<&str>,
    commit: Option<&str>,
    focus_files: &[String],
) -> Result<String, String> {
    let mut arguments = serde_json::Map::new();
    if let Some(base) = base {
        arguments.insert("base".into(), Value::String(base.to_string()));
    }
    if let Some(commit) = commit {
        arguments.insert("commit".into(), Value::String(commit.to_string()));
    }
    if !focus_files.is_empty() {
        arguments.insert(
            "focus_files".into(),
            Value::Array(
                focus_files
                    .iter()
                    .map(|f| Value::String(f.clone()))
                    .collect(),
            ),
        );
    }
    prompt_body("review", Some(&arguments))
}

/// The vendored reviewer definition written to `.craft/agents/reviewer.md`
/// (Phase 5). Craft's harness is not an argosy `Harness` variant, so the
/// definition lives here rather than upstream.
pub const REVIEWER_AGENT_MD: &str = r#"---
description: Read-only code reviewer that records prioritized findings through the argosy review tools and returns a verdict.
tools:
  - read
  - grep
  - glob
  - list
  - search_rules
  - search
  - read_document
  - read_memory
  - start_review
  - review_diff
  - report_finding
  - review_findings
---

You are a code reviewer. You are read-only: never modify, create, or delete
files. Review the diff you were given against the repository and its
styleguide rules.

Workflow:
1. `start_review` to snapshot the diff.
2. `review_diff` for the changed files; read surrounding code as needed.
3. `search_rules` to find the styleguide rules that govern the changed code.
4. For every verified defect, record it with `report_finding` (P0–P3, with file:line, a concrete failure scenario, and a fix).
5. End with a prioritized verdict: counts per priority, overall assessment, and the most important next step.
"#;

/// Idempotently install the reviewer agent definition into
/// `.craft/agents/reviewer.md` under `root`. Never overwrites an existing
/// file (pass `force` to replace).
pub fn install_reviewer_definition(root: &Path, force: bool) -> Result<PathBuf, String> {
    let dir = root.join(".craft").join("agents");
    let path = dir.join("reviewer.md");
    if path.exists() && !force {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    std::fs::write(&path, REVIEWER_AGENT_MD)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

pub async fn call_tool(tool: String, args: Value) -> Result<Value, String> {
    // Code tools work without the store; knowledge tools do not.
    let init_error = (!is_code_tool(&tool))
        .then(|| ArgosyService::global().init_error().map(str::to_string))
        .flatten();
    tokio::task::spawn_blocking(move || {
        if let Some(err) = init_error {
            return Err(format!("argosy unavailable: {err}"));
        }
        ArgosyService::global().execute(&tool, args)
    })
    .await
    .map_err(|e| format!("argosy tool task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_match_the_crate_definitions() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, TOOL_NAMES.to_vec());
    }

    #[test]
    fn native_names_are_recognized() {
        assert!(is_argosy_tool("search"));
        assert!(is_argosy_tool("review_findings"));
        assert!(is_argosy_tool("read_document"));
        assert!(is_argosy_tool("inspect"));
        assert!(!is_argosy_tool("nope"));
        assert!(!is_argosy_tool("argosy__search"));
    }

    #[test]
    fn every_definition_carries_a_schema() {
        for def in tool_definitions() {
            assert!(def.schema.is_object(), "{}", def.native);
            assert!(!def.description.is_empty(), "{}", def.native);
        }
    }

    #[test]
    fn unknown_tool_errors() {
        let err = ArgosyService::global()
            .execute("nope", serde_json::json!({}))
            .unwrap_err();
        assert!(err.contains("unknown argosy tool"));
    }

    #[test]
    fn reviewer_definition_installs_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let first = install_reviewer_definition(dir.path(), false).unwrap();
        assert!(first.exists());
        let again = install_reviewer_definition(dir.path(), false).unwrap();
        assert_eq!(first, again);
    }
}
