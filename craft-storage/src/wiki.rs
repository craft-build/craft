//! In-project local wiki knowledge base. Lives under `<project_root>/.wiki/`:
//!
//! ```text
//! .wiki/
//! ├── pages/              # hand-authored or tool-appended markdown pages
//! │   └── <slug>.md
//! ├── ingested-sources/   # structured notes from ingested files
//! │   └── <slug>.md
//! ├── log.md              # append-only dated log of ingest/edit events
//! └── index.md            # generated: links all pages + sources by first-H1 title
//! ```
//!
//! Plain markdown on purpose: diff-friendly, committable, hand-editable.
//! Distinct from `flow` (state-dir, id-addressed, machine-generated) and the
//! Lua `memory` plugin (curated, bulk-loaded): wiki is in-project, path-addressed,
//! and meant to be read by humans as much as by the agent.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{StorageError, atomic_write};

const WIKI_DIR_NAME: &str = ".wiki";
const PAGES_DIR_NAME: &str = "pages";
const SOURCES_DIR_NAME: &str = "ingested-sources";
const LOG_FILE_NAME: &str = "log.md";
const INDEX_FILE_NAME: &str = "index.md";
const MD_EXT: &str = ".md";
/// Soft cap on the excerpt embedded in an ingested-source note. Keeps notes
/// skimmable without truncating mid-line.
const MAX_EXCERPT_LINES: usize = 40;

#[derive(Debug, thiserror::Error)]
pub enum WikiError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid slug: {0}")]
    InvalidSlug(String),
    #[error("path traversal outside wiki directory is not allowed: {0}")]
    PathTraversal(String),
}

/// What kind of wiki entry a listing refers to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ListingKind {
    Page,
    Source,
}

/// One row of `wiki list`: slug, title, and whether it is a page or source note.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listing {
    pub slug: String,
    pub title: String,
    pub kind: ListingKind,
}

/// Payload for `write_source_note`. The timestamp is an opaque RFC3339 string
/// produced by the caller (the CLI/agent layer that owns the datetime crate);
/// storage stays free of datetime dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceNote {
    pub slug: String,
    pub title: String,
    pub source_path: String,
    pub ingested_at: String,
    pub summary: String,
    pub excerpt: String,
    pub linked_pages: Vec<String>,
}

/// In-project wiki store rooted at `<project_root>/.wiki/`.
pub struct WikiStore {
    root: PathBuf,
}

impl WikiStore {
    /// Open (lazily creating) the wiki under `<project_root>/.wiki/`.
    pub fn open(project_root: &Path) -> Result<Self, WikiError> {
        let root = project_root.join(WIKI_DIR_NAME);
        fs::create_dir_all(root.join(PAGES_DIR_NAME))?;
        fs::create_dir_all(root.join(SOURCES_DIR_NAME))?;
        Ok(Self { root })
    }

    /// Construct a store rooted at an explicit `.wiki` directory (tests).
    pub fn from_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/pages/<slug>.md`. Slugs are restricted to `[a-z0-9-]+` and
    /// rejected on any traversal attempt so a caller-supplied slug can never
    /// escape the pages directory.
    fn page_path(&self, slug: &str) -> Result<PathBuf, WikiError> {
        validate_slug(slug)?;
        Ok(self
            .root
            .join(PAGES_DIR_NAME)
            .join(format!("{slug}{MD_EXT}")))
    }

    fn source_path(&self, slug: &str) -> Result<PathBuf, WikiError> {
        validate_slug(slug)?;
        Ok(self
            .root
            .join(SOURCES_DIR_NAME)
            .join(format!("{slug}{MD_EXT}")))
    }

    /// Read a page by slug. Falls back to the matching ingested-source note if
    /// no page exists, so `wiki show <id>` resolves either kind.
    pub fn read_page(&self, slug: &str) -> Result<String, WikiError> {
        let page = self.page_path(slug)?;
        match fs::read_to_string(&page) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let src = self.source_path(slug)?;
                fs::read_to_string(&src).map_err(|_| WikiError::NotFound(slug.to_string()))
            }
            Err(e) => Err(WikiError::Io(e)),
        }
    }

    /// Append `body` to a page, creating it if missing. Inserts a blank-line
    /// separator between the existing tail and the new body so successive
    /// appends stay readable.
    pub fn append_page(&self, slug: &str, body: &str) -> Result<(), WikiError> {
        let path = self.page_path(slug)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let prev = fs::read_to_string(&path).unwrap_or_default();
        let updated = if prev.is_empty() {
            body.to_string()
        } else {
            let needs_sep = !prev.ends_with('\n');
            let sep = if needs_sep { "\n\n" } else { "\n" };
            format!("{prev}{sep}{body}")
        };
        Ok(atomic_write(&path, updated.as_bytes())?)
    }

    /// Write a source note to `ingested-sources/<slug>.md` atomically.
    pub fn write_source_note(&self, note: &SourceNote) -> Result<(), WikiError> {
        let path = self.source_path(&note.slug)?;
        let rendered = render_source_note(note);
        Ok(atomic_write(&path, rendered.as_bytes())?)
    }

    /// List all pages and ingested sources, each with its title.
    pub fn list(&self) -> Result<Vec<Listing>, WikiError> {
        let mut out = Vec::new();
        collect_listings(&self.root.join(PAGES_DIR_NAME), ListingKind::Page, &mut out)?;
        collect_listings(
            &self.root.join(SOURCES_DIR_NAME),
            ListingKind::Source,
            &mut out,
        )?;
        out.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(out)
    }

    /// Read the append-only event log. Returns an empty string when absent.
    pub fn read_log(&self) -> Result<String, WikiError> {
        let path = self.root.join(LOG_FILE_NAME);
        match fs::read_to_string(&path) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(WikiError::Io(e)),
        }
    }

    /// Append a single line to the event log.
    pub fn append_log(&self, entry: &str) -> Result<(), WikiError> {
        let path = self.root.join(LOG_FILE_NAME);
        let prev = fs::read_to_string(&path).unwrap_or_default();
        let line = if entry.ends_with('\n') {
            entry.to_string()
        } else {
            format!("{entry}\n")
        };
        let updated = format!("{prev}{line}");
        Ok(atomic_write(&path, updated.as_bytes())?)
    }

    /// Regenerate `index.md`: a heading per entry linking to the file by its
    /// first H1 title (falling back to the slug). Written atomically so a crash
    /// mid-rebuild cannot corrupt the index.
    pub fn rebuild_index(&self) -> Result<(), WikiError> {
        let listings = self.list()?;
        let mut body = String::from("# Wiki index\n\n");
        if listings.is_empty() {
            body.push_str("_No pages or sources yet._\n");
        } else {
            for Listing { slug, title, kind } in listings {
                let sub = match kind {
                    ListingKind::Page => PAGES_DIR_NAME,
                    ListingKind::Source => SOURCES_DIR_NAME,
                };
                body.push_str(&format!("- [{title}]({sub}/{slug}{MD_EXT})\n"));
            }
        }
        let path = self.root.join(INDEX_FILE_NAME);
        Ok(atomic_write(&path, body.as_bytes())?)
    }
}

fn validate_slug(slug: &str) -> Result<(), WikiError> {
    if slug.is_empty() || slug.contains('\0') {
        return Err(WikiError::InvalidSlug(slug.to_string()));
    }
    if slug.starts_with('/') || slug.starts_with('\\') {
        return Err(WikiError::PathTraversal(slug.to_string()));
    }
    for component in Path::new(slug).components() {
        use std::path::Component;
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(WikiError::PathTraversal(slug.to_string()));
            }
        }
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(WikiError::InvalidSlug(slug.to_string()));
    }
    Ok(())
}

fn collect_listings(
    dir: &Path,
    kind: ListingKind,
    out: &mut Vec<Listing>,
) -> Result<(), WikiError> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        let content = fs::read_to_string(&path).unwrap_or_default();
        let title = first_h1_title(&content).unwrap_or_else(|| stem.clone());
        out.push(Listing {
            slug: stem,
            title,
            kind,
        });
    }
    Ok(())
}

/// Extract the text of the first level-1 heading, skipping `#` lines inside
/// fenced code blocks. Returns `None` when the document has no H1.
pub fn first_h1_title(markdown: &str) -> Option<String> {
    let blocks = craft_markdown::parse(markdown);
    for block in &blocks {
        if let craft_markdown::Block::Lines(lines) = block {
            for line in lines {
                if let craft_markdown::BlockKind::Heading(1) = line.kind {
                    let cleaned: String = craft_markdown::parse_inline(&line.inline)
                        .into_iter()
                        .map(|s| s.text)
                        .collect();
                    let trimmed = cleaned.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
        }
    }
    None
}

/// First non-empty paragraph of prose, capped at [`MAX_EXCERPT_LINES`] lines.
/// A paragraph is a maximal run of non-empty `Paragraph` lines; headings and
/// list items are skipped so the excerpt is body text, not structure.
pub fn extract_excerpt(markdown: &str) -> String {
    let blocks = craft_markdown::parse(markdown);
    for block in &blocks {
        let craft_markdown::Block::Lines(lines) = block else {
            continue;
        };
        let mut iter = lines
            .iter()
            .skip_while(|lb| !is_prose_paragraph(lb))
            .peekable();
        if iter.peek().is_none() {
            continue;
        }
        let paragraph: Vec<&str> = iter
            .take_while(|lb| is_prose_paragraph(lb))
            .map(|lb| lb.inline.as_str())
            .take(MAX_EXCERPT_LINES)
            .collect();
        if !paragraph.is_empty() {
            return paragraph.join("\n");
        }
    }
    String::new()
}

fn is_prose_paragraph(lb: &craft_markdown::LineBlock) -> bool {
    matches!(lb.kind, craft_markdown::BlockKind::Paragraph) && !lb.inline.trim().is_empty()
}

fn render_source_note(note: &SourceNote) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", note.title));
    out.push_str(&format!("> source: {}\n", note.source_path));
    out.push_str(&format!("> ingested: {}\n", note.ingested_at));
    out.push_str(&format!("> summary: {}\n\n", note.summary));
    out.push_str("## Excerpt\n\n");
    out.push_str(&note.excerpt);
    if !note.excerpt.is_empty() {
        out.push('\n');
    }
    out.push_str("\n## Linked pages\n\n");
    if note.linked_pages.is_empty() {
        out.push_str("_(none)_\n");
    } else {
        for p in &note.linked_pages {
            out.push_str(&format!("- [[{p}]]\n"));
        }
    }
    out
}

/// Derive a URL-safe slug from arbitrary text: lowercase, non-alphanumerics to
/// `-`, collapsed and trimmed. Returns `"untitled"` for empty input.
pub fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_dash = true;
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "untitled".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const INVALID_MSG: &str = "invalid slug";
    const TRAVERSAL_MSG: &str = "path traversal";

    fn store(tmp: &Path) -> WikiStore {
        WikiStore::open(tmp).unwrap()
    }

    #[test]
    fn open_creates_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        assert!(s.root.join(PAGES_DIR_NAME).exists());
        assert!(s.root.join(SOURCES_DIR_NAME).exists());
    }

    #[test]
    fn append_then_read_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page("notes", "first draft").unwrap();
        let body = s.read_page("notes").unwrap();
        assert_eq!(body, "first draft");
    }

    #[test]
    fn append_inserts_separator_between_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page("notes", "one").unwrap();
        s.append_page("notes", "two").unwrap();
        let body = s.read_page("notes").unwrap();
        assert_eq!(body, "one\n\ntwo");
    }

    #[test]
    fn append_without_trailing_newline_still_separates() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page("notes", "one\n").unwrap();
        s.append_page("notes", "two").unwrap();
        let body = s.read_page("notes").unwrap();
        assert_eq!(body, "one\n\ntwo");
    }

    #[test]
    fn read_missing_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        match s.read_page("nope") {
            Err(WikiError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn write_source_note_renders_header() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let note = SourceNote {
            slug: "design-doc".into(),
            title: "Design Doc".into(),
            source_path: "/abs/design.md".into(),
            ingested_at: "2026-01-02T03:04:05Z".into(),
            summary: "It does the thing.".into(),
            excerpt: "Body here.".into(),
            linked_pages: vec!["glossary".into()],
        };
        s.write_source_note(&note).unwrap();
        let body = s.read_page("design-doc").unwrap();
        assert!(body.starts_with("# Design Doc\n\n"));
        assert!(body.contains("> source: /abs/design.md"));
        assert!(body.contains("> ingested: 2026-01-02T03:04:05Z"));
        assert!(body.contains("> summary: It does the thing."));
        assert!(body.contains("## Excerpt"));
        assert!(body.contains("Body here."));
        assert!(body.contains("- [[glossary]]"));
    }

    #[test]
    fn list_returns_pages_and_sources_with_titles() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page("alpha", "# Alpha Page\n\nbody").unwrap();
        let note = SourceNote {
            slug: "beta".into(),
            title: "Beta".into(),
            source_path: "/x".into(),
            ingested_at: "t".into(),
            summary: "s".into(),
            excerpt: "e".into(),
            linked_pages: vec![],
        };
        s.write_source_note(&note).unwrap();
        let mut listed = s.list().unwrap();
        listed.sort_by(|a, b| a.slug.cmp(&b.slug));
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].slug, "alpha");
        assert_eq!(listed[0].title, "Alpha Page");
        assert_eq!(listed[0].kind, ListingKind::Page);
        assert_eq!(listed[1].slug, "beta");
        assert_eq!(listed[1].title, "Beta");
        assert_eq!(listed[1].kind, ListingKind::Source);
    }

    #[test]
    fn title_from_filename_when_no_h1() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page("plain", "just prose, no heading").unwrap();
        let listed = s.list().unwrap();
        assert_eq!(listed[0].title, "plain");
    }

    #[test]
    fn first_h1_skips_hash_in_code_fence() {
        let md = "```\n# not a heading\n```\n\n# Real Heading\n";
        assert_eq!(first_h1_title(md).as_deref(), Some("Real Heading"));
    }

    #[test]
    fn rebuild_index_links_all_entries_by_title() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page("alpha", "# Alpha").unwrap();
        let note = SourceNote {
            slug: "beta".into(),
            title: "Beta".into(),
            source_path: "/x".into(),
            ingested_at: "t".into(),
            summary: "s".into(),
            excerpt: "e".into(),
            linked_pages: vec![],
        };
        s.write_source_note(&note).unwrap();
        s.rebuild_index().unwrap();
        let idx = fs::read_to_string(s.root.join(INDEX_FILE_NAME)).unwrap();
        assert!(idx.contains("# Wiki index"));
        assert!(idx.contains("[Alpha](pages/alpha.md)"));
        assert!(idx.contains("[Beta](ingested-sources/beta.md)"));
    }

    #[test]
    fn rebuild_index_empty_wiki_shows_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.rebuild_index().unwrap();
        let idx = fs::read_to_string(s.root.join(INDEX_FILE_NAME)).unwrap();
        assert!(idx.contains("No pages or sources yet"));
    }

    #[test]
    fn append_log_appends_dated_line() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_log("2026-01-01 ingest x").unwrap();
        s.append_log("2026-01-02 edit y").unwrap();
        let log = s.read_log().unwrap();
        assert!(log.contains("2026-01-01 ingest x"));
        assert!(log.contains("2026-01-02 edit y"));
    }

    #[test_case(".." ; "parent_only")]
    #[test_case("../escape" ; "parent_prefix")]
    #[test_case("/abs" ; "absolute")]
    #[test_case("a/../b" ; "nested_parent")]
    fn traversal_slugs_rejected(slug: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let err = s.append_page(slug, "x").unwrap_err().to_string();
        assert!(err.contains(TRAVERSAL_MSG), "got: {err}");
    }

    #[test_case("UPPER" ; "uppercase")]
    #[test_case("with space" ; "space")]
    #[test_case("under_score" ; "underscore")]
    fn invalid_slug_rejected(slug: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let err = s.append_page(slug, "x").unwrap_err().to_string();
        assert!(err.contains(INVALID_MSG), "got: {err}");
    }

    #[test_case("good" ; "plain")]
    #[test_case("multi-word-slug" ; "dashes")]
    #[test_case("123" ; "numeric")]
    fn valid_slugs_accepted(slug: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page(slug, "x").unwrap();
        assert_eq!(s.read_page(slug).unwrap(), "x");
    }

    #[test_case("Hello, World!", "hello-world" ; "basic")]
    #[test_case("  multiple   spaces!! ", "multiple-spaces" ; "collapse")]
    #[test_case("___", "untitled" ; "all_separators")]
    #[test_case("Café", "café" ; "preserve_non_ascii_alphanumeric")]
    fn slugify_cases(input: &str, expected: &str) {
        assert_eq!(slugify(input), expected);
    }

    #[test]
    fn extract_excerpt_returns_first_paragraph() {
        let md = "# Title\n\nFirst paragraph here.\n\nSecond paragraph.";
        let excerpt = extract_excerpt(md);
        assert_eq!(excerpt, "First paragraph here.");
    }

    #[test]
    fn extract_excerpt_caps_at_limit() {
        let lines: Vec<String> = (0..MAX_EXCERPT_LINES + 10)
            .map(|i| format!("line {i}"))
            .collect();
        let md = lines.join("\n");
        let excerpt = extract_excerpt(&md);
        let count = excerpt.lines().count();
        assert_eq!(count, MAX_EXCERPT_LINES);
    }
}
