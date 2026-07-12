//! In-project local wiki knowledge base, conformant to the Open Knowledge
//! Format (OKF) v0.1. Lives under `<project_root>/.wiki/`:
//!
//! ```text
//! .wiki/
//! ├── index.md             # generated: OKF directory listing (bundle root)
//! ├── log.md               # OKF update log, date-grouped, newest first
//! ├── pages/               # hand-authored or tool-appended concept documents
//! │   └── <slug>.md
//! └── ingested-sources/    # concept documents derived from a source file
//!     └── <slug>.md
//! ```
//!
//! Every concept document carries OKF YAML frontmatter with a non-empty
//! `type` field. `index.md` and `log.md` are the only reserved filenames and
//! follow the OKF §6 / §7 structures. See <https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md>.
//!
//! Plain markdown on purpose: diff-friendly, committable, hand-editable, and
//! interoperable with any OKF-compatible consumer. Distinct from `flow`
//! (state-dir, id-addressed, machine-generated) and the Lua `memory` plugin
//! (curated, bulk-loaded): the wiki is in-project, path-addressed, and meant
//! to be read by humans as much as by the agent.

use std::collections::BTreeMap;
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
/// OKF spec version this module targets when it generates reserved files.
const OKF_VERSION: &str = "0.1";
/// OKF `type` values Craft assigns when generating concept documents.
const TYPE_REFERENCE: &str = "Reference";
const TYPE_NOTE: &str = "Note";
const FRONTMATTER_DELIM: &str = "---";
const FM_TYPE: &str = "type";
const FM_TITLE: &str = "title";
const FM_DESCRIPTION: &str = "description";
const FM_RESOURCE: &str = "resource";
const FM_TAGS: &str = "tags";
const FM_TIMESTAMP: &str = "timestamp";
const FM_OKF_VERSION: &str = "okf_version";

#[derive(Debug, thiserror::Error)]
pub enum WikiError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("frontmatter YAML is invalid: {0}")]
    FrontmatterYaml(String),
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

/// One row of `wiki list`: slug, title, kind, and an OKF `description` if the
/// concept document carried one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listing {
    pub slug: String,
    pub title: String,
    pub kind: ListingKind,
    pub description: Option<String>,
}

/// OKF v0.1 concept-document frontmatter. `concept_type` maps to the required
/// YAML `type` field; `extra` preserves any producer-defined keys so round
/// trips stay lossless per spec §4.1 / §9.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    pub concept_type: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub resource: Option<String>,
    pub tags: Vec<String>,
    pub timestamp: Option<String>,
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

impl Frontmatter {
    /// A minimal conformant frontmatter with only the required `type` field.
    pub fn new(concept_type: impl Into<String>) -> Self {
        Self {
            concept_type: concept_type.into(),
            title: None,
            description: None,
            resource: None,
            tags: Vec::new(),
            timestamp: None,
            extra: BTreeMap::new(),
        }
    }
}

/// Caller-supplied metadata for [`WikiStore::append_page_meta`]. Every field is
/// optional so a caller can set just the type, or just a description, without
/// disturbing the others. `kind` maps to the OKF `type` field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageMeta {
    pub kind: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
}

/// Render frontmatter as an OKF YAML block delimited by `---`. Keys are emitted
/// in spec priority order (`type`, `title`, `description`, `resource`, `tags`,
/// `timestamp`) so output is stable and easy to scan.
pub fn render_frontmatter(fm: &Frontmatter) -> Result<String, WikiError> {
    let mut map = serde_yaml::Mapping::new();
    map.insert(into_yaml(FM_TYPE), into_yaml(&fm.concept_type));
    if let Some(v) = &fm.title {
        map.insert(into_yaml(FM_TITLE), into_yaml(v));
    }
    if let Some(v) = &fm.description {
        map.insert(into_yaml(FM_DESCRIPTION), into_yaml(v));
    }
    if let Some(v) = &fm.resource {
        map.insert(into_yaml(FM_RESOURCE), into_yaml(v));
    }
    if !fm.tags.is_empty() {
        map.insert(into_yaml(FM_TAGS), into_yaml(&fm.tags));
    }
    if let Some(v) = &fm.timestamp {
        map.insert(into_yaml(FM_TIMESTAMP), into_yaml(v));
    }
    for (k, v) in &fm.extra {
        map.insert(into_yaml(k), v.clone());
    }
    let yaml =
        serde_yaml::to_string(&map).map_err(|e| WikiError::FrontmatterYaml(e.to_string()))?;
    let body = yaml.trim_end_matches(['\n', '.']);
    Ok(format!(
        "{FRONTMATTER_DELIM}\n{body}\n{FRONTMATTER_DELIM}\n"
    ))
}

fn into_yaml<T: Serialize + ?Sized>(value: &T) -> serde_yaml::Value {
    serde_yaml::to_value(value).unwrap_or(serde_yaml::Value::Null)
}

/// Split a markdown document into `(frontmatter, body)`. Returns `None` for the
/// frontmatter when the file does not begin with a `---`-delimited block. Per
/// OKF §9 consumers tolerate missing frontmatter, so a parse failure becomes
/// `None` rather than an error; callers treat the whole text as the body.
pub fn split_frontmatter(text: &str) -> (Option<Frontmatter>, String) {
    let body_only = || (None, text.to_string());
    let mut lines = text.lines();
    let first = lines.next();
    if first.map(str::trim) != Some(FRONTMATTER_DELIM) {
        return body_only();
    }
    let mut yaml_lines = Vec::new();
    for line in lines {
        if line.trim() == FRONTMATTER_DELIM {
            let yaml = yaml_lines.join("\n");
            return match parse_frontmatter_block(&yaml) {
                Ok(fm) => (Some(fm), body_after_delim(text)),
                Err(_) => body_only(),
            };
        }
        yaml_lines.push(line);
    }
    body_only()
}

/// Slice the text starting just after the closing frontmatter delimiter.
fn body_after_delim(text: &str) -> String {
    let mut after_open = false;
    let mut byte_pos = 0;
    for (i, line) in text.lines().enumerate() {
        if !after_open {
            if line.trim() == FRONTMATTER_DELIM {
                after_open = true;
            }
            continue;
        }
        if line.trim() == FRONTMATTER_DELIM {
            let mut consumed = 0;
            for (n, l) in text.lines().enumerate() {
                consumed += l.len();
                if n == i {
                    break;
                }
                consumed += 1;
            }
            byte_pos = consumed;
            break;
        }
    }
    if byte_pos == 0 {
        return text.to_string();
    }
    text[byte_pos..]
        .trim_start_matches(['\r', '\n'])
        .to_string()
}

fn parse_frontmatter_block(yaml: &str) -> Result<Frontmatter, WikiError> {
    let map: serde_yaml::Mapping =
        serde_yaml::from_str(yaml).map_err(|e| WikiError::FrontmatterYaml(e.to_string()))?;
    let get_string = |key: &str| -> Option<String> {
        map.get(into_yaml(key))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    let Some(concept_type) = get_string(FM_TYPE) else {
        return Err(WikiError::FrontmatterYaml(format!(
            "missing required `{FM_TYPE}` field"
        )));
    };
    if concept_type.trim().is_empty() {
        return Err(WikiError::FrontmatterYaml(format!(
            "`{FM_TYPE}` field is empty"
        )));
    }
    let tags = map
        .get(into_yaml(FM_TAGS))
        .and_then(|v| {
            v.as_sequence().map(|s| {
                s.iter()
                    .filter_map(|i| i.as_str().map(str::to_owned))
                    .collect()
            })
        })
        .unwrap_or_default();
    let reserved = [
        FM_TYPE,
        FM_TITLE,
        FM_DESCRIPTION,
        FM_RESOURCE,
        FM_TAGS,
        FM_TIMESTAMP,
    ];
    let mut extra = BTreeMap::new();
    for (k, v) in &map {
        if let Some(ks) = k.as_str()
            && !reserved.contains(&ks)
        {
            extra.insert(ks.to_owned(), v.clone());
        }
    }
    Ok(Frontmatter {
        concept_type,
        title: get_string(FM_TITLE),
        description: get_string(FM_DESCRIPTION),
        resource: get_string(FM_RESOURCE),
        tags,
        timestamp: get_string(FM_TIMESTAMP),
        extra,
    })
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
    /// Full verbatim body of the ingested source document.
    pub body: String,
    pub linked_pages: Vec<String>,
}

impl SourceNote {
    fn frontmatter(&self) -> Frontmatter {
        Frontmatter {
            concept_type: TYPE_REFERENCE.to_owned(),
            title: Some(self.title.clone()),
            description: Some(self.summary.clone()),
            resource: Some(self.source_path.clone()),
            tags: Vec::new(),
            timestamp: Some(self.ingested_at.clone()),
            extra: BTreeMap::new(),
        }
    }
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

    /// Read a concept document by slug, returning the markdown body with
    /// frontmatter stripped. Falls back to the matching ingested-source note
    /// if no page exists, so `wiki show <id>` resolves either kind.
    pub fn read_page(&self, slug: &str) -> Result<String, WikiError> {
        let page = self.page_path(slug)?;
        match fs::read_to_string(&page) {
            Ok(s) => Ok(strip_frontmatter_body(&s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let src = self.source_path(slug)?;
                fs::read_to_string(&src)
                    .map(|s| strip_frontmatter_body(&s))
                    .map_err(|_| WikiError::NotFound(slug.to_string()))
            }
            Err(e) => Err(WikiError::Io(e)),
        }
    }

    /// Append `body` to a page, creating it if missing. Ensures the page
    /// carries OKF frontmatter with a non-empty `type`, injecting a default
    /// `type: Note` block when the page is new or missing frontmatter. Existing
    /// frontmatter is preserved and its `timestamp` bumped to `timestamp` when
    /// supplied. Inserts a blank-line separator between the existing tail and
    /// the new body so successive appends stay readable.
    pub fn append_page(
        &self,
        slug: &str,
        body: &str,
        timestamp: Option<&str>,
    ) -> Result<(), WikiError> {
        self.append_page_meta(slug, body, timestamp, PageMeta::default())
    }

    /// Like [`append_page`], but lets the caller supply OKF metadata. For a new
    /// page the `kind` overrides the default `type: Note`. For an existing page
    /// the type is never changed (OKF §4.1: producers MUST preserve existing
    /// frontmatter); a supplied `description` or `tags` fills in a missing
    /// field but never overwrites a value the page already carries. `timestamp`
    /// is always bumped when supplied.
    pub fn append_page_meta(
        &self,
        slug: &str,
        body: &str,
        timestamp: Option<&str>,
        meta: PageMeta,
    ) -> Result<(), WikiError> {
        let path = self.page_path(slug)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let prev = fs::read_to_string(&path).unwrap_or_default();
        let updated = append_page_body(&prev, body, timestamp, &meta);
        Ok(atomic_write(&path, updated.as_bytes())?)
    }

    /// Write a source note to `ingested-sources/<slug>.md` atomically as an OKF
    /// concept document (`type: Reference`).
    pub fn write_source_note(&self, note: &SourceNote) -> Result<(), WikiError> {
        let path = self.source_path(&note.slug)?;
        let rendered = render_source_note(note)?;
        Ok(atomic_write(&path, rendered.as_bytes())?)
    }

    /// List all pages and ingested sources, each with its title (frontmatter
    /// title preferred, then first H1, then stem) and OKF `description`.
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

    /// Read the OKF update log. Returns an empty string when absent.
    pub fn read_log(&self) -> Result<String, WikiError> {
        let path = self.root.join(LOG_FILE_NAME);
        match fs::read_to_string(&path) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(WikiError::Io(e)),
        }
    }

    /// Append an entry to the OKF `log.md`. The entry is re-grouped under its
    /// ISO 8601 date (`YYYY-MM-DD`, per spec §7) with newest dates first.
    /// `verb` is the bolded convention word (e.g. `Update`, `Creation`).
    pub fn append_log(
        &self,
        timestamp_iso: &str,
        verb: &str,
        message: &str,
    ) -> Result<(), WikiError> {
        let path = self.root.join(LOG_FILE_NAME);
        let prev = fs::read_to_string(&path).unwrap_or_default();
        let date = log_date(timestamp_iso);
        let line = format!("* **{verb}**: {message}");
        let rebuilt = regroup_log(&prev, &date, &line);
        Ok(atomic_write(&path, rebuilt.as_bytes())?)
    }

    /// Regenerate `index.md` in OKF §6 form: a bundle-root index with an
    /// `okf_version` frontmatter block (the only frontmatter permitted in an
    /// index), then section-grouped listings that include each concept's
    /// `description`. Written atomically so a crash mid-rebuild cannot corrupt
    /// the index.
    pub fn rebuild_index(&self) -> Result<(), WikiError> {
        let listings = self.list()?;
        let mut body = String::new();
        body.push_str(&format!(
            "{FRONTMATTER_DELIM}\n{FM_OKF_VERSION}: \"{OKF_VERSION}\"\n{FRONTMATTER_DELIM}\n\n"
        ));
        body.push_str("# Wiki index\n\n");
        if listings.is_empty() {
            body.push_str("_No pages or sources yet._\n");
        } else {
            let pages: Vec<&Listing> = listings
                .iter()
                .filter(|l| l.kind == ListingKind::Page)
                .collect();
            let sources: Vec<&Listing> = listings
                .iter()
                .filter(|l| l.kind == ListingKind::Source)
                .collect();
            if !pages.is_empty() {
                body.push_str("# Pages\n\n");
                for l in pages {
                    body.push_str(&format_index_entry(PAGES_DIR_NAME, l));
                }
                body.push('\n');
            }
            if !sources.is_empty() {
                body.push_str("# Ingested sources\n\n");
                for l in sources {
                    body.push_str(&format_index_entry(SOURCES_DIR_NAME, l));
                }
            }
        }
        let path = self.root.join(INDEX_FILE_NAME);
        Ok(atomic_write(&path, body.as_bytes())?)
    }
}

fn format_index_entry(dir: &str, l: &Listing) -> String {
    match &l.description {
        Some(d) if !d.trim().is_empty() => {
            format!("- [{}]({dir}/{}{MD_EXT}) - {d}\n", l.title, l.slug)
        }
        _ => format!("- [{}]({dir}/{}{MD_EXT})\n", l.title, l.slug),
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
        let (fm, body) = split_frontmatter(&content);
        let title = fm
            .as_ref()
            .and_then(|f| f.title.clone())
            .or_else(|| first_h1_title(&body))
            .unwrap_or_else(|| stem.clone());
        let description = fm.and_then(|f| f.description);
        out.push(Listing {
            slug: stem,
            title,
            kind,
            description,
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

/// Strip frontmatter from a concept document, returning only the body. When no
/// frontmatter is present the whole text is returned unchanged.
fn strip_frontmatter_body(text: &str) -> String {
    split_frontmatter(text).1
}

/// Append `body` to an existing page that already (or newly) carries OKF
/// frontmatter. Bumps `timestamp` when supplied. For a new page, `meta.kind`
/// overrides the default `type: Note`. For an existing page the type is
/// preserved; `meta.description` and `meta.tags` fill in a missing field only.
fn append_page_body(prev: &str, body: &str, timestamp: Option<&str>, meta: &PageMeta) -> String {
    let (fm_opt, mut prev_body) = split_frontmatter(prev);
    let mut fm = match fm_opt {
        Some(existing) => {
            let mut fm = existing;
            if fm.description.is_none()
                && let Some(d) = &meta.description
            {
                fm.description = Some(d.clone());
            }
            if fm.tags.is_empty() && !meta.tags.is_empty() {
                fm.tags = meta.tags.clone();
            }
            fm
        }
        None => {
            let kind = meta.kind.as_deref().unwrap_or(TYPE_NOTE);
            let mut fm = Frontmatter::new(kind);
            fm.description = meta.description.clone();
            if !meta.tags.is_empty() {
                fm.tags = meta.tags.clone();
            }
            fm
        }
    };
    if let Some(ts) = timestamp {
        fm.timestamp = Some(ts.to_owned());
    }
    prev_body = prev_body.trim_end_matches('\n').to_string();
    let joined = if prev_body.is_empty() {
        body.to_string()
    } else {
        format!("{prev_body}\n\n{body}")
    };
    let front = render_frontmatter(&fm).expect("frontmatter renders from an in-memory struct");
    format!("{front}{joined}")
}

fn render_source_note(note: &SourceNote) -> Result<String, WikiError> {
    let fm = note.frontmatter();
    let front = render_frontmatter(&fm)?;
    let mut out = String::new();
    out.push_str(&front);
    out.push_str(&format!("# {}\n\n", note.title));
    out.push_str("## Excerpt\n\n");
    out.push_str(&note.excerpt);
    if !note.excerpt.is_empty() {
        out.push('\n');
    }
    out.push_str("\n## Source\n\n");
    out.push_str(&note.body);
    if !note.body.is_empty() && !note.body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n## Linked pages\n\n");
    if note.linked_pages.is_empty() {
        out.push_str("_(none)_\n");
    } else {
        for p in &note.linked_pages {
            out.push_str(&format!("- [{}]({})\n", p, link_to_page(p)));
        }
    }
    Ok(out)
}

/// OKF §5 recommends absolute bundle-relative links: `/pages/<slug>.md`.
fn link_to_page(slug: &str) -> String {
    format!("/{PAGES_DIR_NAME}/{slug}{MD_EXT}")
}

/// Extract the `YYYY-MM-DD` portion of an ISO 8601 timestamp for log grouping.
fn log_date(iso: &str) -> String {
    iso.get(..10).unwrap_or(iso).to_owned()
}

/// Rebuild an OKF §7 log with `date` re-grouped at the correct position
/// (newest first) and `new_line` appended to its bullet list. The log is parsed
/// line-by-line: `## YYYY-MM-DD` headings start a group, `- ...` bullets fill it.
fn regroup_log(prev: &str, date: &str, new_line: &str) -> String {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;
    for raw in prev.lines() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("## ") {
            if let Some(g) = current.take() {
                groups.push(g);
            }
            let heading = line.trim_start()[3..].trim().to_owned();
            current = Some((heading, Vec::new()));
        } else if let Some((_, lines)) = current.as_mut()
            && (line.trim_start().starts_with("- ") || line.trim_start().starts_with("* "))
        {
            lines.push(line.trim_start()[2..].trim().to_owned());
        }
    }
    if let Some(g) = current.take() {
        groups.push(g);
    }

    let mut target: Option<Vec<String>> = None;
    let mut rest: Vec<(String, Vec<String>)> = Vec::new();
    for (d, ls) in groups {
        if d == date {
            target = Some(ls);
        } else {
            rest.push((d, ls));
        }
    }
    let mut target = target.unwrap_or_default();
    target.push(new_line.to_owned());
    rest.sort_by(|a, b| b.0.cmp(&a.0));

    let mut all: Vec<(String, Vec<String>)> = Vec::new();
    let mut inserted = false;
    for (d, ls) in rest {
        if !inserted && d.as_str() < date {
            all.push((date.to_owned(), target.clone()));
            inserted = true;
        }
        all.push((d, ls));
    }
    if !inserted {
        all.push((date.to_owned(), target));
    }

    let mut out = String::from("# Directory Update Log\n\n");
    for (d, ls) in &all {
        out.push_str(&format!("## {d}\n"));
        for line in ls {
            out.push_str(&format!("* {line}\n"));
        }
        out.push('\n');
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
    const TIMESTAMP: &str = "2026-01-02T03:04:05Z";

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
    fn append_then_read_roundtrips_body_without_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page("notes", "first draft", None).unwrap();
        let body = s.read_page("notes").unwrap();
        assert_eq!(body, "first draft");
    }

    #[test]
    fn append_inserts_separator_between_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page("notes", "one", None).unwrap();
        s.append_page("notes", "two", None).unwrap();
        let body = s.read_page("notes").unwrap();
        assert_eq!(body, "one\n\ntwo");
    }

    #[test]
    fn append_without_trailing_newline_still_separates() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page("notes", "one\n", None).unwrap();
        s.append_page("notes", "two", None).unwrap();
        let body = s.read_page("notes").unwrap();
        assert_eq!(body, "one\n\ntwo");
    }

    #[test]
    fn append_injects_okf_frontmatter_with_required_type() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page("notes", "body", Some(TIMESTAMP)).unwrap();
        let raw = std::fs::read_to_string(s.root.join("pages").join("notes.md")).unwrap();
        assert!(raw.starts_with("---\n"));
        assert!(raw.contains("type: Note"));
        assert!(raw.contains("timestamp:"));
        let (fm, _) = split_frontmatter(&raw);
        let fm = fm.expect("frontmatter parses");
        assert_eq!(fm.concept_type, "Note");
        assert_eq!(fm.timestamp.as_deref(), Some(TIMESTAMP));
    }

    #[test]
    fn append_preserves_existing_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let path = s.root.join("pages").join("notes.md");
        std::fs::write(
            &path,
            "---\ntype: Playbook\ntitle: My Playbook\ntags: [oncall]\n---\n\nold body\n",
        )
        .unwrap();
        s.append_page("notes", "new body", Some(TIMESTAMP)).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let (fm, body) = split_frontmatter(&raw);
        let fm = fm.expect("frontmatter parses");
        assert_eq!(fm.concept_type, "Playbook");
        assert_eq!(fm.title.as_deref(), Some("My Playbook"));
        assert_eq!(fm.tags, vec!["oncall".to_owned()]);
        assert_eq!(fm.timestamp.as_deref(), Some(TIMESTAMP));
        assert!(body.contains("old body"));
        assert!(body.contains("new body"));
    }

    #[test]
    fn append_with_meta_sets_type_on_new_page() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let meta = PageMeta {
            kind: Some("Reference".into()),
            description: Some("stack overview".into()),
            tags: vec!["rust".into()],
        };
        s.append_page_meta("tech-stack", "# Tech Stack", Some(TIMESTAMP), meta)
            .unwrap();
        let raw = std::fs::read_to_string(s.root.join("pages").join("tech-stack.md")).unwrap();
        let (fm, _) = split_frontmatter(&raw);
        let fm = fm.expect("frontmatter parses");
        assert_eq!(fm.concept_type, "Reference");
        assert_eq!(fm.description.as_deref(), Some("stack overview"));
        assert_eq!(fm.tags, vec!["rust".to_owned()]);
    }

    #[test]
    fn append_with_meta_default_still_uses_note() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page_meta("notes", "body", None, PageMeta::default())
            .unwrap();
        let raw = std::fs::read_to_string(s.root.join("pages").join("notes.md")).unwrap();
        let (fm, _) = split_frontmatter(&raw);
        let fm = fm.expect("frontmatter parses");
        assert_eq!(fm.concept_type, "Note");
    }

    #[test]
    fn append_with_meta_never_overwrites_existing_type_or_description() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let path = s.root.join("pages").join("notes.md");
        std::fs::write(
            &path,
            "---\ntype: Decision\ndescription: original\n---\n\nold body\n",
        )
        .unwrap();
        let meta = PageMeta {
            kind: Some("Reference".into()),
            description: Some("should not override".into()),
            tags: vec!["new".into()],
        };
        s.append_page_meta("notes", "new body", None, meta).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let (fm, body) = split_frontmatter(&raw);
        let fm = fm.expect("frontmatter parses");
        assert_eq!(fm.concept_type, "Decision");
        assert_eq!(fm.description.as_deref(), Some("original"));
        assert_eq!(fm.tags, vec!["new".to_owned()]);
        assert!(body.contains("old body"));
        assert!(body.contains("new body"));
    }

    #[test]
    fn append_with_meta_fills_missing_description_and_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let path = s.root.join("pages").join("notes.md");
        std::fs::write(&path, "---\ntype: Decision\n---\n\nbody\n").unwrap();
        let meta = PageMeta {
            kind: None,
            description: Some("filled in".into()),
            tags: vec!["adr".into()],
        };
        s.append_page_meta("notes", "more", None, meta).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let (fm, _) = split_frontmatter(&raw);
        let fm = fm.expect("frontmatter parses");
        assert_eq!(fm.concept_type, "Decision");
        assert_eq!(fm.description.as_deref(), Some("filled in"));
        assert_eq!(fm.tags, vec!["adr".to_owned()]);
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
    fn write_source_note_emits_okf_frontmatter_and_body() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let note = SourceNote {
            slug: "design-doc".into(),
            title: "Design Doc".into(),
            source_path: "/abs/design.md".into(),
            ingested_at: TIMESTAMP.into(),
            summary: "It does the thing.".into(),
            excerpt: "Body here.".into(),
            body: "# Design Doc\n\nFull body text.".into(),
            linked_pages: vec!["glossary".into()],
        };
        s.write_source_note(&note).unwrap();
        let raw =
            std::fs::read_to_string(s.root.join(SOURCES_DIR_NAME).join("design-doc.md")).unwrap();
        let (fm, body) = split_frontmatter(&raw);
        let fm = fm.expect("source note has frontmatter");
        assert_eq!(fm.concept_type, "Reference");
        assert_eq!(fm.title.as_deref(), Some("Design Doc"));
        assert_eq!(fm.description.as_deref(), Some("It does the thing."));
        assert_eq!(fm.resource.as_deref(), Some("/abs/design.md"));
        assert_eq!(fm.timestamp.as_deref(), Some(TIMESTAMP));
        assert!(body.starts_with("# Design Doc\n\n"));
        assert!(body.contains("## Excerpt"));
        assert!(body.contains("Body here."));
        assert!(body.contains("## Source"));
        assert!(body.contains("Full body text."));
        assert!(body.contains("- [glossary](/pages/glossary.md)"));
    }

    #[test]
    fn read_page_strips_frontmatter_from_source_note() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let note = SourceNote {
            slug: "design-doc".into(),
            title: "Design Doc".into(),
            source_path: "/abs/design.md".into(),
            ingested_at: TIMESTAMP.into(),
            summary: "It does the thing.".into(),
            excerpt: "Excerpt body.".into(),
            body: "Full body.".into(),
            linked_pages: vec![],
        };
        s.write_source_note(&note).unwrap();
        let body = s.read_page("design-doc").unwrap();
        assert!(!body.starts_with("---"));
        assert!(body.contains("# Design Doc"));
    }

    #[test]
    fn list_returns_pages_and_sources_with_titles_and_descriptions() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page("alpha", "# Alpha Page\n\nbody", None)
            .unwrap();
        let note = SourceNote {
            slug: "beta".into(),
            title: "Beta".into(),
            source_path: "/x".into(),
            ingested_at: TIMESTAMP.into(),
            summary: "beta summary".into(),
            excerpt: "e".into(),
            body: "full body".into(),
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
        assert_eq!(listed[1].description.as_deref(), Some("beta summary"));
    }

    #[test]
    fn list_prefers_frontmatter_title_over_h1() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let path = s.root.join("pages").join("plain.md");
        std::fs::write(
            &path,
            "---\ntype: Note\ntitle: From Frontmatter\n---\n\n# H1 Title\n\nbody\n",
        )
        .unwrap();
        let listed = s.list().unwrap();
        assert_eq!(listed[0].title, "From Frontmatter");
    }

    #[test]
    fn title_from_filename_when_no_h1_or_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page("plain", "just prose, no heading", None)
            .unwrap();
        let listed = s.list().unwrap();
        assert_eq!(listed[0].title, "plain");
    }

    #[test]
    fn first_h1_skips_hash_in_code_fence() {
        let md = "```\n# not a heading\n```\n\n# Real Heading\n";
        assert_eq!(first_h1_title(md).as_deref(), Some("Real Heading"));
    }

    #[test]
    fn rebuild_index_emits_okf_sections_with_descriptions() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page("alpha", "# Alpha", None).unwrap();
        let note = SourceNote {
            slug: "beta".into(),
            title: "Beta".into(),
            source_path: "/x".into(),
            ingested_at: TIMESTAMP.into(),
            summary: "beta desc".into(),
            excerpt: "e".into(),
            body: "full body".into(),
            linked_pages: vec![],
        };
        s.write_source_note(&note).unwrap();
        s.rebuild_index().unwrap();
        let idx = fs::read_to_string(s.root.join(INDEX_FILE_NAME)).unwrap();
        assert!(idx.starts_with("---\nokf_version: \"0.1\"\n---\n"));
        assert!(idx.contains("# Wiki index"));
        assert!(idx.contains("# Pages"));
        assert!(idx.contains("[Alpha](pages/alpha.md)"));
        assert!(idx.contains("# Ingested sources"));
        assert!(idx.contains("[Beta](ingested-sources/beta.md) - beta desc"));
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
    fn append_log_groups_by_date_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_log("2026-01-01T00:00:00Z", "Creation", "made alpha")
            .unwrap();
        s.append_log("2026-01-03T00:00:00Z", "Update", "touched beta")
            .unwrap();
        s.append_log("2026-01-02T00:00:00Z", "Update", "touched gamma")
            .unwrap();
        let log = s.read_log().unwrap();
        let first_date_pos = log.find("## 2026-01-03").unwrap();
        let second_date_pos = log.find("## 2026-01-02").unwrap();
        let third_date_pos = log.find("## 2026-01-01").unwrap();
        assert!(first_date_pos < second_date_pos);
        assert!(second_date_pos < third_date_pos);
        assert!(log.contains("**Update**: touched gamma"));
    }

    #[test]
    fn append_log_same_date_accumulates_under_one_heading() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_log("2026-01-01T01:00:00Z", "Creation", "first")
            .unwrap();
        s.append_log("2026-01-01T02:00:00Z", "Update", "second")
            .unwrap();
        let log = s.read_log().unwrap();
        assert_eq!(log.matches("## 2026-01-01").count(), 1);
        assert!(log.contains("**Creation**: first"));
        assert!(log.contains("**Update**: second"));
    }

    #[test_case(".." ; "parent_only")]
    #[test_case("../escape" ; "parent_prefix")]
    #[test_case("/abs" ; "absolute")]
    #[test_case("a/../b" ; "nested_parent")]
    fn traversal_slugs_rejected(slug: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let err = s.append_page(slug, "x", None).unwrap_err().to_string();
        assert!(err.contains(TRAVERSAL_MSG), "got: {err}");
    }

    #[test_case("UPPER" ; "uppercase")]
    #[test_case("with space" ; "space")]
    #[test_case("under_score" ; "underscore")]
    fn invalid_slug_rejected(slug: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let err = s.append_page(slug, "x", None).unwrap_err().to_string();
        assert!(err.contains(INVALID_MSG), "got: {err}");
    }

    #[test_case("good" ; "plain")]
    #[test_case("multi-word-slug" ; "dashes")]
    #[test_case("123" ; "numeric")]
    fn valid_slugs_accepted(slug: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.append_page(slug, "x", None).unwrap();
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

    #[test]
    fn split_frontmatter_roundtrips_extra_keys() {
        let fm = Frontmatter {
            concept_type: "Playbook".into(),
            title: Some("T".into()),
            description: None,
            resource: None,
            tags: vec!["a".into()],
            timestamp: Some("2026-01-01T00:00:00Z".into()),
            extra: {
                let mut m = BTreeMap::new();
                m.insert("owner".into(), serde_yaml::Value::String("team".into()));
                m
            },
        };
        let rendered = render_frontmatter(&fm).unwrap();
        let (parsed, _) = split_frontmatter(&rendered);
        let parsed = parsed.expect("parses back");
        assert_eq!(parsed, fm);
    }

    #[test]
    fn split_frontmatter_missing_type_returns_none() {
        let text = "---\ntitle: No Type\n---\n\nbody\n";
        let (fm, body) = split_frontmatter(text);
        assert!(fm.is_none(), "non-conformant frontmatter must be ignored");
        assert_eq!(body, text);
    }
}
