//! Scheduling claims for filesystem tools.
//!
//! Resource keys are the normalized spellings produced by
//! [`crate::run::dedup::normalize_write_path_with_root`], anchored at the
//! workspace root so relative and absolute requests of one file claim the
//! same key. The scheme:
//!
//! - readers claim `reads` of their normalized path (a file, or the
//!   directory a walk starts from);
//! - writers claim `writes` of every normalized target **plus every
//!   ancestor directory up to and including the workspace root**, so a
//!   directory walk conflicts with any write beneath it;
//! - as a consequence, two writes to different files in the same directory
//!   also conflict through their shared ancestor key — deliberately
//!   conservative;
//! - targets outside the workspace root (the session plan file) claim the
//!   target alone: nothing walks there.

use std::path::Path;

use animist::agent::tool::ToolScheduling;

use crate::run::dedup::normalize_write_path_with_root;

/// Reads of the given paths, normalized against the workspace root.
pub(crate) fn reads(root: Option<&str>, paths: &[String]) -> ToolScheduling {
    ToolScheduling::Resources {
        reads: paths.iter().map(|path| normalize(root, path)).collect(),
        writes: Vec::new(),
    }
}

/// Writes of the given paths plus their ancestor directories, normalized
/// against the workspace root. An empty path list (arguments that name no
/// target) is exclusive: it conflicts with everything.
pub(crate) fn writes(root: Option<&str>, paths: &[String]) -> ToolScheduling {
    if paths.is_empty() {
        return ToolScheduling::Exclusive;
    }
    ToolScheduling::Resources {
        reads: Vec::new(),
        writes: write_keys(root, paths),
    }
}

/// The normalized read keys for a set of targets: each path alone, as
/// readers claim.
pub(crate) fn read_keys(root: Option<&str>, paths: &[String]) -> Vec<String> {
    paths.iter().map(|path| normalize(root, path)).collect()
}

/// The normalized write keys for a set of targets: each target plus its
/// ancestor directories, clamped at the workspace root. Shared by the
/// claims and the cross-agent path locks so a claim and its lock never
/// disagree.
pub(crate) fn write_keys(root: Option<&str>, paths: &[String]) -> Vec<String> {
    let root_key = root.map(|root| normalize(Some(root), root));
    let mut keys: Vec<String> = paths.iter().map(|path| normalize(root, path)).collect();
    for path in paths {
        keys.extend(ancestor_dirs(&root_key, &normalize(root, path)));
    }
    keys.sort();
    keys.dedup();
    keys
}

/// The normalized ancestor directories of `path` up to and including the
/// root. Paths outside the root claim no ancestors.
fn ancestor_dirs(root_key: &Option<String>, path: &str) -> Vec<String> {
    let Some(root) = root_key else {
        return Vec::new();
    };
    let root = Path::new(root.as_str());
    if !Path::new(path).starts_with(root) {
        return Vec::new();
    }
    let mut keys = Vec::new();
    let mut dir = Path::new(path).parent();
    while let Some(current) = dir {
        if current != root && !current.starts_with(root) {
            break;
        }
        keys.push(current.display().to_string());
        dir = current.parent();
    }
    keys
}

fn normalize(root: Option<&str>, path: &str) -> String {
    normalize_write_path_with_root(root, path)
}
