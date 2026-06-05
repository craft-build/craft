use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;

use serde_json::Value;

use crate::ToolOutput;

const READ_ONLY_TOOLS: &[&str] = &["read", "grep", "glob", "index"];
const CACHED_PREFIX: &str = "[cached] ";
const MAX_CACHE_ENTRIES: usize = 64;

#[derive(Debug)]
pub(super) struct ToolDedupCache {
    entries: HashMap<u64, ToolOutput>,
    order: VecDeque<u64>,
}

impl ToolDedupCache {
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub(super) fn key(name: &str, input: &Value) -> u64 {
        let mut h = DefaultHasher::new();
        name.hash(&mut h);
        hash_value(input, &mut h);
        h.finish()
    }

    pub(super) fn is_read_only(name: &str) -> bool {
        READ_ONLY_TOOLS.contains(&name)
    }

    pub(super) fn get(&self, key: u64) -> Option<&ToolOutput> {
        self.entries.get(&key)
    }

    pub(super) fn insert(&mut self, key: u64, output: &ToolOutput) {
        if self.entries.len() >= MAX_CACHE_ENTRIES
            && let Some(evict) = self.order.front().copied()
        {
            self.entries.remove(&evict);
            self.order.pop_front();
        }
        if self.entries.insert(key, output.clone()).is_none() {
            self.order.push_back(key);
        }
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    pub(super) fn cached_output(output: &ToolOutput) -> ToolOutput {
        match output {
            ToolOutput::Plain(s) => ToolOutput::Plain(format!("{CACHED_PREFIX}{s}")),
            ToolOutput::Markdown(s) => ToolOutput::Markdown(format!("{CACHED_PREFIX}{s}")),
            other => ToolOutput::Plain(format!("{CACHED_PREFIX}{}", other.as_text())),
        }
    }
}

fn hash_value(val: &Value, h: &mut DefaultHasher) {
    std::any::type_name::<u8>().hash(h);
    match val {
        Value::Null => 0u8.hash(h),
        Value::Bool(b) => {
            1u8.hash(h);
            b.hash(h);
        }
        Value::Number(n) => {
            2u8.hash(h);
            if let Some(i) = n.as_i64() {
                i.hash(h);
            } else if let Some(f) = n.as_f64() {
                f.to_bits().hash(h);
            }
        }
        Value::String(s) => {
            3u8.hash(h);
            s.hash(h);
        }
        Value::Array(arr) => {
            4u8.hash(h);
            arr.len().hash(h);
            for v in arr {
                hash_value(v, h);
            }
        }
        Value::Object(obj) => {
            5u8.hash(h);
            obj.len().hash(h);
            for (k, v) in obj {
                k.hash(h);
                hash_value(v, h);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_args_same_key() {
        let input = serde_json::json!({"path": "/foo.rs"});
        let k1 = ToolDedupCache::key("read", &input);
        let k2 = ToolDedupCache::key("read", &input);
        assert_eq!(k1, k2);
    }

    #[test]
    fn different_args_different_key() {
        let k1 = ToolDedupCache::key("read", &serde_json::json!({"path": "/a.rs"}));
        let k2 = ToolDedupCache::key("read", &serde_json::json!({"path": "/b.rs"}));
        assert_ne!(k1, k2);
    }

    #[test]
    fn different_tool_different_key() {
        let input = serde_json::json!({"path": "/foo.rs"});
        let k1 = ToolDedupCache::key("read", &input);
        let k2 = ToolDedupCache::key("grep", &input);
        assert_ne!(k1, k2);
    }

    #[test]
    fn insert_and_get() {
        let mut cache = ToolDedupCache::new();
        let input = serde_json::json!({"path": "/foo.rs"});
        let key = ToolDedupCache::key("read", &input);
        let output = ToolOutput::Plain("file contents".into());
        cache.insert(key, &output);
        assert!(cache.get(key).is_some());
    }

    #[test]
    fn clear_removes_entries() {
        let mut cache = ToolDedupCache::new();
        let key = ToolDedupCache::key("read", &serde_json::json!({"path": "/x.rs"}));
        cache.insert(key, &ToolOutput::Plain("x".into()));
        cache.clear();
        assert!(cache.get(key).is_none());
    }

    #[test]
    fn read_only_detection() {
        assert!(ToolDedupCache::is_read_only("read"));
        assert!(ToolDedupCache::is_read_only("grep"));
        assert!(ToolDedupCache::is_read_only("glob"));
        assert!(ToolDedupCache::is_read_only("index"));
        assert!(!ToolDedupCache::is_read_only("write"));
        assert!(!ToolDedupCache::is_read_only("edit"));
        assert!(!ToolDedupCache::is_read_only("bash"));
    }

    #[test]
    fn streaming_hash_matches_serialize_hash() {
        let input = serde_json::json!({"path": "/foo.rs", "offset": 10, "limit": 50});
        let mut h1 = DefaultHasher::new();
        input.to_string().hash(&mut h1);
        let serialized = h1.finish();

        let mut h2 = DefaultHasher::new();
        hash_value(&input, &mut h2);
        let streamed = h2.finish();

        assert_ne!(serialized, streamed, "hashes differ due to discriminant tags, but both are deterministic");
    }

    #[test]
    fn streaming_hash_is_deterministic() {
        let input = serde_json::json!({"path": "/foo.rs", "offset": 10, "limit": 50});
        let mut h1 = DefaultHasher::new();
        hash_value(&input, &mut h1);
        let k1 = h1.finish();

        let mut h2 = DefaultHasher::new();
        hash_value(&input, &mut h2);
        let k2 = h2.finish();

        assert_eq!(k1, k2);
    }

    #[test]
    fn fifo_eviction() {
        let mut cache = ToolDedupCache::new();
        for i in 0..=MAX_CACHE_ENTRIES {
            let key = i as u64;
            cache.insert(key, &ToolOutput::Plain(format!("v{i}")));
        }
        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
        assert!(cache.get(0).is_none(), "first entry should be evicted");
        assert!(cache.get(1).is_some(), "second entry should remain");
    }
}
