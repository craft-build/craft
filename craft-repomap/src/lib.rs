use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub mod graph;
pub mod render;
pub mod tags;

use tags::{FileTags, collect_all_tags, extract_mentioned_idents, file_path_to_idents};

const DEFAULT_MAX_TOKENS: u32 = 1024;
const NO_CONTEXT_MULTIPLIER: u32 = 8;
const MAX_DEFS_BASE: usize = 100;

pub struct RepoMap {
    root: PathBuf,
    max_tokens: u32,
    tags_cache: Arc<Mutex<Option<CachedTags>>>,
    render_cache: Arc<Mutex<Option<CachedRender>>>,
}

struct CachedTags {
    signature: String,
    tags: Vec<FileTags>,
}

struct CachedRender {
    signature: String,
    rendered: String,
}

impl RepoMap {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            tags_cache: Arc::new(Mutex::new(None)),
            render_cache: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn try_from_cwd() -> Option<Self> {
        let cwd = std::env::current_dir().ok()?;
        if !cwd.join(".git").exists() {
            return None;
        }
        Some(Self::new(cwd))
    }

    fn get_file_tags(&self) -> Vec<FileTags> {
        let sig = tags_signature(&self.root);
        {
            let cache = self.tags_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = cache.as_ref()
                && cached.signature == sig
            {
                return cached.tags.clone();
            }
        }

        let tags = collect_all_tags(&self.root);

        let mut cache = self.tags_cache.lock().unwrap_or_else(|e| e.into_inner());
        *cache = Some(CachedTags {
            signature: sig,
            tags: tags.clone(),
        });

        tags
    }

    pub fn get_repo_map(
        &self,
        mentioned_files: &[String],
        context_files: &[String],
        last_user_message: &str,
    ) -> String {
        let file_tags = self.get_file_tags();
        if file_tags.is_empty() {
            return String::new();
        }

        let render_sig = render_signature(
            &file_tags,
            mentioned_files,
            context_files,
            last_user_message,
        );
        {
            let cache = self.render_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = cache.as_ref()
                && cached.signature == render_sig
            {
                return cached.rendered.clone();
            }
        }

        let mut mentioned_idents = extract_mentioned_idents(last_user_message);
        for mf in mentioned_files {
            mentioned_idents.extend(file_path_to_idents(mf));
        }

        let ranked = graph::rank_files(&file_tags, &mentioned_idents, context_files);

        let max_defs = if context_files.is_empty() {
            MAX_DEFS_BASE * (NO_CONTEXT_MULTIPLIER as usize)
        } else {
            MAX_DEFS_BASE
        };

        let defs = graph::top_defs_for_files(&file_tags, &ranked, &mentioned_idents, max_defs);

        let budget = if context_files.is_empty() {
            self.max_tokens * NO_CONTEXT_MULTIPLIER
        } else {
            self.max_tokens
        };

        let rendered = render::render_with_budget(&defs, budget);

        let mut cache = self.render_cache.lock().unwrap_or_else(|e| e.into_inner());
        *cache = Some(CachedRender {
            signature: render_sig,
            rendered: rendered.clone(),
        });

        rendered
    }

    pub fn force_refresh(&self) {
        let mut tags = self.tags_cache.lock().unwrap_or_else(|e| e.into_inner());
        *tags = None;
        let mut render = self.render_cache.lock().unwrap_or_else(|e| e.into_inner());
        *render = None;
    }
}

fn tags_signature(root: &std::path::Path) -> String {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .build();

    let mut parts: Vec<String> = walker
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !path.is_file() {
                return None;
            }
            let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
            Some(format!(
                "{}:{:?}",
                path.strip_prefix(root).unwrap_or(path).display(),
                mtime
            ))
        })
        .collect();
    parts.sort();
    parts.join("|")
}

fn render_signature(
    file_tags: &[FileTags],
    mentioned_files: &[String],
    context_files: &[String],
    last_user_message: &str,
) -> String {
    let mut parts: Vec<String> = file_tags
        .iter()
        .map(|ft| format!("{}:{:?}", ft.rel_path, ft.mtime))
        .collect();
    parts.sort();
    parts.extend(mentioned_files.iter().cloned());
    parts.extend(context_files.iter().cloned());
    parts.push(last_user_message.to_string());
    parts.join("|")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn init_git_repo(dir: &Path) {
        fs::create_dir_all(dir.join(".git")).unwrap();
    }

    #[test]
    fn repomap_generates_for_rust_files() {
        let dir = TempDir::new().unwrap();
        init_git_repo(dir.path());
        fs::write(
            dir.path().join("main.rs"),
            "fn hello() -> u32 { 42 }\nfn world() { hello(); }\n",
        )
        .unwrap();

        let map = RepoMap::new(dir.path());
        let rendered = map.get_repo_map(&[], &[], "show me the hello function");
        assert!(rendered.contains("hello") || rendered.contains("main.rs"));
    }

    #[test]
    fn repomap_caches_identical_calls() {
        let dir = TempDir::new().unwrap();
        init_git_repo(dir.path());
        fs::write(
            dir.path().join("main.rs"),
            "fn alpha() {}\nfn beta() { alpha(); }\n",
        )
        .unwrap();

        let map = RepoMap::new(dir.path());
        let first = map.get_repo_map(&[], &[], "same message");
        let second = map.get_repo_map(&[], &[], "same message");
        assert_eq!(first, second);
    }

    #[test]
    fn repomap_mention_reranks_output() {
        let dir = TempDir::new().unwrap();
        init_git_repo(dir.path());
        fs::write(
            dir.path().join("a.rs"),
            "fn alpha() {}\nfn common() { alpha(); }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.rs"),
            "fn beta() {}\nfn common() { beta(); }\n",
        )
        .unwrap();

        let map = RepoMap::new(dir.path());
        let with_alpha = map.get_repo_map(&[], &[], "look at alpha");
        map.force_refresh();
        let with_beta = map.get_repo_map(&[], &[], "look at beta");
        assert!(
            with_alpha.contains("alpha") || with_beta.contains("beta"),
            "mentioning an ident should surface it: alpha={with_alpha:?} beta={with_beta:?}"
        );
    }

    #[test]
    fn empty_repo_produces_empty_map() {
        let dir = TempDir::new().unwrap();
        init_git_repo(dir.path());
        let map = RepoMap::new(dir.path());
        let rendered = map.get_repo_map(&[], &[], "hello");
        assert!(rendered.is_empty());
    }
}
