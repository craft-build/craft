use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const DEFAULT_MAX_ENTRIES: usize = 100;

/// In-memory store mapping content hashes to their original (uncompressed) text.
/// Used by CCR (Reversible Compression) so the LLM can retrieve original content.
pub(crate) struct CompressionStore {
    entries: HashMap<String, String>,
    order: Vec<String>,
    max_entries: usize,
}

impl CompressionStore {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: Vec::new(),
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }

    /// Store an original and return its hash marker. If the hash already exists
    /// with different content, extends the hash to avoid collision.
    pub(crate) fn put(&mut self, original: &str) -> String {
        let mut hash = short_hash(original);
        // Handle hash collision: if hash exists with different content, extend it
        if let Some(existing) = self.entries.get(&hash)
            && existing != original
        {
            hash = extended_hash(original);
        }
        if self.entries.get(&hash).is_some_and(|e| e == original) {
            return hash;
        }
        // Evict oldest if at capacity
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

    pub(crate) fn get(&self, hash: &str) -> Option<&str> {
        self.entries.get(hash).map(String::as_str)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Thread-safe handle for sharing CompressionStore between the compression
/// pipeline and the retrieve tool execution.
pub(crate) type SharedCompressionStore = Arc<Mutex<CompressionStore>>;

pub(crate) fn shared_store() -> SharedCompressionStore {
    Arc::new(Mutex::new(CompressionStore::new()))
}

/// Produce a short, unique-enough hash from content (first 8 hex chars of a simple hash).
fn short_hash(content: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:08x}", hasher.finish())
}

/// Extended hash (16 hex chars) used when the short hash collides.
fn extended_hash(content: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    let h1 = hasher.finish();
    // Hash again for more bits
    h1.hash(&mut hasher);
    let h2 = hasher.finish();
    format!("{:08x}{:04x}", h1, h2 & 0xFFFF)
}

/// Build a retrieval marker string to append to compressed output.
pub(crate) fn retrieval_marker(
    original_lines: usize,
    compressed_lines: usize,
    hash: &str,
) -> String {
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
        assert!(marker.contains("50"));
        assert!(marker.contains("5"));
        assert!(marker.contains("abc12345"));
        assert!(marker.contains("Retrieve original"));
    }

    #[test]
    fn shared_store_is_usable() {
        let store = shared_store();
        let hash = store.lock().unwrap().put("test");
        assert_eq!(store.lock().unwrap().get(&hash), Some("test"));
    }
}
