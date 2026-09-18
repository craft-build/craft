//! Plans storage: slug-named markdown plan files under `<state>/plans/`.
//!
//! Ported from the reference `craft-storage/src/plans.rs`. Randomness comes
//! from UUIDv4 bytes instead of `getrandom` (not a dependency here).

use std::path::PathBuf;
use std::sync::LazyLock;

use uuid::Uuid;

use crate::storage::{StateDir, StorageError};

const PLANS_DIR: &str = "plans";
const SLUG_RETRIES: usize = 10;

static ADJECTIVES: LazyLock<Vec<&str>> =
    LazyLock::new(|| load_words(include_str!("words/adjectives.txt")));
static NOUNS: LazyLock<Vec<&str>> = LazyLock::new(|| load_words(include_str!("words/nouns.txt")));

fn load_words(text: &'static str) -> Vec<&'static str> {
    let words: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    assert!(!words.is_empty(), "word list must not be empty");
    words
}

/// Returns a fresh, not-yet-existing `<adjective>-<adjective>-<noun>.md`
/// path inside the plans directory (created if needed).
pub fn new_plan_path(dir: &StateDir) -> Result<PathBuf, StorageError> {
    let plans_dir = dir.ensure_subdir(PLANS_DIR)?;
    for _ in 0..SLUG_RETRIES {
        let path = plans_dir.join(format!("{}.md", generate_slug()));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(StorageError::SlugCollision)
}

fn generate_slug() -> String {
    let buf = *Uuid::new_v4().as_bytes();
    let adj1_idx = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]) as usize % ADJECTIVES.len();
    let mut adj2_idx =
        u32::from_le_bytes([buf[10], buf[11], buf[12], buf[13]]) as usize % ADJECTIVES.len();
    let noun_idx = u32::from_le_bytes([buf[9], buf[14], buf[15], buf[0]]) as usize % NOUNS.len();
    if adj1_idx == adj2_idx {
        adj2_idx = (adj2_idx + 1) % ADJECTIVES.len();
    }
    format!(
        "{}-{}-{}",
        ADJECTIVES[adj1_idx], ADJECTIVES[adj2_idx], NOUNS[noun_idx]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StateDir;
    use std::fs;

    #[test]
    fn slug_format_invariants() {
        let slug = generate_slug();
        let parts: Vec<&str> = slug.split('-').collect();
        assert_eq!(parts.len(), 3, "expected 3 parts: {slug}");
        assert!(
            parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase())),
            "invalid part in slug: {slug}",
        );
        assert_ne!(parts[0], parts[1], "duplicate adjective in slug: {slug}");
    }

    #[test]
    fn new_plan_path_under_plans_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let path = new_plan_path(&dir).unwrap();
        assert!(path.starts_with(tmp.path().join("plans")));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("md"));
        assert!(!path.exists());
    }

    #[test]
    fn new_plan_path_avoids_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let a = new_plan_path(&dir).unwrap();
        fs::write(&a, "# plan").unwrap();
        let b = new_plan_path(&dir).unwrap();
        assert_ne!(a, b);
        assert!(!b.exists());
    }
}
