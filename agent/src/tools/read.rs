use rig_core::tool::{IntoToolOutput, PortableTool, ToolExecutionError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{
    MAX_LINE_BYTES, MAX_OUTPUT_BYTES, Result, Workspace, clip, failure, invalid, not_found,
    read_bytes, text,
};
use crate::skills::Discovery;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    /// Workspace-relative path, an absolute path inside the workspace, or a
    /// resource URL: `skill://<name>`, `argosy://<uri>`, `mcp://<server>/<uri>`,
    /// or a resource URI published by a connected MCP server.
    pub path: String,
    /// First line, one-based.
    #[serde(default = "default_offset")]
    pub offset: usize,
    /// Lines to return (default 200, maximum 2000; 0 means 2000).
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_offset() -> usize {
    1
}
fn default_limit() -> usize {
    200
}

#[derive(Debug)]
pub struct ReadLine {
    pub number: usize,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug)]
pub struct ReadOutput {
    pub path: String,
    pub lines: Vec<ReadLine>,
    pub total_lines: usize,
    pub next_offset: Option<usize>,
    /// Subdirectory instruction files discovered for the read file's parent
    /// directory, as `(canonical_path, content)` pairs.
    pub instructions: Vec<(String, String)>,
}

impl IntoToolOutput for ReadOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        let mut text = self
            .lines
            .iter()
            .map(|line| {
                format!(
                    "{}: {}{}",
                    line.number,
                    line.text,
                    if line.truncated { "..." } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(offset) = self.next_offset {
            text.push_str(&format!(
                "\n\n...\n\nTruncated lines: {offset}-{}. Use offset={offset} to read further.",
                self.total_lines
            ));
        }
        for (path, content) in &self.instructions {
            text.push_str(&format!("\n\n---\nInstructions from: {path}\n{content}"));
        }
        Ok(ToolOutput::text(text))
    }
}

#[derive(Clone)]
pub struct Read(pub Workspace);

impl Read {
    fn execute(workspace: &Workspace, args: ReadArgs) -> Result<ReadOutput> {
        if let Some(result) = read_skill(&args.path, &skill_discovery(workspace), workspace) {
            return result;
        }
        let path = workspace.file(&args.path)?;
        let contents = text(read_bytes(&path)?)?;
        let total_lines = contents.lines().count();
        if args.offset > total_lines.saturating_add(1) {
            return Err(invalid(format!(
                "offset exceeds end of file ({total_lines} lines)"
            )));
        }
        let limit = if args.limit == 0 { 2000 } else { args.limit };
        let (lines, _, next) = page_lines(&contents, args.offset, limit);
        let instructions = if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(crate::instructions::is_instruction_file)
        {
            Vec::new()
        } else {
            let parent = path.parent().unwrap_or(&path);
            crate::instructions::find_subdirectory_instructions(
                parent,
                workspace.root(),
                &workspace.loaded_instructions,
            )
        };
        Ok(ReadOutput {
            path: workspace.display(&path),
            lines,
            total_lines,
            next_offset: next,
            instructions,
        })
    }
}

/// The shared paging loop for file and resource bodies: one-based line
/// numbers, [`MAX_LINE_BYTES`] line clipping, an [`MAX_OUTPUT_BYTES`] output
/// budget, and the continuation offset when more lines remain.
fn page_lines(body: &str, offset: usize, limit: usize) -> (Vec<ReadLine>, usize, Option<usize>) {
    let total_lines = body.lines().count();
    let mut lines = Vec::new();
    let mut bytes = 0;
    for (index, line) in body.lines().enumerate().skip(offset - 1).take(limit) {
        let (line, truncated) = clip(line, MAX_LINE_BYTES);
        if bytes + line.len() > MAX_OUTPUT_BYTES {
            break;
        }
        bytes += line.len();
        lines.push(ReadLine {
            number: index + 1,
            text: line.into(),
            truncated,
        });
    }
    let next_offset = (offset + lines.len() <= total_lines).then_some(offset + lines.len());
    (lines, total_lines, next_offset)
}

/// Resource-URL modes of the read tool (the `mcp_read` merge):
/// `argosy://` through the in-process argosy link, `mcp://<server>/<uri>`
/// and verbatim resource URIs through the MCP resource index. `skill://`
/// stays with `execute` (it reads the discovery tree like a file); plain
/// paths return `None` so the filesystem walk proceeds.
async fn read_url(workspace: &Workspace, args: &ReadArgs) -> Option<Result<ReadOutput>> {
    if args.path.starts_with("skill://") {
        return None;
    }
    if let Some(rest) = args.path.strip_prefix("argosy://") {
        return Some(read_argosy(rest, workspace, args).await);
    }
    if !is_resource_uri(&args.path) {
        return None;
    }
    Some(read_mcp(workspace, args).await)
}

/// The builtin `argosy://` namespace, served by the in-process argosy
/// link — no MCP hop, so it resolves with no argosy server connected.
async fn read_argosy(rest: &str, workspace: &Workspace, args: &ReadArgs) -> Result<ReadOutput> {
    if rest.trim().is_empty() {
        return Err(invalid(
            "argosy:// requires a resource URI (argosy://_argosys lists the active argosys)",
        ));
    }
    let uri = format!("argosy://{rest}");
    let root = workspace.root().to_path_buf();
    let target = uri.clone();
    let body = tokio::task::spawn_blocking(move || {
        crate::knowledge::ArgosyService::global().read_resource(&root, &target)
    })
    .await
    .map_err(|error| failure(format!("argosy read task failed: {error}")))?
    .map_err(failure)?;
    resource_page(&uri, &body, args)
}

/// One MCP resource read, in either addressing form:
/// - `mcp://<server>/<uri>` — the qualified form; server names are
///   alphanumeric plus `-`, so the first `/` ends the name.
/// - any other resource URI — verbatim, matched against the published
///   resource index; it must have exactly one publishing server.
async fn read_mcp(workspace: &Workspace, args: &ReadArgs) -> Result<ReadOutput> {
    if args.path.starts_with("mcp://") && qualified_resource(&args.path).is_none() {
        return Err(invalid(
            "mcp:// URLs take the form mcp://<server>/<uri> (e.g. mcp://srv/file:///notes.txt)",
        ));
    }
    let Some(handle) = workspace.mcp() else {
        return Err(invalid(format!(
            "{} addresses an MCP resource but no MCP servers are connected; the read tool \
             takes workspace paths and skill://, argosy://, or mcp:// URLs",
            args.path
        )));
    };
    let snapshot = handle.reader().load_full();
    let (server, uri) = match qualified_resource(&args.path) {
        Some(pair) => pair,
        None => {
            let mut servers: Vec<String> = snapshot
                .resources
                .iter()
                .filter(|resource| resource.uri == args.path)
                .map(|resource| resource.server.clone())
                .collect();
            servers.sort();
            servers.dedup();
            match servers.len() {
                0 => {
                    return Err(not_found(unknown_resource_hint(
                        &args.path,
                        &snapshot.resources,
                    )));
                }
                1 => (servers.remove(0), args.path.clone()),
                _ => {
                    return Err(invalid(format!(
                        "{} is published by several servers ({}); disambiguate with \
                         mcp://<server>/{}",
                        args.path,
                        servers.join(", "),
                        args.path
                    )));
                }
            }
        }
    };
    let body = handle
        .read_resource(&server, &uri)
        .await
        .map_err(|error| failure(error.to_string()))?;
    resource_page(&format!("mcp://{server}/{uri}"), &body, args)
}

/// The not-found error for a verbatim URI: names the URI and, when any
/// server publishes resources, lists them so the model can pick one.
fn unknown_resource_hint(uri: &str, resources: &[crate::mcp::McpResourceInfo]) -> String {
    let mut hint = format!("{uri}: no MCP server publishes this URI");
    if resources.is_empty() {
        hint.push_str(" and no MCP resources are published");
    } else {
        hint.push_str("; published resources:");
        for resource in resources.iter().take(20) {
            hint.push_str(&format!(
                "\n- {} (server {})",
                resource.uri, resource.server
            ));
        }
        if resources.len() > 20 {
            hint.push_str(&format!("\n- … and {} more", resources.len() - 20));
        }
    }
    hint
}

/// Paginate a resource body exactly like a file read.
fn resource_page(path: &str, body: &str, args: &ReadArgs) -> Result<ReadOutput> {
    let total_lines = body.lines().count();
    if args.offset > total_lines.saturating_add(1) {
        return Err(invalid(format!(
            "offset exceeds end of resource ({total_lines} lines)"
        )));
    }
    let limit = if args.limit == 0 { 2000 } else { args.limit };
    let (lines, _, next_offset) = page_lines(body, args.offset, limit);
    Ok(ReadOutput {
        path: path.to_string(),
        lines,
        total_lines,
        next_offset,
        instructions: Vec::new(),
    })
}

/// `mcp://<server>/<uri>` split into its parts; the resource URI keeps any
/// scheme of its own (`mcp://srv/file:///notes.txt` → `file:///notes.txt`).
fn qualified_resource(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("mcp://")?;
    let (server, uri) = rest.split_once('/')?;
    (!server.is_empty() && !uri.is_empty()).then(|| (server.to_string(), uri.to_string()))
}

/// Whether `path` is an RFC 3986-shaped `scheme://…` URL rather than a
/// filesystem path — those route to the resource namespaces.
fn is_resource_uri(path: &str) -> bool {
    match path.split_once("://") {
        Some((scheme, rest)) => {
            !rest.is_empty()
                && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme[1..]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        None => false,
    }
}

/// The permission scopes for a read-tool resource URL, when the path is
/// one: `argosy:<uri>` for the builtin namespace, `mcp:<server>:<uri>` for
/// the qualified form, `mcp:*:<uri>` for a verbatim URI (the publishing
/// server is only known at dispatch). `skill://` and plain file paths
/// return `None` and keep the default scope.
pub(crate) fn resource_scope(path: &str) -> Option<Vec<String>> {
    if path.starts_with("skill://") || !is_resource_uri(path) {
        return None;
    }
    if let Some(rest) = path.strip_prefix("argosy://") {
        return Some(vec![format!("argosy:{rest}")]);
    }
    Some(vec![match qualified_resource(path) {
        Some((server, uri)) => format!("mcp:{server}:{uri}"),
        None => format!("mcp:*:{path}"),
    }])
}

/// `skill://<name>` internal URL scheme: resolve a discovered skill and
/// return its full SKILL.md body with a location header. Returns `None`
/// when the path is not a skill URL, so ordinary reads proceed.
fn read_skill(
    path: &str,
    discovery: &Discovery,
    workspace: &Workspace,
) -> Option<Result<ReadOutput>> {
    let name = path.strip_prefix("skill://")?;
    let name = name.trim();
    if name.is_empty() {
        return Some(Err(invalid("skill:// requires a skill name")));
    }
    let Some(skill) = discovery.find(name) else {
        return Some(Err(invalid(format!(
            "skill '{name}' not found{}",
            discovery.skill_list()
        ))));
    };
    let is_builtin = skill.scope.is_builtin();
    let location = if is_builtin {
        skill.location()
    } else {
        workspace.display(&skill.path)
    };
    let body = if is_builtin {
        skill.content
    } else {
        let not_symlink = std::fs::symlink_metadata(&skill.path)
            .map(|m| m.is_file())
            .unwrap_or(false);
        if !not_symlink {
            return Some(Err(invalid(format!(
                "skill '{name}' marker is not a regular file"
            ))));
        }
        match read_bytes(&skill.path).and_then(text) {
            Ok(body) => body,
            Err(error) => {
                return Some(Err(invalid(format!(
                    "failed to read skill '{name}': {error}"
                ))));
            }
        }
    };

    let mut lines = Vec::new();
    let mut bytes = 0;
    for (index, line) in body.lines().enumerate() {
        let (line, truncated) = clip(line, MAX_LINE_BYTES);
        if bytes + line.len() > MAX_OUTPUT_BYTES {
            break;
        }
        bytes += line.len();
        lines.push(ReadLine {
            number: index + 1,
            text: line.into(),
            truncated,
        });
    }
    let total_lines = body.lines().count();
    Some(Ok(ReadOutput {
        path: format!("skill://{name} ({location})"),
        lines,
        total_lines,
        next_offset: None,
        instructions: Vec::new(),
    }))
}

/// Skill discovery rooted at the workspace, so `skill://` reads follow the
/// project the session is working in.
fn skill_discovery(workspace: &Workspace) -> Discovery {
    Discovery::new(
        workspace.root().to_path_buf(),
        crate::paths::home(),
        crate::paths::xdg_config_dir().ok(),
    )
}

impl PortableTool for Read {
    const NAME: &'static str = "read";
    type Args = ReadArgs;
    type Output = ReadOutput;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Read UTF-8 text with one-based line numbers. Use offset/limit for paging and the \
         returned truncation hint to continue. Files are capped at 8 MiB; lines longer than \
         2048 bytes end with '...'. No binary files, symlinks, or paths outside the workspace. \
         `path` also resolves resource URLs: 'skill://<name>' (a discovered skill body), \
         'argosy://<uri>' (a concept or pseudo-resource of the project's in-process argosy; \
         argosy://_argosys lists the active argosys), 'mcp://<server>/<uri>' (an MCP server \
         resource), or any resource URI published by a connected MCP server."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ReadArgs)).unwrap_or_else(|error| {
            tracing::error!(
                tool = "read",
                %error,
                "tool schema serialization failed; serving a bare object schema"
            );
            serde_json::json!({"type": "object"})
        })
    }

    async fn call(&self, args: Self::Args) -> Result<ReadOutput> {
        if args.offset == 0 || args.limit > 2000 {
            return Err(invalid(
                "offset must be >= 1 and limit must be between 0 and 2000",
            ));
        }
        // Resource URLs bypass the blocking filesystem walk: they read
        // in-process or remote services, never the workspace tree.
        if let Some(result) = read_url(&self.0, &args).await {
            return result;
        }
        self.0
            .run(move |workspace| Self::execute(workspace, args))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn workspace() -> (TempDir, Workspace) {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        let ws = Workspace::new(tmp.path()).unwrap();
        (tmp, ws)
    }

    fn discovery(root: &std::path::Path) -> Discovery {
        // Canonicalize to match `Workspace::new`, so `display` strips the root.
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        Discovery::new(root, None, None)
    }

    #[test]
    fn skill_url_reads_builtin_body() {
        let (tmp, ws) = workspace();
        let out = read_skill("skill://run", &discovery(tmp.path()), &ws)
            .expect("skill url handled")
            .expect("builtin found");
        assert_eq!(out.path, "skill://run (<builtin>/skills/run/SKILL.md)");
        assert_eq!(
            out.total_lines,
            crate::skills::builtin("run").unwrap().lines().count()
        );
        assert_eq!(out.lines[0].text, "---");
        assert_eq!(out.next_offset, None);
    }

    #[test]
    fn skill_url_reads_project_skill_relative_location() {
        let (tmp, ws) = workspace();
        let path = tmp.path().join(".craft/skills/audit/SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "---\nname: audit\n---\naudit body").unwrap();
        let out = read_skill("skill://audit", &discovery(tmp.path()), &ws)
            .expect("skill url handled")
            .expect("project skill found");
        assert_eq!(out.path, "skill://audit (.craft/skills/audit/SKILL.md)");
        assert!(out.lines.iter().any(|l| l.text.contains("audit body")));
    }

    #[test]
    fn unknown_skill_errors_with_list() {
        let (tmp, ws) = workspace();
        let error = read_skill("skill://nope", &discovery(tmp.path()), &ws)
            .expect("skill url handled")
            .expect_err("not found");
        let message = error.to_string();
        assert!(message.contains("skill 'nope' not found"), "{message}");
        assert!(message.contains("- verify:"), "{message}");
    }

    #[test]
    fn empty_skill_selector_errors() {
        let (tmp, ws) = workspace();
        assert!(
            read_skill("skill://", &discovery(tmp.path()), &ws)
                .expect("handled")
                .is_err()
        );
    }

    #[test]
    fn non_skill_paths_fall_through() {
        let (tmp, ws) = workspace();
        assert!(read_skill("src/main.rs", &discovery(tmp.path()), &ws).is_none());
        assert!(read_skill("http://example.com", &discovery(tmp.path()), &ws).is_none());
    }

    #[test]
    fn builtin_read_ignores_paging() {
        let (tmp, ws) = workspace();
        let out = read_skill("skill://verify", &discovery(tmp.path()), &ws)
            .expect("handled")
            .expect("found");
        assert_eq!(out.lines.len(), out.total_lines);
    }

    #[test]
    fn resource_uris_are_classified() {
        assert!(is_resource_uri("mcp://srv/file:///notes.txt"));
        assert!(is_resource_uri("file:///notes.txt"));
        assert!(is_resource_uri("argosy://catalog"));
        assert!(is_resource_uri("db+pg://tables"));
        assert!(!is_resource_uri("src/main.rs"));
        assert!(!is_resource_uri("a/b://c"));
        assert!(!is_resource_uri("://x"));
        assert!(!is_resource_uri("argosy://"));
    }

    #[test]
    fn qualified_resources_split_on_first_slash() {
        assert_eq!(
            qualified_resource("mcp://srv/file:///notes.txt"),
            Some(("srv".into(), "file:///notes.txt".into()))
        );
        assert_eq!(qualified_resource("mcp://srv"), None);
        assert_eq!(qualified_resource("mcp:///x"), None);
        assert_eq!(qualified_resource("mcp://srv/"), None);
        assert_eq!(qualified_resource("file:///x"), None);
    }

    #[tokio::test]
    async fn malformed_mcp_url_errors_with_the_expected_form() {
        let (_tmp, ws) = workspace();
        let error = invoke_read(&ws, "mcp://srv").await.unwrap_err();
        assert!(error.to_string().contains("mcp://<server>/<uri>"));
    }

    #[test]
    fn resource_scopes_cover_namespaces() {
        assert_eq!(
            resource_scope("argosy://catalog").unwrap(),
            vec!["argosy:catalog".to_string()]
        );
        assert_eq!(
            resource_scope("argosy://local/memory/gotchas").unwrap(),
            vec!["argosy:local/memory/gotchas".to_string()]
        );
        assert_eq!(
            resource_scope("mcp://srv/file:///notes.txt").unwrap(),
            vec!["mcp:srv:file:///notes.txt".to_string()]
        );
        assert_eq!(
            resource_scope("file:///notes.txt").unwrap(),
            vec!["mcp:*:file:///notes.txt".to_string()]
        );
        assert_eq!(resource_scope("skill://run"), None);
        assert_eq!(resource_scope("src/lib.rs"), None);
        assert_eq!(resource_scope("/abs/path.rs"), None);
    }

    #[test]
    fn resource_pages_like_file_pages() {
        let args = ReadArgs {
            path: "file:///notes.txt".into(),
            offset: 2,
            limit: 1,
        };
        let out = resource_page("file:///notes.txt", "first\r\nβeta\r\nlast", &args).unwrap();
        assert_eq!(out.total_lines, 3);
        assert_eq!(out.lines[0].number, 2);
        assert_eq!(out.lines[0].text, "βeta");
        assert_eq!(out.next_offset, Some(3));
        assert_eq!(out.path, "file:///notes.txt");
    }

    #[tokio::test]
    async fn empty_argosy_selector_errors_without_touching_the_service() {
        let (_tmp, ws) = workspace();
        let error = invoke_read(&ws, "argosy://").await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("argosy:// requires a resource URI")
        );
    }

    #[tokio::test]
    async fn resource_uri_without_mcp_handle_errors_with_guidance() {
        let (_tmp, ws) = workspace();
        let error = invoke_read(&ws, "file:///notes.txt").await.unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("no MCP servers are connected"),
            "{message}"
        );
    }

    async fn invoke_read(ws: &Workspace, path: &str) -> Result<ReadOutput> {
        use rig_core::tool::PortableTool;
        Read(ws.clone())
            .call(ReadArgs {
                path: path.into(),
                offset: 1,
                limit: 200,
            })
            .await
    }
}
