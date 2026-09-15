//! Tool dedup cache: a bounded FIFO cache keyed by (tool, arguments) that
//! lets read-only tools answer identical repeat calls from cache instead of
//! re-executing. Entries are invalidated per-path when a write tool touches
//! the file, and the whole cache is cleared before compaction (compacted
//! history may describe a different world than the one the cache sampled).

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::history::{ToolResult, ToolResultContent};

const READ_ONLY_TOOLS: &[&str] = &["read", "grep", "glob"];
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
}

impl ToolDedupCache {
    pub fn new() -> Self {
        Self::default()
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
    /// capacity. `path` (the argument path, if any) scopes invalidation.
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
            path: path.map(String::from),
            check: canonical_args(name, input),
        };
        if self.entries.len() >= MAX_CACHE_ENTRIES
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
    /// sample the whole tree, so any write can change their answer).
    pub fn invalidate_path(&mut self, path: &str) {
        self.entries
            .retain(|_, e| e.path.is_some() && e.path.as_deref() != Some(path));
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
    Arc::new(Mutex::new(ToolDedupCache::new()))
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
}
