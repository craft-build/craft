//! Tool dedup cache: a bounded FIFO cache keyed by (tool, arguments) that
//! lets read-only tools answer identical repeat calls from cache instead of
//! re-executing. Entries are invalidated per-path when a write tool touches
//! the file, and the whole cache is cleared before compaction (compacted
//! history may describe a different world than the one the cache sampled).
//!
//! Paths are normalized structurally (root-joined, `.`/`..` collapsed,
//! case-folded on Windows) rather than lexically, so a relative call and an
//! absolute call naming the same file share one identity. This is done in
//! pure string space — no `fs::canonicalize` — because these operations sit
//! on the hot tool path and `invalidate_path` may receive a path the write
//! just deleted. Residual gap: symlink aliases are NOT resolved; a write
//! through one alias will not drop entries cached under another name. As a
//! mitigation every `invalidate_path` also drops pathless entries (grep/glob
//! resample the tree on their next call).

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::history::{ToolResult, ToolResultContent};

const READ_ONLY_TOOLS: &[&str] = &["read", "grep", "glob", "view_image"];
const WRITE_TOOLS: &[&str] = &[
    "write",
    "edit",
    "edit_lines",
    "insert_lines",
    "multiedit",
    "delete",
    "apply_patch",
    "move",
];
const CACHED_PREFIX: &str = "[cached] ";
const MAX_CACHE_ENTRIES: usize = 64;

/// Structural path normalizer: joins relative inputs onto a workspace root,
/// collapses `.`/`..`/duplicate separators, and case-folds on Windows — all
/// in string space (no filesystem access; see the module doc for why).
#[derive(Debug, Clone, Default)]
pub struct PathNormalizer {
    root: Option<String>,
}

impl PathNormalizer {
    /// `root` (an absolute workspace root, if known) anchors relative paths.
    pub fn new(root: Option<&str>) -> Self {
        Self {
            root: root.map(|r| Self::default().normalize(r)),
        }
    }

    /// Normalize to a comparable form. Both sides of a comparison must come
    /// from a normalizer with the same root.
    pub fn normalize(&self, path: &str) -> String {
        let joined = match self.root.as_deref() {
            Some(root) if !is_absolute(path) => format!("{root}/{path}"),
            _ => path.to_string(),
        };
        let absolute = is_absolute(&joined);
        let mut parts: Vec<&str> = Vec::new();
        for seg in joined.split(['/', '\\']) {
            match seg {
                "" | "." => {}
                ".." => {
                    if parts.last().is_some_and(|p| *p != "..") {
                        parts.pop();
                    } else if !absolute {
                        parts.push("..");
                    }
                    // `..` above an anchored root clamps: stays inside it.
                }
                s => parts.push(s),
            }
        }
        let rendered = if absolute {
            format!("/{}", parts.join("/"))
        } else {
            parts.join("/")
        };
        if cfg!(windows) {
            rendered.to_lowercase()
        } else {
            rendered
        }
    }
}

fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
        || path.starts_with('\\')
        || path.get(..2).is_some_and(|p| p.as_bytes()[1] == b':')
}

#[derive(Debug, Clone)]
struct Entry {
    result: ToolResult,
    /// The argument path, if any, scoping per-path invalidation.
    path: Option<String>,
    /// Canonical `(name, args)` serialization used to verify a hit: a
    /// 64-bit digest alone cannot prove the entry belongs to this call.
    check: String,
}

#[derive(Debug, Default)]
pub struct ToolDedupCache {
    entries: HashMap<u64, Entry>,
    order: VecDeque<u64>,
    normalizer: PathNormalizer,
}

impl ToolDedupCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// A cache anchored at `root` so relative and absolute spellings of the
    /// same file invalidate identically.
    pub fn with_root(root: Option<&str>) -> Self {
        Self {
            normalizer: PathNormalizer::new(root),
            ..Self::default()
        }
    }

    pub fn key(name: &str, input: &Value) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut hasher);
        hash_value(input, &mut hasher);
        hasher.finish()
    }

    pub fn is_read_only(name: &str) -> bool {
        READ_ONLY_TOOLS.contains(&name)
    }

    pub fn is_write(name: &str) -> bool {
        WRITE_TOOLS.contains(&name)
    }

    pub fn get(&self, key: u64, name: &str, input: &Value) -> Option<&ToolResult> {
        self.entries
            .get(&key)
            .filter(|e| e.check == canonical_args(name, input))
            .map(|e| &e.result)
    }

    /// Cache one successful read-only result, evicting the oldest entry at
    /// capacity — but only when the key is new; overwriting an existing key
    /// must never evict an innocent FIFO neighbor. `path` (the argument
    /// path, if any) is normalized and scopes invalidation.
    pub fn insert(
        &mut self,
        key: u64,
        result: &ToolResult,
        path: Option<&str>,
        name: &str,
        input: &Value,
    ) {
        if result.is_error {
            return;
        }
        let entry = Entry {
            result: result.clone(),
            path: path.map(|p| self.normalizer.normalize(p)),
            check: canonical_args(name, input),
        };
        if self.entries.len() >= MAX_CACHE_ENTRIES
            && !self.entries.contains_key(&key)
            && let Some(evict) = self.order.front().copied()
        {
            self.entries.remove(&evict);
            self.order.pop_front();
        }
        if self.entries.insert(key, entry).is_none() {
            self.order.push_back(key);
        }
    }

    /// Drop the entry for `path`, plus every pathless entry (grep/glob
    /// sample the whole tree, so any write can change their answer — this
    /// also serves as the symlink-alias mitigation; see the module doc).
    pub fn invalidate_path(&mut self, path: &str) {
        let normalized = self.normalizer.normalize(path);
        self.entries
            .retain(|_, e| e.path.is_some() && e.path.as_deref() != Some(normalized.as_str()));
        self.order.retain(|key| self.entries.contains_key(key));
    }

    /// Drop every pathless entry; used when a write touches no known path.
    pub fn invalidate_pathless(&mut self) {
        self.entries.retain(|_, e| e.path.is_some());
        self.order.retain(|key| self.entries.contains_key(key));
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Adapt a cached result for replay: the same content with the `[cached] `
/// marker on the leading text block, answering this call's id.
pub fn cached_result(cached: &ToolResult, call_id: &str) -> ToolResult {
    let mut content = cached.content.clone();
    match content.first_mut() {
        Some(ToolResultContent::Text(text)) => {
            text.text = format!("{CACHED_PREFIX}{}", text.text);
        }
        Some(ToolResultContent::Json { value }) => {
            let rendered = ToolResultContent::Json {
                value: value.clone(),
            }
            .to_text();
            content[0] = ToolResultContent::text(format!("{CACHED_PREFIX}{rendered}"));
        }
        Some(ToolResultContent::Image(image)) => {
            image.caption = format!("{CACHED_PREFIX}{}", image.caption);
        }
        None => content.push(ToolResultContent::text(CACHED_PREFIX.trim_end())),
    }
    ToolResult {
        call: call_id.to_owned(),
        name: cached.name.clone(),
        content,
        is_error: false,
    }
}

/// The path whose contents a call reads or writes, if its arguments name one.
pub fn extract_file_path(input: &Value) -> Option<String> {
    input.get("path").and_then(Value::as_str).map(String::from)
}

/// Every path a write call touches (multiedit edits carry several).
pub fn extract_write_paths(name: &str, input: &Value) -> Vec<String> {
    if !ToolDedupCache::is_write(name) {
        return Vec::new();
    }
    if name == "apply_patch"
        && let Some(patch) = input.get("patch_text").and_then(Value::as_str)
    {
        return crate::tools::apply_patch::patch_paths(patch);
    }
    if name == "multiedit"
        && let Some(edits) = input.get("edits").and_then(Value::as_array)
    {
        return edits
            .iter()
            .filter_map(|edit| edit.get("path").and_then(Value::as_str))
            .map(String::from)
            .collect();
    }
    if name == "delete"
        && let Some(files) = input.get("files").and_then(Value::as_array)
    {
        return files
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
    }
    extract_file_path(input).into_iter().collect()
}

/// Deterministic canonical form of `(name, input)` for hit verification:
/// object keys sorted at every level so argument order cannot fork the check.
fn canonical_args(name: &str, input: &Value) -> String {
    format!(
        "{}:{}",
        name,
        serde_json::to_string(&sorted(input)).expect("JSON is serializable")
    )
}

fn sorted(val: &Value) -> Value {
    match val {
        Value::Object(obj) => {
            let map: std::collections::BTreeMap<String, Value> =
                obj.iter().map(|(k, v)| (k.clone(), sorted(v))).collect();
            serde_json::to_value(map).expect("JSON is serializable")
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// Session-wide handle: the dispatcher (which clones per turn) and the
/// compaction engine share one cache through this.
pub type SharedDedupCache = Arc<Mutex<ToolDedupCache>>;

pub fn shared_cache() -> SharedDedupCache {
    // Anchor at the process working directory, resolved once at construction
    // (never on the hot path): relative and absolute argument spellings then
    // invalidate the same entries in production callers.
    let root = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    shared_cache_with_root(root.as_deref())
}

/// Like [`shared_cache`], but anchored at a workspace root so relative and
/// absolute argument paths invalidate the same entries.
pub fn shared_cache_with_root(root: Option<&str>) -> SharedDedupCache {
    Arc::new(Mutex::new(ToolDedupCache::with_root(root)))
}

/// Conflict-grade normalization for write-path comparison (wave scheduling,
/// guardrail decisions): resolves the deepest existing ancestor through the
/// filesystem so symlink aliases collapse, then case-folds on Windows.
/// Falls back to structural normalization when nothing exists on disk yet.
/// Not for the hot read path — callers invoke this once per write call.
pub fn normalize_write_path(path: &str) -> String {
    static ROOT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let root = ROOT.get_or_init(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    });
    normalize_write_path_with_root(root.as_deref(), path)
}

/// Like [`normalize_write_path`], but relative paths are anchored at the
/// given workspace root (where the tools themselves resolve them) instead of
/// the process working directory.
pub fn normalize_write_path_with_root(root: Option<&str>, path: &str) -> String {
    let structural = PathNormalizer::new(root).normalize(path);
    let mut cur = std::path::PathBuf::from(&structural);
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    // Resolving dangling/missing tails: walk up, following symlinks (even
    // dangling ones) via `read_link`, until a real existing ancestor is
    // found; the missing tail is then appended to its canonical form.
    for _ in 0..64 {
        let is_symlink = std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink());
        if !is_symlink && cur.exists() {
            break;
        }
        if is_symlink {
            let Ok(target) = cur.read_link() else {
                return structural;
            };
            if let Some(parent) = cur.parent().filter(|p| !p.as_os_str().is_empty()) {
                cur = parent.join(target);
            } else {
                cur = target;
            }
            continue;
        }
        let Some(parent) = cur.parent().filter(|p| !p.as_os_str().is_empty()) else {
            return structural;
        };
        tail.push(cur.file_name().unwrap().to_os_string());
        cur = parent.to_path_buf();
    }
    let base = match std::fs::canonicalize(&cur) {
        Ok(c) => c,
        Err(_) => return structural,
    };
    let mut canon = base;
    for seg in tail.iter().rev() {
        canon.push(seg);
    }
    if cfg!(windows) {
        canon.to_string_lossy().to_lowercase()
    } else {
        canon.to_string_lossy().into_owned()
    }
}

/// Deterministic streaming hash of a JSON value. Discriminant tags keep
/// same-text different-shape values (e.g. `1` vs `"1"`) apart.
fn hash_value(val: &Value, hasher: &mut std::collections::hash_map::DefaultHasher) {
    std::any::type_name::<u8>().hash(hasher);
    walk(val, hasher)
}

fn walk(val: &Value, hasher: &mut std::collections::hash_map::DefaultHasher) {
    match val {
        Value::Null => 0u8.hash(hasher),
        Value::Bool(b) => {
            1u8.hash(hasher);
            b.hash(hasher);
        }
        Value::Number(n) => {
            2u8.hash(hasher);
            if let Some(i) = n.as_i64() {
                i.hash(hasher);
            } else if let Some(f) = n.as_f64() {
                f.to_bits().hash(hasher);
            }
        }
        Value::String(s) => {
            3u8.hash(hasher);
            s.hash(hasher);
        }
        Value::Array(arr) => {
            4u8.hash(hasher);
            arr.len().hash(hasher);
            for v in arr {
                walk(v, hasher);
            }
        }
        Value::Object(obj) => {
            5u8.hash(hasher);
            obj.len().hash(hasher);
            for (k, v) in obj {
                k.hash(hasher);
                walk(v, hasher);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(text: &str) -> ToolResult {
        ToolResult::text("c1", "read", text)
    }

    #[test]
    fn normalize_write_path_with_root_collapses_relative_and_absolute_aliases() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let abs = normalize_write_path_with_root(Some(&root), "src/a.rs");
        assert_eq!(
            abs,
            normalize_write_path_with_root(Some(&root), &format!("{root}/src/./a.rs"))
        );
        assert_eq!(
            abs,
            normalize_write_path_with_root(Some(&root), &format!("{root}/src/b/../a.rs"))
        );
    }

    #[test]
    fn normalize_write_path_collapses_symlink_and_spelling_aliases() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("a.rs"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();

        let abs = normalize_write_path(&real.join("a.rs").to_string_lossy());
        #[cfg(unix)]
        {
            let via_link = normalize_write_path(&tmp.path().join("link/a.rs").to_string_lossy());
            assert_eq!(abs, via_link);
            let via_link_new_dir =
                normalize_write_path(&tmp.path().join("link/new_dir/b.rs").to_string_lossy());
            assert_eq!(
                via_link_new_dir,
                format!(
                    "{}/new_dir/b.rs",
                    real.canonicalize().unwrap().to_string_lossy()
                )
            );
            let dangling_target = tmp.path().join("gone");
            let dangling = tmp.path().join("dang");
            std::os::unix::fs::symlink(&dangling_target, &dangling).unwrap();
            let via_dangling = normalize_write_path(&dangling.join("c.rs").to_string_lossy());
            let direct = normalize_write_path(&dangling_target.join("c.rs").to_string_lossy());
            assert_eq!(via_dangling, direct);
        }
        let nested_new = normalize_write_path(&real.join("sub/new.rs").to_string_lossy());
        assert_eq!(
            nested_new,
            format!("{}/sub/new.rs", abs.trim_end_matches("/a.rs"))
        );
        assert!(!nested_new.is_empty());
    }

    #[test]
    fn same_args_same_key() {
        let input = serde_json::json!({"path": "/foo.rs"});
        assert_eq!(
            ToolDedupCache::key("read", &input),
            ToolDedupCache::key("read", &input)
        );
    }

    #[test]
    fn different_args_or_tool_differ() {
        let input = serde_json::json!({"path": "/foo.rs"});
        assert_ne!(
            ToolDedupCache::key("read", &input),
            ToolDedupCache::key("read", &serde_json::json!({"path": "/bar.rs"}))
        );
        assert_ne!(
            ToolDedupCache::key("read", &input),
            ToolDedupCache::key("grep", &input)
        );
    }

    #[test]
    fn typed_hash_separates_shapes() {
        assert_ne!(
            ToolDedupCache::key("t", &serde_json::json!(1)),
            ToolDedupCache::key("t", &serde_json::json!("1"))
        );
    }

    #[test]
    fn insert_get_clear() {
        let mut cache = ToolDedupCache::new();
        let input = serde_json::json!({"path": "/x.rs"});
        let key = ToolDedupCache::key("read", &input);
        cache.insert(key, &result("x"), None, "read", &input);
        assert_eq!(
            cache
                .get(key, "read", &input)
                .map(|r| r.content[0].to_text()),
            Some("x".to_string())
        );
        cache.clear();
        assert!(cache.get(key, "read", &input).is_none());
    }

    #[test]
    fn digest_collision_does_not_serve_another_entry() {
        let mut cache = ToolDedupCache::new();
        let input_a = serde_json::json!({"path": "/a.rs"});
        let input_b = serde_json::json!({"path": "/b.rs"});
        cache.insert(7, &result("a"), None, "read", &input_a);
        assert!(cache.get(7, "read", &input_b).is_none());
        assert!(cache.get(7, "grep", &input_a).is_none());
    }

    #[test]
    fn argument_key_order_does_not_fork_the_check() {
        let mut cache = ToolDedupCache::new();
        let key = ToolDedupCache::key("read", &serde_json::json!({"path": "/x", "offset": 1}));
        cache.insert(
            key,
            &result("x"),
            None,
            "read",
            &serde_json::json!({"offset": 1, "path": "/x"}),
        );
        assert!(
            cache
                .get(key, "read", &serde_json::json!({"path": "/x", "offset": 1}))
                .is_some()
        );
    }

    #[test]
    fn errors_are_never_cached() {
        let mut cache = ToolDedupCache::new();
        let mut errored = result("boom");
        errored.is_error = true;
        cache.insert(1, &errored, None, "read", &serde_json::json!({}));
        assert!(cache.get(1, "read", &serde_json::json!({})).is_none());
    }

    #[test]
    fn fifo_eviction_at_capacity() {
        let mut cache = ToolDedupCache::new();
        for i in 0..=MAX_CACHE_ENTRIES {
            cache.insert(
                i as u64,
                &result("v"),
                None,
                "read",
                &serde_json::json!({"i": i}),
            );
        }
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        assert!(cache.get(0, "read", &serde_json::json!({"i": 0})).is_none());
        assert!(cache.get(1, "read", &serde_json::json!({"i": 1})).is_some());
    }

    #[test]
    fn invalidate_path_removes_only_matching() {
        let mut cache = ToolDedupCache::new();
        let input_a = serde_json::json!({"path": "/a.rs"});
        let input_b = serde_json::json!({"path": "/b.rs"});
        let key_a = ToolDedupCache::key("read", &input_a);
        let key_b = ToolDedupCache::key("read", &input_b);
        cache.insert(key_a, &result("a"), Some("/a.rs"), "read", &input_a);
        cache.insert(key_b, &result("b"), Some("/b.rs"), "read", &input_b);

        cache.invalidate_path("/a.rs");

        assert!(cache.get(key_a, "read", &input_a).is_none());
        assert!(cache.get(key_b, "read", &input_b).is_some());
    }

    #[test]
    fn any_write_invalidates_pathless_entries() {
        let mut cache = ToolDedupCache::new();
        let grep_input = serde_json::json!({"pattern": "fn foo"});
        let key_g = ToolDedupCache::key("grep", &grep_input);
        cache.insert(key_g, &result("g"), None, "grep", &grep_input);

        cache.invalidate_path("/unrelated.rs");

        assert!(
            cache.get(key_g, "grep", &grep_input).is_none(),
            "a write can change grep results, so the stale entry must go"
        );
    }

    #[test]
    fn read_only_and_write_classification() {
        for name in ["read", "grep", "glob"] {
            assert!(ToolDedupCache::is_read_only(name));
        }
        for name in [
            "write",
            "edit",
            "edit_lines",
            "insert_lines",
            "multiedit",
            "delete",
        ] {
            assert!(ToolDedupCache::is_write(name));
        }
        assert!(!ToolDedupCache::is_read_only("write"));
        assert!(!ToolDedupCache::is_write("read"));
    }

    #[test]
    fn write_paths_cover_multiedit_edits() {
        let input = serde_json::json!({
            "edits": [
                {"path": "/a.rs", "old_string": "x", "new_string": "y"},
                {"path": "/b.rs", "old_string": "x", "new_string": "y"}
            ]
        });
        assert_eq!(
            extract_write_paths("multiedit", &input),
            vec!["/a.rs".to_string(), "/b.rs".to_string()]
        );
        assert_eq!(
            extract_write_paths(
                "write",
                &serde_json::json!({"path": "/c.rs", "content": ""})
            ),
            vec!["/c.rs".to_string()]
        );
        assert!(extract_write_paths("read", &serde_json::json!({"path": "/c.rs"})).is_empty());
        assert_eq!(
            extract_write_paths(
                "apply_patch",
                &serde_json::json!({"patch_text": "*** Begin Patch\n*** Update File: /d.rs\n@@\n-a\n+b\n*** End Patch"})
            ),
            vec!["/d.rs".to_string()]
        );
    }

    #[test]
    fn cached_result_marks_and_rekeys() {
        let cached = result("1: hello");
        let replay = cached_result(&cached, "call-2");
        assert_eq!(replay.call, "call-2");
        assert_eq!(replay.name, "read");
        assert!(replay.content[0].to_text().starts_with("[cached] 1: hello"));
        assert_eq!(cached.content[0].to_text(), "1: hello", "source untouched");
    }

    #[test]
    fn reinserting_existing_key_at_capacity_evicts_nothing() {
        let mut cache = ToolDedupCache::new();
        for i in 0..MAX_CACHE_ENTRIES {
            cache.insert(
                i as u64,
                &result("v"),
                None,
                "read",
                &serde_json::json!({"i": i}),
            );
        }
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        cache.insert(
            0,
            &result("refreshed"),
            None,
            "read",
            &serde_json::json!({"i": 0}),
        );
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES, "no innocent eviction");
        assert!(
            cache.get(1, "read", &serde_json::json!({"i": 1})).is_some(),
            "the FIFO neighbor survives an overwrite"
        );
        // order stays consistent with entries: every key in order is live.
        let live: std::collections::HashSet<u64> = cache.entries.keys().copied().collect();
        assert!(cache.order.iter().all(|k| live.contains(k)));
        assert_eq!(cache.order.len(), cache.entries.len());
    }

    fn rooted_cache() -> ToolDedupCache {
        ToolDedupCache::with_root(Some("/abs/root"))
    }

    #[test]
    fn invalidate_path_matches_absolute_spelling_of_relative_insert() {
        let mut cache = rooted_cache();
        let input = serde_json::json!({"path": "./src/a.rs"});
        let key = ToolDedupCache::key("read", &input);
        cache.insert(key, &result("a"), Some("./src/a.rs"), "read", &input);

        cache.invalidate_path("/abs/root/src/a.rs");

        assert!(cache.get(key, "read", &input).is_none());
    }

    #[test]
    fn invalidate_path_matches_parent_traversal_spelling() {
        let mut cache = rooted_cache();
        let input = serde_json::json!({"path": "src/a.rs"});
        let key = ToolDedupCache::key("read", &input);
        cache.insert(key, &result("a"), Some("src/a.rs"), "read", &input);

        cache.invalidate_path("../root/src/a.rs");

        assert!(cache.get(key, "read", &input).is_none());
    }

    #[test]
    fn normalizer_collapses_dots_and_separators() {
        let norm = PathNormalizer::new(Some("/abs/root"));
        assert_eq!(
            norm.normalize("./src//b/./c.rs"),
            norm.normalize("/abs/root/src/b/c.rs")
        );
        let unrooted = PathNormalizer::new(None);
        assert_eq!(
            unrooted.normalize("src//a.rs"),
            unrooted.normalize("src/./a.rs")
        );
        assert_eq!(unrooted.normalize("src/../lib/a.rs"), "lib/a.rs");
    }

    #[cfg(not(windows))]
    #[test]
    fn normalizer_is_case_sensitive_off_windows() {
        let norm = PathNormalizer::new(Some("/abs/root"));
        assert_ne!(norm.normalize("src/A.rs"), norm.normalize("src/a.rs"));
    }

    #[cfg(windows)]
    #[test]
    fn normalizer_case_folds_on_windows() {
        let norm = PathNormalizer::new(Some("C:\\ws"));
        assert_eq!(norm.normalize("src\\A.rs"), norm.normalize("SRC/a.rs"));
    }

    #[test]
    fn symlink_alias_write_still_drops_pathless_entries() {
        // A write through a symlink alias cannot invalidate the entry cached
        // under the real path (string-space normalization does not resolve
        // symlinks); the mitigation is that every write also drops pathless
        // entries, whose tools resample the tree on their next call.
        let mut cache = rooted_cache();
        let real = serde_json::json!({"path": "/abs/root/src/a.rs"});
        let key_real = ToolDedupCache::key("read", &real);
        cache.insert(
            key_real,
            &result("a"),
            Some("/abs/root/src/a.rs"),
            "read",
            &real,
        );
        let grep_input = serde_json::json!({"pattern": "fn main"});
        let key_g = ToolDedupCache::key("grep", &grep_input);
        cache.insert(key_g, &result("g"), None, "grep", &grep_input);

        cache.invalidate_path("/abs/root/alias/a.rs"); // different name: miss

        // Residual: the aliased-file entry survives; the pathless one must not.
        assert!(cache.get(key_real, "read", &real).is_some());
        assert!(cache.get(key_g, "grep", &grep_input).is_none());
    }
}
