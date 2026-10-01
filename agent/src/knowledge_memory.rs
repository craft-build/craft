//! Post-turn memory extraction (Phase 4 of the argosy integration).
//!
//! Ported from the reference craft's `memory_extraction`: after a successful
//! run, a keyword cue gate decides whether the turn might contain durable
//! facts; a weak-tier side completion (the `edge`/compaction pattern, no
//! tools) extracts a bounded set of strict-JSON facts; each is persisted
//! into the project's local argosy through `write_memory` (same title →
//! same slug → in-place update). Detached `tokio::spawn`, best-effort:
//! failures are swallowed with a tracing warn and never fail the turn.
//!
//! Controlled by `agent.memory_extraction` (default on).

use rig_core::completion::{CompletionModel, message::AssistantContent};
use serde::Deserialize;

use crate::history::Message;

/// Cue words that make a turn eligible for extraction. Cheap and
/// conservative: no cue, no model call.
const CUES: &[&str] = &[
    "remember",
    "learned",
    "lesson",
    "gotcha",
    "insight",
    "decision",
    "decided",
    "convention",
    "pattern",
    "architecture",
    "durable",
    "important",
];

/// Maximum facts extracted per turn.
const MAX_FACTS: usize = 5;

fn extraction_prompt() -> String {
    format!(
        "You extract durable project knowledge from a conversation turn. \
         Only extract facts a future session in this repository would need: \
         architecture decisions, conventions, gotchas, and non-obvious project \
         behavior. Skip transient state, user preferences about the chat \
         itself, and anything derivable from the code.\n\n\
         Reply with ONLY a JSON array (no prose, no markdown fences) of at \
         most {MAX_FACTS} objects, each exactly: {{\"title\": \"short \
         imperative title\", \"description\": \"one line\", \"content\": \
         \"1-5 sentences of markdown body\"}}. Reply with [] when nothing \
         qualifies."
    )
}

#[derive(Debug, Clone, Deserialize)]
pub struct Fact {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub content: String,
}

/// Plain text of a message, when it carries any.
fn message_text(message: &Message) -> Option<String> {
    match message {
        Message::User { content } => Some(
            content
                .iter()
                .filter_map(|part| match part {
                    crate::history::UserContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        ),
        Message::Assistant { content } => Some(
            content
                .iter()
                .filter_map(|part| match part {
                    crate::history::AssistantContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

/// Whether any cue word appears in the turn's user and assistant text.
pub fn has_cue(history: &[Message]) -> bool {
    history.iter().rev().take(12).any(|message| {
        message_text(message).is_some_and(|text| {
            let text = text.to_lowercase();
            CUES.iter().any(|cue| text.contains(cue))
        })
    })
}

/// Parse the extractor's strict-JSON reply. Tolerates JSON embedded in
/// prose by extracting the outermost `[...]`/`{...}` block.
pub fn parse_facts(text: &str) -> Result<Vec<Fact>, String> {
    let value = crate::json_repair::extract_json(text.trim())?;
    // A single object is accepted as a one-element list.
    let array = match value {
        serde_json::Value::Array(items) => items,
        object @ serde_json::Value::Object(_) => vec![object],
        _ => return Err("expected a JSON array of facts".into()),
    };
    let facts: Vec<Fact> = array
        .into_iter()
        .filter_map(|item| serde_json::from_value::<Fact>(item).ok())
        .filter(|fact| !fact.title.trim().is_empty() && !fact.content.trim().is_empty())
        .collect();
    Ok(facts.into_iter().take(MAX_FACTS).collect())
}

/// Slugify a title into a stable concept path segment.
pub fn slugify(title: &str) -> String {
    let mut slug = String::new();
    for ch in title.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if (ch == ' ' || ch == '-' || ch == '_') && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "untitled".into()
    } else {
        slug
    }
}

/// Render one fact as the memory concept markdown argosy stores.
pub fn fact_markdown(fact: &Fact) -> String {
    let description = fact
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| fact.title.trim());
    format!(
        "---\ntype: Memory\ndescription: {}\n---\n\n{}\n",
        yaml_escape(description),
        fact.content.trim()
    )
}

fn yaml_escape(text: &str) -> String {
    // One-line, quote only when the content would parse as something else.
    let text = text.replace(['\n', '\r'], " ");
    if text.contains(':') || text.contains('#') || text.starts_with(' ') {
        format!("{text:?}")
    } else {
        text
    }
}

/// Write one fact into the project's local argosy (blocking; callers run
/// it off the async workers). Same title → same slug path → update.
pub fn write_fact(fact: &Fact, cwd: &std::path::Path) -> Result<(), String> {
    let path = format!("memory/{}", slugify(&fact.title));
    let params = serde_json::json!({
        "cwd": cwd.display().to_string(),
        "path": path,
        "content": fact_markdown(fact),
    });
    crate::knowledge::ArgosyService::global()
        .execute("write_memory", params)
        .map(|_| ())
        .map_err(|e| format!("write_memory: {e:#}"))
}

/// One weak-tier side completion that extracts facts from the turn
/// (the auto-review/compaction edge pattern).
async fn extract_facts<M: CompletionModel>(
    model: &M,
    history: &[Message],
) -> Result<Vec<Fact>, String> {
    let mut transcript = String::new();
    for message in history.iter().rev().take(12).rev() {
        if let Some(text) = message_text(message) {
            transcript.push_str(if matches!(message, Message::User { .. }) {
                "User: "
            } else {
                "Assistant: "
            });
            transcript.push_str(&text);
            transcript.push_str("\n\n");
        }
    }
    let messages = vec![Message::User {
        content: vec![crate::history::UserContent::text(transcript)],
    }];
    let request = crate::edge::to_request(&messages, &[], Some(&extraction_prompt()), None, None);
    let response = model
        .completion(request)
        .await
        .map_err(|e| format!("extraction call failed: {e}"))?;
    let text = response
        .choice
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return Err("extraction produced no text".into());
    }
    parse_facts(&text)
}

/// Outstanding extraction tasks, so surfaces can drain them before the
/// session (or process) goes away instead of relying on the runtime
/// outliving the work.
static PENDING: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>> =
    std::sync::Mutex::new(Vec::new());

/// Await every outstanding extraction, bounded by `timeout`; leftover
/// tasks on timeout are detached (writes already landed or never will).
pub async fn wait_for_pending(timeout: std::time::Duration) {
    let tasks: Vec<_> = std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner()));
    if tasks.is_empty() {
        return;
    }
    let _ = tokio::time::timeout(
        timeout,
        futures::future::join_all(tasks.into_iter().map(|task| async move {
            let _ = task.await;
        })),
    )
    .await;
}

/// Fire-and-forget post-turn extraction: detached spawn, best-effort, all
/// failures swallowed. Called by every surface after a successful run; the
/// handle is also tracked so [`wait_for_pending`] can drain it at teardown.
pub fn spawn_extraction(
    model: crate::providers::DynamicModel,
    history: Vec<Message>,
    cwd: std::path::PathBuf,
    enabled: bool,
) {
    if !enabled || !has_cue(&history) {
        return;
    }
    let task = tokio::spawn(async move {
        let facts = match extract_facts(&model, &history).await {
            Ok(facts) => facts,
            Err(err) => {
                tracing::debug!(error = %err, "memory extraction skipped");
                return;
            }
        };
        for fact in &facts {
            if let Err(err) = tokio::task::spawn_blocking({
                let fact = fact.clone();
                let cwd = cwd.clone();
                move || write_fact(&fact, &cwd)
            })
            .await
            .unwrap_or_else(|e| Err(format!("task failed: {e}")))
            {
                tracing::warn!(error = %err, "memory write failed");
            }
        }
        if !facts.is_empty() {
            tracing::info!(count = facts.len(), "memory extraction stored facts");
        }
    });
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(task);
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|task| !task.is_finished());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cue_gate_matches_keywords() {
        assert!(has_cue(&[Message::User {
            content: vec![crate::history::UserContent::text(
                "we learned the api retries",
            )],
        }]));
        assert!(has_cue(&[Message::assistant("Remember this gotcha")]));
        assert!(!has_cue(&[Message::assistant("hello world")]));
        assert!(!has_cue(&[] as &[Message]));
    }

    #[test]
    fn facts_parse_from_plain_json_and_single_object() {
        let facts = parse_facts(
            r#"[{"title":"Retry policy","description":"api","content":"Retries twice."}]"#,
        )
        .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].title, "Retry policy");
        let one = parse_facts(r#"{"title":"X","content":"y"}"#).unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn blank_facts_are_dropped_and_max_enforced() {
        let items: Vec<String> = (0..8)
            .map(|i| format!(r#"{{"title":"t{i}","content":"c{i}"}}"#))
            .collect();
        let facts = parse_facts(&format!("[{}]", items.join(","))).unwrap();
        assert_eq!(facts.len(), MAX_FACTS);
        assert!(
            parse_facts(r#"[{"title":"  ","content":"x"}]"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn slugify_stabilizes_titles() {
        assert_eq!(slugify("Retry Policy!"), "retry-policy");
        assert_eq!(slugify("  A--B  "), "a-b");
        assert_eq!(slugify("???"), "untitled");
    }

    #[test]
    fn fact_markdown_has_frontmatter_and_body() {
        let fact = Fact {
            title: "Retry policy".into(),
            description: Some("api retries".into()),
            content: "Retries twice.".into(),
        };
        let md = fact_markdown(&fact);
        assert!(md.starts_with("---\ntype: Memory\ndescription: api retries\n---\n"));
        assert!(md.contains("Retries twice."));
    }

    #[test]
    fn yaml_specials_are_quoted() {
        let fact = Fact {
            title: "t".into(),
            description: Some("key: value # hash".into()),
            content: "c".into(),
        };
        assert!(fact_markdown(&fact).contains("\"key: value # hash\""));
    }
}
