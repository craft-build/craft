//! Post-turn memory auto-extraction.
//!
//! After a run ends (the `TurnOutcome::Done` boundary in `run.rs`), this module
//! runs a cheap keyword pre-filter on the user's message for that run. If it
//! fires, one weak-tier side-completion extracts up to `MAX_FACTS` durable facts
//! as strict JSON; each fact is written as a memory concept (type `Note`) into
//! the per-project argosy (`<state>/projects/<id>/argosy`), and any superseded
//! note is deleted. Extraction is best-effort: every error is logged and
//! swallowed, and the host calls it via a detached `tokio::spawn` so it never
//! delays the run's return.

use std::path::PathBuf;
use std::sync::Arc;

use flume::unbounded;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use craft_config::model_roles::ModelRole;
use craft_providers::provider::Provider;
use craft_providers::roles::resolve_role;
use craft_providers::{Message, Model, RequestOptions, Timeouts};
use craft_storage::argosy_store::{ArgosyError, ArgosyStore};
use craft_storage::id::SessionRef;

const MAX_FACTS: usize = 4;
const MAX_USER_TEXT_CHARS: usize = 12_000;

/// Cue phrases that signal a durable, extractable fact. Lowercase; matched as
/// substrings against the lowercased user message. Conservative on purpose: a
/// false negative only skips one cheap save, while a false positive wastes a
/// model call.
const CUES: &[&str] = &[
    "i'm ",
    "i am ",
    "my product",
    "my project",
    "we decided",
    "we use",
    "we prefer",
    "we always",
    "we never",
    "from now on",
    "going forward",
    "rebrand",
    "renamed",
    "rename ",
    "convention is",
    "our convention",
    "our standard",
    "always use",
    "never use",
    "prefer ",
    "please use",
    "make sure to",
    "remember that",
    "note that",
    "the rule is",
    "policy is",
    "by default we",
    "don't forget",
    "important:",
    "heads up",
    "fyi",
    "btw",
];

const SYSTEM_PROMPT_HEAD: &str = "\
You extract durable facts a developer stated in their message so they persist across sessions.\n\
Durable = a stated preference, decision, convention, requirement, or 'from now on do X'.\n\
NOT durable = a one-off task, a question, a status update, or transient context.\n\n\
Rules:\n\
- Quote the user's words where possible rather than paraphrasing.\n\
- Never include secrets, credentials, tokens, or file contents — only the stated fact.\n\
- If `supersedes` matches one of the existing titles, set it to that exact title; else empty string.\n\
- Return at most ";
const SYSTEM_PROMPT_TAIL: &str = " facts.\n\
- Output ONLY a single JSON object, no prose, no code fences:\n\
  {\"facts\":[{\"title\":\"kebab-or-snake-case\",\"content\":\"the fact\",\"source\":\"user-stated\",\"supersedes\":\"\"}]}\n\
- If nothing durable was stated, return {\"facts\":[]} exactly.";

const PROVENANCE_SUFFIX: &str = "\n\n<!-- source: user-stated -->";

/// Owned context the detached task needs to run extraction. Everything is owned
/// so the spawned future borrows nothing from the `Agent`.
pub struct ExtractionCtx {
    pub project_root: PathBuf,
    pub argosy_dir: PathBuf,
    pub user_text: String,
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub timeouts: Timeouts,
    pub session_id: Option<SessionRef>,
}

#[derive(Debug, Deserialize)]
struct Fact {
    title: String,
    content: String,
    #[serde(default)]
    #[allow(dead_code)]
    source: String,
    #[serde(default)]
    supersedes: String,
}

#[derive(Debug, Deserialize, Default)]
struct Extraction {
    #[serde(default)]
    facts: Vec<Fact>,
}

/// Resolve the project root: the nearest ancestor containing a `.git` marker,
/// or the current directory when none is found. Falls back to the cwd on any
/// error. The memory tool resolves roots the same way.
pub(crate) fn memory_project_root() -> PathBuf {
    let Ok(cwd) = std::env::current_dir() else {
        return PathBuf::from(".");
    };
    for ancestor in cwd.ancestors() {
        if ancestor.join(".git").exists() {
            return ancestor.to_path_buf();
        }
    }
    cwd
}

/// Cheap keyword gate: true when the (lowercased) user message contains a
/// durable-fact cue. Off the latency path — only called once per qualifying run.
fn worth_extracting(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    CUES.iter().any(|cue| lower.contains(cue))
}

/// Tolerant JSON extraction: strip a leading code fence if present, slice from
/// the first `{` to the last `}`, then parse. Returns `None` on any miss so the
/// caller treats prose/non-JSON replies as "nothing to save".
fn parse_extraction(reply: &str) -> Option<Extraction> {
    let trimmed = reply.trim();
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str(&body[start..=end]).ok()
}

/// Turn a fact title into a stable concept name.
fn slugify(title: &str) -> String {
    let stripped = title.trim().trim_end_matches(".md");
    let slug: String = stripped
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Existing memory note names, used for supersession.
fn list_titles(argosy: &ArgosyStore) -> Vec<String> {
    argosy
        .list(argosy::bundle::Namespace::Memory)
        .unwrap_or_default()
}

/// Resolve the weak-tier provider/model for extraction: the configured
/// `memory_extractor` role if it resolves, else the run's active model.
pub async fn resolve_extraction_model(
    fallback_provider: Arc<dyn Provider>,
    fallback_model: Model,
    timeouts: Timeouts,
) -> (Arc<dyn Provider>, Model) {
    let role = resolve_role(
        ModelRole::MemoryExtractor,
        fallback_model.clone(),
        Arc::clone(&fallback_provider),
        timeouts,
    )
    .await;
    (role.primary.provider, role.primary.model)
}

/// One side-completion returning the model's raw text reply.
async fn collect_text(
    provider: &dyn Provider,
    model: &Model,
    messages: &[Message],
    system: &str,
    session_id: Option<&SessionRef>,
) -> Result<String, craft_providers::AgentError> {
    let (ptx, _prx) = unbounded();
    let tools = Value::Array(vec![]);
    let response = provider
        .stream_message(
            model,
            messages,
            system,
            &tools,
            &ptx,
            RequestOptions::default(),
            session_id,
        )
        .await?;
    Ok(response.message.user_text().unwrap_or_default().to_string())
}

fn build_system_prompt(existing_titles: &[String]) -> String {
    let mut prompt = format!(
        "{SYSTEM_PROMPT_HEAD}{MAX_FACTS}{SYSTEM_PROMPT_TAIL}\n\nExisting memory titles (use one verbatim in `supersedes` only when replacing it):\n"
    );
    if existing_titles.is_empty() {
        prompt.push_str("(none yet)\n");
    } else {
        for t in existing_titles {
            prompt.push_str("- ");
            prompt.push_str(t);
            prompt.push('\n');
        }
    }
    prompt
}

/// Run extraction end-to-end. Best-effort: every error is logged and swallowed.
pub async fn extract_and_store(ctx: ExtractionCtx) {
    if !worth_extracting(&ctx.user_text) {
        return;
    }
    if let Err(e) = run_extraction(&ctx).await {
        warn!(error = %e, "memory extraction failed");
    }
}

async fn run_extraction(ctx: &ExtractionCtx) -> Result<(), String> {
    let store = ArgosyStore::open_or_init(&ctx.argosy_dir, "memory-extraction")
        .map_err(|e| e.to_string())?;
    let titles = list_titles(&store);
    let system = build_system_prompt(&titles);
    let user_text = truncate_chars(&ctx.user_text, MAX_USER_TEXT_CHARS);
    let messages = vec![Message::user(user_text)];

    let (provider, model) =
        resolve_extraction_model(Arc::clone(&ctx.provider), ctx.model.clone(), ctx.timeouts).await;

    let reply = collect_text(
        provider.as_ref(),
        &model,
        &messages,
        &system,
        ctx.session_id.as_ref(),
    )
    .await
    .map_err(|e| e.to_string())?;

    let Some(extraction) = parse_extraction(&reply) else {
        info!("memory extraction returned no parseable JSON");
        return Ok(());
    };
    if extraction.facts.is_empty() {
        info!("memory extraction found no durable facts");
        return Ok(());
    }

    let mut written = 0usize;
    for fact in extraction.facts.into_iter().take(MAX_FACTS) {
        let title = fact.title.trim();
        let content = fact.content.trim();
        if title.is_empty() || content.is_empty() {
            continue;
        }
        let name = slugify(title);
        let body = format!("# {title}\n\n{content}{PROVENANCE_SUFFIX}");
        if let Err(e) = store.write_memory(&name, &body) {
            warn!(error = %e, name = %name, "failed to write memory note");
            continue;
        }
        written += 1;
        let supersedes = slugify(fact.supersedes.trim());
        if !fact.supersedes.trim().is_empty() && supersedes != name {
            match store.delete_memory(&supersedes) {
                Ok(()) | Err(ArgosyError::NotFound(_)) => {}
                Err(e) => {
                    warn!(error = %e, name = %supersedes, "failed to delete superseded note")
                }
            }
        }
    }
    info!(written, project = ?ctx.project_root, "memory extraction complete");
    Ok(())
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut = s.floor_char_boundary(max);
    let mut out = s[..cut].to_string();
    out.push_str("\n...(truncated)");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("we're rebranding to acme", true ; "rebrand")]
    #[test_case("from now on use sqlx for the db", true ; "from_now_on")]
    #[test_case("my product targets enterprise", true ; "my_product")]
    #[test_case("we decided to drop module x", true ; "we_decided")]
    #[test_case("please use snake_case for constants", true ; "please_use")]
    #[test_case("remember that prod runs on arm", true ; "remember_that")]
    #[test_case("fix the failing test", false ; "pure_task")]
    #[test_case("run the tests again", false ; "run_tests")]
    #[test_case("", false ; "empty")]
    #[test_case("add a button to the form", false ; "feature_request")]
    fn worth_extracting_cases(text: &str, expected: bool) {
        assert_eq!(worth_extracting(text), expected);
    }

    #[test_case(r#"{"facts":[{"title":"rebrand","content":"we are acme now","source":"user-stated","supersedes":""}]}"#, Some(1) ; "plain_json")]
    #[test_case("```json\n{\"facts\":[{\"title\":\"x\",\"content\":\"y\",\"supersedes\":\"\"}]}\n```", Some(1) ; "fenced_json")]
    #[test_case("Here you go:\n{\"facts\":[]}\nDone.", Some(0) ; "prose_wrapped_empty")]
    #[test_case("no json here at all", None ; "no_json_returns_none")]
    #[test_case("{ malformed", None ; "malformed_returns_none")]
    fn parse_extraction_cases(reply: &str, expected: Option<usize>) {
        match (parse_extraction(reply), expected) {
            (None, None) => {}
            (Some(extraction), Some(n)) => assert_eq!(extraction.facts.len(), n),
            (parsed, expected) => panic!("parsed={parsed:?}, expected={expected:?}"),
        }
    }

    #[test_case("Rebrand Plan", "rebrand-plan" ; "spaces_and_case")]
    #[test_case("api_v2 !!!", "api-v2" ; "symbols")]
    #[test_case("...___...", "untitled" ; "only_separators")]
    #[test_case("note.md", "note" ; "md_suffix_stripped")]
    fn slugify_cases(title: &str, expected: &str) {
        assert_eq!(slugify(title), expected);
    }

    fn tmp_store() -> (tempfile::TempDir, ArgosyStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ArgosyStore::open_or_init(&dir.path().join("argosy"), "test").unwrap();
        (dir, store)
    }

    #[test]
    fn list_titles_reports_memory_concepts() {
        let (_guard, store) = tmp_store();
        store.write_memory("rebrand", "x").unwrap();
        store.write_document("goal", "not a memory").unwrap();
        assert_eq!(list_titles(&store), vec!["rebrand".to_string()]);
    }

    #[tokio::test]
    async fn extract_and_store_writes_note_and_supersedes() {
        let provider = scripted_provider(
            r#"{"facts":[{"title":"rebrand","content":"we are acme","supersedes":"old-brand"}]}"#,
        );
        let dir = tempfile::tempdir().unwrap();
        let argosy_dir = dir.path().join("argosy");
        {
            let seed = ArgosyStore::open_or_init(&argosy_dir, "test").unwrap();
            seed.write_memory("old-brand", "stale").unwrap();
        }

        let ctx = ExtractionCtx {
            project_root: PathBuf::from("/tmp/repo"),
            argosy_dir: argosy_dir.clone(),
            user_text: "we're rebranding to acme".into(),
            provider,
            model: extraction_model(),
            timeouts: Timeouts::default(),
            session_id: None,
        };
        extract_and_store(ctx).await;

        let store = ArgosyStore::open_or_init(&argosy_dir, "test").unwrap();
        assert!(matches!(
            store.read_memory("old-brand"),
            Err(ArgosyError::NotFound(_))
        ));
        let body = store.read_memory("rebrand").unwrap();
        assert!(body.contains("we are acme"));
        assert!(body.contains("source: user-stated"));
    }

    #[tokio::test]
    async fn extract_and_store_skips_when_no_cue() {
        let provider =
            scripted_provider(r#"{"facts":[{"title":"x","content":"y","supersedes":""}]}"#);
        let dir = tempfile::tempdir().unwrap();
        let argosy_dir = dir.path().join("argosy");
        let ctx = ExtractionCtx {
            project_root: PathBuf::from("/tmp/repo"),
            argosy_dir,
            user_text: "just run the tests".into(),
            provider,
            model: extraction_model(),
            timeouts: Timeouts::default(),
            session_id: None,
        };
        extract_and_store(ctx).await;
        assert!(!dir.path().join("argosy").join("memory").exists());
    }

    fn scripted_provider(reply: &str) -> Arc<dyn Provider> {
        Arc::new(ScriptedProvider {
            reply: reply.to_string(),
        })
    }

    fn extraction_model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
    }

    struct ScriptedProvider {
        reply: String,
    }

    #[async_trait::async_trait]
    impl Provider for ScriptedProvider {
        async fn stream_message(
            &self,
            _model: &Model,
            _messages: &[Message],
            _system: &str,
            _tools: &Value,
            _event_tx: &flume::Sender<craft_providers::ProviderEvent>,
            _opts: RequestOptions,
            _session_id: Option<&SessionRef>,
        ) -> Result<craft_providers::StreamResponse, craft_providers::AgentError> {
            Ok(craft_providers::StreamResponse {
                message: Message {
                    role: craft_providers::Role::Assistant,
                    content: vec![craft_providers::ContentBlock::Text {
                        text: self.reply.clone(),
                    }],
                    ..Default::default()
                },
                usage: craft_providers::TokenUsage::default(),
                stop_reason: Some(craft_providers::StopReason::EndTurn),
            })
        }

        async fn list_models(&self) -> Result<Vec<String>, craft_providers::AgentError> {
            unimplemented!()
        }
    }
}
