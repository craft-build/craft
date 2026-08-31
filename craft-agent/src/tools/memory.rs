//! The `memory` tool: persistent, project-scoped notes (gotchas, patterns,
//! decisions) stored as argosy memory concepts under
//! `<state>/projects/<project-id>/argosy/memory/`. One argosy per project,
//! shared with the Flow document namespace.

use std::path::PathBuf;
use std::sync::Arc;

use craft_storage::argosy_store::ArgosyStore;
use craft_tool_macro::Tool;
use serde::Deserialize;

use super::ToolContext;
use crate::tools::ToolInvocation;
use crate::types::ToolOutput;

const MAX_LINES_PER_NOTE: usize = 200;
const MAX_DIR_BYTES: usize = 50 * 1024;
const SEARCH_TOP_K: usize = 5;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Memory {
    #[param(description = "Command: view, write, delete")]
    command: String,
    #[param(description = "Note name (e.g. 'architecture'). Omit on view to list all.")]
    path: Option<String>,
    #[param(description = "Note content for 'write'")]
    content: Option<String>,
}

const DESCRIPTION: &str = "\
Persistent, project-scoped scratchpad for learnings, patterns, decisions, and \
gotchas across sessions.\n\n\
- Save important context before compaction or to build up project knowledge; \
proactively save non-obvious project gotchas and architecture decisions.\n\
- Keep entries concise and current. Delete outdated information.\n\
- `view` with a search query (not a note name) recalls notes by keyword rank.\n\
- Notes are stored as concepts; `view` without a name lists them all.";

impl Memory {
    pub const NAME: &str = "memory";
    pub const DESCRIPTION: &str = DESCRIPTION;
    pub const EXAMPLES: Option<&str> = Some(
        r#"[{"command":"view"},{"command":"write","path":"conventions","content":"Conventions: we use argosy for notes."},{"command":"delete","path":"stale-note"}]"#,
    );

    pub fn start_header(&self) -> String {
        match &self.path {
            Some(path) => format!("memory {} {path}", self.command),
            None => self.command.clone(),
        }
    }

    pub async fn execute(&self, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        let store = open_project_store()?;
        match self.command.as_str() {
            "view" => view(&store, self.path.as_deref()),
            "write" => {
                let path = self.path.as_deref().ok_or("'path' is required")?;
                let content = self.content.as_deref().ok_or("'content' is required")?;
                write(&store, normalize(path), content)
            }
            "delete" => {
                let path = self.path.as_deref().ok_or("'path' is required")?;
                delete(&store, normalize(path))
            }
            other => Err(format!(
                "unknown command '{other}'. Valid commands: view, write, delete"
            )),
        }
    }
}

pub(crate) fn open_project_store() -> Result<Arc<ArgosyStore>, String> {
    let state = craft_storage::paths::state_dir().map_err(|e| e.to_string())?;
    let cwd = std::env::current_dir().map_err(|e| format!("cwd error: {e}"))?;
    let root = project_root(&cwd);
    let id = craft_storage::flow::project_id(&root);
    let dir: PathBuf = state.join("projects").join(&id).join("argosy");
    ArgosyStore::open_or_init(&dir, &id)
        .map(Arc::new)
        .map_err(|e| e.to_string())
}

/// Nearest ancestor holding a `.git` marker, else `cwd` (matches how memory
/// extraction resolves the project).
fn project_root(cwd: &std::path::Path) -> PathBuf {
    for ancestor in cwd.ancestors() {
        if ancestor.join(".git").exists() {
            return ancestor.to_path_buf();
        }
    }
    cwd.to_path_buf()
}

fn normalize(path: &str) -> &str {
    path.trim_end_matches(".md")
}

fn view(store: &ArgosyStore, path: Option<&str>) -> Result<ToolOutput, String> {
    let names = store
        .list(argosy::bundle::Namespace::Memory)
        .map_err(|e| e.to_string())?;
    let path = path.map(normalize);
    let Some(path) = path else {
        if names.is_empty() {
            return Ok(ToolOutput::Plain("No memories yet.".to_string()));
        }
        let mut out = String::new();
        let mut total = 0usize;
        for name in &names {
            let size = store.read_memory(name).map(|b| b.len()).unwrap_or(0);
            total += size;
            out.push_str(&format!("- {name} ({size} bytes)\n"));
        }
        out.push_str(&format!("\n{} notes, {total} bytes total", names.len()));
        return Ok(ToolOutput::Plain(out));
    };
    if names.iter().any(|n| n == path) {
        let body = store.read_memory(path).map_err(|e| e.to_string())?;
        return Ok(ToolOutput::Plain(body));
    }
    search(store, path, &names)
}

fn search(store: &ArgosyStore, query: &str, names: &[String]) -> Result<ToolOutput, String> {
    let terms = tokenize(query);
    let mut scored = Vec::new();
    for name in names {
        let body = store.read_memory(name).map_err(|e| e.to_string())?;
        let score = score(&body, &terms);
        if score > 0.0 {
            scored.push((score, name.clone(), body));
        }
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(SEARCH_TOP_K);
    if scored.is_empty() {
        return Err(format!("'{query}' not found"));
    }
    let mut out = format!("Keyword search results for '{query}':\n");
    for (score, name, body) in scored {
        out.push_str(&format!("\n## {name} (score: {score:.2})\n\n{body}\n"));
    }
    Ok(ToolOutput::Plain(out))
}

fn write(store: &ArgosyStore, path: &str, content: &str) -> Result<ToolOutput, String> {
    let lines = content.lines().count().max(1);
    if lines > MAX_LINES_PER_NOTE {
        return Err(format!(
            "content exceeds {MAX_LINES_PER_NOTE} lines ({lines} lines); reduce content size"
        ));
    }
    let names = store
        .list(argosy::bundle::Namespace::Memory)
        .map_err(|e| e.to_string())?;
    let mut total = 0usize;
    for name in &names {
        if name == path {
            continue;
        }
        total += store.read_memory(name).map(|b| b.len()).unwrap_or(0);
    }
    if total + content.len() > MAX_DIR_BYTES {
        return Err(format!(
            "memory would exceed {MAX_DIR_BYTES} byte limit; delete stale entries first"
        ));
    }
    store
        .write_memory(path, content)
        .map_err(|e| e.to_string())?;
    Ok(ToolOutput::Plain(format!("wrote {path} ({lines} lines)")))
}

fn delete(store: &ArgosyStore, path: &str) -> Result<ToolOutput, String> {
    match store.delete_memory(path) {
        Ok(()) => Ok(ToolOutput::Plain(format!("deleted {path}"))),
        Err(craft_storage::argosy_store::ArgosyError::NotFound(_)) => {
            Err(format!("'{path}' does not exist"))
        }
        Err(e) => Err(e.to_string()),
    }
}

fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// Non-overlapping case-insensitive term occurrences, normalized by note
/// length so short, focused notes rank above long ones with the same hits.
fn score(content: &str, terms: &[String]) -> f32 {
    if terms.is_empty() {
        return 0.0;
    }
    let lower = content.to_ascii_lowercase();
    let mut hits = 0usize;
    for term in terms {
        if let Some(at) = lower.find(term.as_str()) {
            hits += 1;
            let mut rest = &lower[at + term.len()..];
            while let Some(next) = rest.find(term.as_str()) {
                hits += 1;
                rest = &rest[next + term.len()..];
            }
        }
    }
    if hits == 0 {
        return 0.0;
    }
    let len = lower.split_whitespace().count().max(1) as f32;
    hits as f32 / len
}

super::impl_tool!(
    Memory,
    kind = "edit",
    tier = super::registry::ToolTier::Core
);

impl ToolInvocation for Memory {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Memory::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Memory::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn store() -> ArgosyStore {
        let tmp = tempfile::tempdir().unwrap();
        ArgosyStore::open_or_init(&tmp.path().join("argosy"), "test").unwrap()
    }

    #[test]
    fn view_write_delete_round_trip() {
        let s = store();
        match view(&s, None).unwrap() {
            ToolOutput::Plain(text) => assert!(text.contains("No memories yet"), "got: {text}"),
            other => panic!("expected plain output, got {other:?}"),
        }

        write(&s, "conventions", "# Conventions\n\nuse argosy").unwrap();
        match view(&s, Some("conventions")).unwrap() {
            ToolOutput::Plain(text) => assert!(text.contains("use argosy")),
            other => panic!("expected plain output, got {other:?}"),
        }
        match view(&s, None).unwrap() {
            ToolOutput::Plain(text) => assert!(text.contains("conventions")),
            other => panic!("expected plain output, got {other:?}"),
        }
        delete(&s, "conventions").unwrap();
        assert!(delete(&s, "conventions").is_err());
    }

    #[tokio::test]
    async fn view_unknown_path_keyword_searches() {
        let s = store();
        write(&s, "login", "the login flow uses argosy sessions").unwrap();
        write(&s, "docker", "unrelated notes about containers").unwrap();
        match view(&s, Some("login flow")).unwrap() {
            ToolOutput::Plain(text) => {
                assert!(text.contains("login"), "got: {text}");
                assert!(text.contains("argosy sessions"));
            }
            other => panic!("expected plain output, got {other:?}"),
        }
    }

    #[test]
    fn write_enforces_line_cap() {
        let s = store();
        let big = "x\n".repeat(MAX_LINES_PER_NOTE + 1);
        let err = write(&s, "big", &big).unwrap_err();
        assert!(err.contains("exceeds"), "got: {err}");
    }

    #[test]
    fn write_enforces_dir_byte_cap() {
        let s = store();
        let err = write(&s, "big", &"x".repeat(MAX_DIR_BYTES + 1)).unwrap_err();
        assert!(err.contains("byte limit"), "got: {err}");
    }

    #[test_case("goal.md" ; "md_suffix_normalized")]
    #[test_case("nested/deep/note" ; "nested")]
    fn names_round_trip(name: &str) {
        let s = store();
        let name = normalize(name);
        write(&s, name, "body").unwrap();
        match view(&s, Some(name)).unwrap() {
            ToolOutput::Plain(text) => assert_eq!(text, "body"),
            other => panic!("expected plain output, got {other:?}"),
        }
    }
}
