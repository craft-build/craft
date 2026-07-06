//! Internal URL scheme dispatch for the `read` tool.
//!
//! Lets one interface (`read`) address things that today need separate tools,
//! cutting the model's tool vocabulary. Implemented schemes (no external deps):
//! - `skill://<name>`: read a discovered skill body (SKILL.md).
//! - `rule://<name>`: read a discovered rule body (`.craft/rules/<name>.md`).
//!   `rule://*` lists every discovered rule.
//! - `conflict://N`: read the Nth (1-indexed) merge-conflict hunk in the repo, numbered globally
//!   across all files in scan order.
//!   `conflict://*` lists every hunk across the repo with its global index.
//! - `agent://findings` / `agent://findings.<i>`: read structured review findings
//!   collected from subagents (composes with `report_finding` and `review`).
//! - `flow://<path>`: read a Flow workstream document by its path (the `path`
//!   field returned by `flow_search`). `flow://*` lists every document in the
//!   active workstream. Only resolves inside a Flow stage.
//!
//! Schemes that need external services (`pr://`, `issue://`, `diff://`) are
//! intentionally out of the initial scope.

use crate::ToolOutput;

use super::ToolContext;
use super::conflicts::collect_conflicts;
use super::relative_path;

pub(super) fn handles(path: &str) -> bool {
    path.contains("://")
        && path.split("://").next().is_some_and(|scheme| {
            matches!(scheme, "skill" | "rule" | "conflict" | "agent" | "flow")
        })
}

pub(super) async fn resolve(path: &str, ctx: &ToolContext) -> Result<ToolOutput, String> {
    let (scheme, rest) = path
        .split_once("://")
        .ok_or_else(|| format!("not an internal URL scheme: {path}"))?;
    match scheme {
        "skill" => resolve_skill(rest, ctx).await,
        "rule" => resolve_rule(rest),
        "conflict" => resolve_conflict(rest),
        "agent" => resolve_agent(rest, ctx),
        "flow" => resolve_flow(rest, ctx).await,
        other => Err(format!("unsupported scheme '{other}://'")),
    }
}

async fn resolve_skill(name: &str, ctx: &ToolContext) -> Result<ToolOutput, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("skill:// requires a skill name".into());
    }
    let discovery = crate::discovery::Discovery::from_env();
    let found = discovery
        .discover_dirs("skills", "SKILL.md")
        .into_iter()
        .find(|d| d.name == name);
    let discovered = match found {
        Some(d) => d,
        None => return Err(format!("skill '{name}' not found")),
    };
    let body = ctx
        .fs
        .read_text_file(&discovered.path)
        .await
        .map_err(|e| format!("failed to read skill '{name}': {e}"))?;
    Ok(ToolOutput::Plain(format!(
        "# skill://{name} ({})\n\n{body}",
        relative_path(&discovered.path.to_string_lossy())
    )))
}

fn resolve_rule(selector: &str) -> Result<ToolOutput, String> {
    let selector = selector.trim();
    let discovery = crate::discovery::Discovery::from_env();
    let rules = discovery.discover_files("rules", &["md"]);
    if rules.is_empty() {
        return Err("no rules found (looked for .craft/rules/*.md)".into());
    }

    if selector.is_empty() || selector == "*" {
        let mut out = format!("{} rule(s):\n", rules.len());
        for r in &rules {
            out.push_str(&format!(
                "- rule://{} ({})\n",
                r.name,
                relative_path(&r.path.to_string_lossy())
            ));
        }
        return Ok(ToolOutput::Plain(out));
    }

    let found = rules
        .into_iter()
        .find(|r| r.name == selector)
        .ok_or_else(|| format!("rule '{selector}' not found"))?;
    Ok(ToolOutput::Plain(format!(
        "# rule://{}\n\n{}",
        found.name,
        found.content.trim()
    )))
}

async fn resolve_flow(selector: &str, ctx: &ToolContext) -> Result<ToolOutput, String> {
    let selector = selector.trim();
    let Some(backend) = ctx.flow_search.as_ref() else {
        return Err(
            "flow:// is only available inside a Flow stage (no active workstream)".to_string(),
        );
    };
    let Some((project_id, workstream_id)) = backend.workstream() else {
        return Err(
            "flow:// is only available inside a Flow stage (no active workstream)".to_string(),
        );
    };
    if selector.is_empty() {
        return Err("flow:// requires a document path (or '*' to list all)".into());
    }
    if selector == "*" {
        let docs = backend.list_documents(&project_id, &workstream_id).await?;
        if docs.is_empty() {
            return Ok(ToolOutput::Plain(
                "no flow documents in this workstream".into(),
            ));
        }
        let mut out = format!("{} flow document(s) in this workstream:\n", docs.len());
        for p in &docs {
            out.push_str(&format!("- flow://{p}\n"));
        }
        return Ok(ToolOutput::Plain(out));
    }
    let body = backend
        .read_document(&project_id, &workstream_id, selector)
        .await?;
    Ok(ToolOutput::Plain(format!("# flow://{selector}\n\n{body}")))
}

fn resolve_agent(rest: &str, ctx: &ToolContext) -> Result<ToolOutput, String> {
    let rest = rest.trim();
    if !rest.starts_with("findings") {
        return Err(format!(
            "unsupported agent:// selector '{rest}'; use agent://findings or agent://findings.<i>"
        ));
    }
    let store = ctx
        .findings_store
        .as_ref()
        .ok_or_else(|| "no findings store available for this context".to_string())?;
    let entries = store
        .lock()
        .map_err(|e| format!("findings store lock error: {e}"))?
        .snapshot();
    if entries.is_empty() {
        return Ok(ToolOutput::Plain("no agent findings recorded".into()));
    }

    let suffix = rest.trim_start_matches("findings");
    if let Some(idx_str) = suffix.strip_prefix('.') {
        let idx: usize = idx_str
            .parse()
            .map_err(|_| format!("agent://findings.<i> expects an index, got '{idx_str}'"))?;
        let entry = entries.get(idx).ok_or_else(|| {
            format!(
                "agent://findings.{idx} out of range ({} recorded)",
                entries.len()
            )
        })?;
        return Ok(ToolOutput::Plain(format_agent_finding(idx, entry)));
    }

    let mut out = format!("{} finding(s) from agents:\n", entries.len());
    for (i, entry) in entries.iter().enumerate() {
        out.push_str(&format!(
            "- agent://findings.{i} [{}] {} ({}:{})\n",
            entry.finding.priority,
            entry.finding.title,
            relative_path(&entry.finding.file_path),
            entry.finding.line_start,
        ));
    }
    Ok(ToolOutput::Plain(out))
}

fn format_agent_finding(idx: usize, entry: &crate::agent::findings_store::StoredFinding) -> String {
    format!(
        "# agent://findings.{idx} [{}]\n\ntitle: {}\nfile: {}:{}-{}\n\n{}",
        entry.finding.priority,
        entry.finding.title,
        entry.finding.file_path,
        entry.finding.line_start,
        entry.finding.line_end,
        entry.finding.body.trim(),
    )
}

fn resolve_conflict(selector: &str) -> Result<ToolOutput, String> {
    let selector = selector.trim();
    let cwd = std::env::current_dir().map_err(|e| format!("cwd error: {e}"))?;
    let conflicts = collect_conflicts(&cwd.to_string_lossy());
    if conflicts.is_empty() {
        return Ok(ToolOutput::Plain("no merge conflicts found".into()));
    }

    let flat: Vec<(&String, &super::conflicts::ConflictMarker, usize)> = conflicts
        .iter()
        .flat_map(|(file, markers)| {
            markers
                .iter()
                .enumerate()
                .map(move |(idx, m)| (file, m, idx + 1))
        })
        .collect();
    let total = flat.len();

    if selector == "*" {
        let mut out = format!(
            "{files} file(s) with conflicts ({total} hunk(s)):\n",
            files = conflicts.len()
        );
        for (n, (file, m, _)) in flat.iter().enumerate() {
            out.push_str(&format!(
                "\n{file} conflict://{} (lines {} - {}: {} vs {})\n",
                n + 1,
                m.start_line,
                m.end_line,
                m.our_branch,
                m.their_branch
            ));
        }
        return Ok(ToolOutput::Plain(out));
    }

    let n: usize = selector
        .parse()
        .map_err(|_| format!("conflict:// expects an index (1-based) or '*', got '{selector}'"))?;
    let idx = n
        .checked_sub(1)
        .ok_or_else(|| format!("conflict://{n} not found; {total} conflict hunk(s) in scope"))?;
    let (file, m, _) = flat
        .get(idx)
        .ok_or_else(|| format!("conflict://{n} not found; {total} conflict hunk(s) in scope"))?;
    let hunk = extract_hunk(&cwd.to_string_lossy(), file, m);
    Ok(ToolOutput::Plain(format!(
        "{file} conflict://{n} (lines {} - {})\n{hunk}",
        m.start_line, m.end_line
    )))
}

fn extract_hunk(cwd: &str, rel: &str, marker: &super::conflicts::ConflictMarker) -> String {
    let abs = if std::path::Path::new(rel).is_absolute() {
        rel.to_string()
    } else {
        std::path::Path::new(cwd)
            .join(rel)
            .to_string_lossy()
            .into_owned()
    };
    let Ok(content) = std::fs::read_to_string(&abs) else {
        return format!("(could not read {rel})");
    };
    let lines: Vec<&str> = content.lines().collect();
    let start = marker.start_line.saturating_sub(1).min(lines.len());
    let end = marker.end_line.min(lines.len());
    let mut out = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        let display_line = start + i + 1;
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{display_line}: {line}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use crate::agent::findings_store::FindingsStore;
    use crate::tools::flow_search::{FlowSearchBackend, ListFuture, ReadFuture, SearchFuture};
    use crate::tools::test_support::{stub_ctx, stub_ctx_with};
    use crate::types::{Finding, Priority};
    use std::sync::Arc;

    #[test]
    fn handles_recognizes_schemes() {
        for path in [
            "skill://foo",
            "rule://bar",
            "rule://*",
            "conflict://1",
            "conflict://*",
            "agent://findings",
            "flow://goal.md",
            "flow://*",
        ] {
            assert!(handles(path), "{path} should be recognized");
        }
    }

    #[test]
    fn handles_ignores_non_schemes() {
        for path in ["/abs/path", "./rel", "README.md"] {
            assert!(!handles(path), "{path} should not be recognized");
        }
    }

    #[test]
    fn handles_rejects_unsupported_scheme() {
        assert!(!handles("pr://1"));
    }

    #[tokio::test]
    async fn resolve_rule_empty_errors() {
        let ctx = stub_ctx(&AgentMode::Build);
        assert!(resolve("rule://nonexistent", &ctx).await.is_err());
    }

    #[tokio::test]
    async fn resolve_rule_discovered_file_reads_body() {
        let dir = tempfile::TempDir::new().unwrap();
        let rules_dir = dir.path().join(".craft").join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("no-leak.md"),
            "# No Box::leak\n\nDon't leak.",
        )
        .unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let out = resolve("rule://no-leak", &ctx).await;
        let _ = std::env::set_current_dir(&prev);
        let out = out.unwrap();
        let text = out.as_text().to_string();
        assert!(text.contains("rule://no-leak"), "{text}");
        assert!(text.contains("No Box::leak"), "{text}");
    }

    #[tokio::test]
    async fn resolve_agent_findings_empty_without_store() {
        let ctx = stub_ctx(&AgentMode::Build);
        assert!(resolve("agent://findings", &ctx).await.is_err());
    }

    #[tokio::test]
    async fn resolve_agent_findings_lists_and_indexes() {
        let ctx = stub_ctx(&AgentMode::Build);
        let mut agent_ctx = ctx;
        let store = FindingsStore::new_shared();
        store.lock().unwrap().extend(
            "review",
            [Finding {
                title: "unused import".into(),
                body: "drop it".into(),
                priority: Priority::P2,
                confidence: 0.9,
                file_path: "src/lib.rs".into(),
                line_start: 4,
                line_end: 4,
                rule_ids: vec![],
                suggestion: None,
            }],
        );
        agent_ctx.findings_store = Some(store);

        let list = resolve("agent://findings", &agent_ctx).await.unwrap();
        let list_text = list.as_text().to_string();
        assert!(list_text.contains("1 finding"), "{list_text}");
        assert!(list_text.contains("agent://findings.0"), "{list_text}");

        let one = resolve("agent://findings.0", &agent_ctx).await.unwrap();
        let one_text = one.as_text().to_string();
        assert!(one_text.contains("unused import"), "{one_text}");
        assert!(one_text.contains("src/lib.rs:4"), "{one_text}");
    }

    #[tokio::test]
    async fn resolve_agent_findings_out_of_range_errors() {
        let ctx = stub_ctx(&AgentMode::Build);
        let mut agent_ctx = ctx;
        let store = FindingsStore::new_shared();
        store.lock().unwrap().extend(
            "review",
            [Finding {
                title: "x".into(),
                body: "y".into(),
                priority: Priority::P3,
                confidence: 0.1,
                file_path: "a.rs".into(),
                line_start: 1,
                line_end: 1,
                rule_ids: vec![],
                suggestion: None,
            }],
        );
        agent_ctx.findings_store = Some(store);
        let err = resolve("agent://findings.5", &agent_ctx).await.unwrap_err();
        assert!(err.contains("out of range"), "{err}");
    }

    #[tokio::test]
    async fn resolve_agent_unsupported_selector_errors() {
        let ctx = stub_ctx(&AgentMode::Build);
        let err = resolve("agent://history", &ctx).await.unwrap_err();
        assert!(err.contains("unsupported agent:// selector"), "{err}");
    }

    const FLOW_WS: &str = "ws-flow";

    /// Backend stub for `flow://` tests: returns canned docs keyed by path.
    struct FlowStub {
        docs: Vec<(&'static str, &'static str)>,
    }

    impl FlowSearchBackend for FlowStub {
        fn workstream(&self) -> Option<(String, String)> {
            Some(("proj".to_string(), FLOW_WS.to_string()))
        }
        fn search<'a>(
            &'a self,
            _project_id: &'a str,
            _workstream_id: &'a str,
            _query: &'a str,
            _k: usize,
        ) -> SearchFuture<'a> {
            Box::pin(async move { Ok(Vec::new()) })
        }
        fn read_document<'a>(
            &'a self,
            _project_id: &'a str,
            _workstream_id: &'a str,
            rel_path: &'a str,
        ) -> ReadFuture<'a> {
            let body = self
                .docs
                .iter()
                .find(|(p, _)| *p == rel_path)
                .map(|(_, c)| (*c).to_string())
                .unwrap_or_else(|| format!("(missing {rel_path})"));
            Box::pin(async move { Ok(body) })
        }
        fn list_documents<'a>(
            &'a self,
            _project_id: &'a str,
            _workstream_id: &'a str,
        ) -> ListFuture<'a> {
            let paths: Vec<String> = self.docs.iter().map(|(p, _)| (*p).to_string()).collect();
            Box::pin(async move { Ok(paths) })
        }
    }

    fn flow_ctx(backend: Option<FlowStub>) -> ToolContext {
        let mut ctx = stub_ctx_with(
            &AgentMode::Flow(FLOW_WS.to_string()),
            None,
            Some("flow:test"),
        );
        ctx.flow_search = backend.map(|b| Arc::new(b) as Arc<dyn FlowSearchBackend>);
        ctx
    }

    #[tokio::test]
    async fn resolve_flow_without_backend_errors_with_guidance() {
        let ctx = flow_ctx(None);
        let err = resolve("flow://goal.md", &ctx).await.unwrap_err();
        assert!(
            err.contains("only available inside a Flow stage"),
            "expected guidance, got: {err}"
        );
    }

    #[tokio::test]
    async fn resolve_flow_list_shows_all_documents() {
        let ctx = flow_ctx(Some(FlowStub {
            docs: vec![("goal.md", "goal body"), ("plan.md", "plan body")],
        }));
        let out = resolve("flow://*", &ctx).await.unwrap();
        let text = out.as_text().to_string();
        assert!(text.contains("2 flow document"), "{text}");
        assert!(text.contains("flow://goal.md"), "{text}");
        assert!(text.contains("flow://plan.md"), "{text}");
    }

    #[tokio::test]
    async fn resolve_flow_doc_returns_body_with_header() {
        let ctx = flow_ctx(Some(FlowStub {
            docs: vec![("goal.md", "acceptance: ...")],
        }));
        let out = resolve("flow://goal.md", &ctx).await.unwrap();
        let text = out.as_text().to_string();
        assert!(text.contains("# flow://goal.md"), "{text}");
        assert!(text.contains("acceptance: ..."), "{text}");
    }

    #[tokio::test]
    async fn resolve_flow_empty_selector_errors() {
        let ctx = flow_ctx(Some(FlowStub { docs: vec![] }));
        let err = resolve("flow://", &ctx).await.unwrap_err();
        assert!(err.contains("requires a document path"), "{err}");
    }
}
