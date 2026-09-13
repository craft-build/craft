//! Reversible compression store: keeps originals of request-view-replaced
//! tool results keyed by content hash, so the `retrieve` tool can restore
//! them on demand. In-memory and per session; entries are never persisted.
//!
//! Ported from the reference's `agent/compression_store.rs`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const DEFAULT_MAX_ENTRIES: usize = 100;

/// Maps content hashes to their original (uncompressed) text with FIFO
/// eviction, so the model can retrieve original content via markers.
pub struct CompressionStore {
    entries: HashMap<String, String>,
    order: Vec<String>,
    max_entries: usize,
}

impl CompressionStore {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: Vec::new(),
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }

    /// Store an original and return its hash marker. Identical content
    /// returns the existing hash; a hash collision with different content
    /// extends the hash.
    pub fn put(&mut self, original: &str) -> String {
        let mut hash = short_hash(original);
        if let Some(existing) = self.entries.get(&hash)
            && existing != original
        {
            hash = extended_hash(original);
        }
        if self.entries.get(&hash).is_some_and(|e| e == original) {
            return hash;
        }
        if self.entries.len() >= self.max_entries
            && let Some(evicted) = self.order.first().cloned()
        {
            self.entries.remove(&evicted);
            self.order.remove(0);
        }
        self.entries.insert(hash.clone(), original.to_owned());
        self.order.push(hash.clone());
        hash
    }

    pub fn get(&self, hash: &str) -> Option<&str> {
        self.entries.get(hash).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for CompressionStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe handle shared between the request-build path and the
/// `retrieve` tool.
pub type SharedCompressionStore = Arc<Mutex<CompressionStore>>;

pub fn shared_store() -> SharedCompressionStore {
    Arc::new(Mutex::new(CompressionStore::new()))
}

/// First 8 hex chars of a DefaultHasher pass: short, unique-enough key.
fn short_hash(content: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:08x}", hasher.finish())
}

/// Extended 12-hex hash used when the short hash collides.
fn extended_hash(content: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    let h1 = hasher.finish();
    h1.hash(&mut hasher);
    let h2 = hasher.finish();
    format!("{:08x}{:04x}", h1, h2 & 0xFFFF)
}

/// Marker appended to replaced content so the model knows it can call
/// `retrieve` with the hash to get the original back.
pub fn retrieval_marker(original_lines: usize, compressed_lines: usize, hash: &str) -> String {
    format!(
        "\n\n[{} lines compressed from {}. Retrieve original: hash={}]",
        compressed_lines, original_lines, hash,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_get_roundtrip() {
        let mut store = CompressionStore::new();
        let hash = store.put("original content here");
        assert_eq!(store.get(&hash), Some("original content here"));
    }

    #[test]
    fn same_content_returns_same_hash() {
        let mut store = CompressionStore::new();
        let h1 = store.put("hello");
        let h2 = store.put("hello");
        assert_eq!(h1, h2);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn different_content_different_hash() {
        let mut store = CompressionStore::new();
        let h1 = store.put("aaa");
        let h2 = store.put("bbb");
        assert_ne!(h1, h2);
    }

    #[test]
    fn lru_eviction_at_capacity() {
        let mut store = CompressionStore::new();
        store.max_entries = 3;
        let h1 = store.put("one");
        let _ = store.put("two");
        let _ = store.put("three");
        assert_eq!(store.len(), 3);
        let _ = store.put("four");
        assert_eq!(store.len(), 3);
        assert!(store.get(&h1).is_none(), "oldest should be evicted");
    }

    #[test]
    fn retrieval_marker_format() {
        let marker = retrieval_marker(50, 5, "abc12345");
        assert!(marker.contains("5 lines compressed from 50"));
        assert!(marker.contains("hash=abc12345"));
        assert!(marker.contains("Retrieve original"));
    }

    #[test]
    fn hash_collision_with_different_content_extends_hash() {
        let mut store = CompressionStore::new();
        let a = "content-a";
        let short = short_hash(a);
        store.entries.insert(short.clone(), "different".into());
        store.order.push(short.clone());
        let h = store.put(a);
        assert_ne!(h, short, "collision must not reuse the short hash");
        assert_eq!(store.get(&h), Some(a));
        assert_eq!(store.get(&short), Some("different"));
    }

    #[test]
    fn shared_store_is_usable() {
        let store = shared_store();
        let hash = store.lock().unwrap().put("test");
        assert_eq!(store.lock().unwrap().get(&hash), Some("test"));
    }
}
