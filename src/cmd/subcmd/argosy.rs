//! `craft argosy`: lifecycle management for the project's argosy store,
//! mirroring the argosy CLI's semantics on craft's own layout — the local
//! bundle lives at `<state>/projects/<project-id>/argosy` (the same store
//! the `memory` tool and Flow documents use), pulled checkouts sit beside
//! it, and the global store at `<state>/global`.

use std::path::{Path, PathBuf};

use argosy::context::ProjectContext;
use argosy::index::{Filter, Index, Query, VectorStore};
use argosy::package::import_styleguide_yaml;
use argosy::{Argosy, ValidationReport};
use color_eyre::Result;
use color_eyre::eyre::{Context, bail};

use craft_storage::argosy_index::{CraftEmbeddingProvider, FileVecStore};
use craft_storage::argosy_store::ArgosyStore;
use craft_storage::flow::project_id;
use craft_storage::paths;

use crate::cli::{ArgosyAction, ArgosyIndexAction};

pub async fn run(action: ArgosyAction) -> Result<()> {
    match action {
        ArgosyAction::Init => init(&local_argosy_dir()?, &project_name()?),
        ArgosyAction::Pull {
            source,
            name,
            global,
        } => pull(&source, &name, global),
        ArgosyAction::Validate { path } => {
            validate(path.as_deref().unwrap_or(&local_argosy_dir()?))
        }
        ArgosyAction::Index { verb } => match verb {
            ArgosyIndexAction::Status => index_status(),
            ArgosyIndexAction::Build => index_build(),
            ArgosyIndexAction::Query { text, k } => index_query(&text, k),
        },
        ArgosyAction::Convert { yaml_dir } => convert(&yaml_dir, &project_name()?),
    }
}

/// Nearest ancestor holding a `.git` marker, else `cwd`.
fn project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("resolve the working directory")?;
    Ok(cwd
        .ancestors()
        .find(|a| a.join(".git").exists())
        .unwrap_or(cwd.as_path())
        .to_path_buf())
}

fn project_name() -> Result<String> {
    Ok(project_root()?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string()))
}

fn local_argosy_dir() -> Result<PathBuf> {
    let state = paths::state_dir().context("resolve the craft state dir")?;
    let id = project_id(&project_root()?);
    Ok(state.join("projects").join(id).join("argosy"))
}

/// Bundle directories (holding `argosy.md`) beside the local bundle, plus
/// the global store — the same discovery the styleguide tools use.
fn imported_bundles(local: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(project_dir) = local.parent() {
        roots.extend(bundle_dirs(project_dir, Some(local)));
        for ancestor in project_dir.ancestors().skip(1).take(2) {
            roots.extend(bundle_dirs(&ancestor.join("global"), None));
        }
    }
    roots
}

fn bundle_dirs(dir: &Path, skip: Option<&Path>) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("argosy.md").is_file() && skip.is_none_or(|s| *p != *s))
        .collect();
    dirs.sort();
    dirs
}

fn open_context(local: &Path) -> Result<ProjectContext> {
    ProjectContext::open(local, imported_bundles(local))
        .with_context(|| format!("open the argosy at {}", local.display()))
}

fn init(dir: &Path, name: &str) -> Result<()> {
    let store = ArgosyStore::open_or_init(dir, name)
        .with_context(|| format!("initialize the argosy at {}", dir.display()))?;
    let manifest = store.with(|argosy| argosy.manifest().clone());
    println!(
        "created {} {} at {}",
        manifest.name(),
        manifest.argosy_version(),
        dir.display()
    );
    Ok(())
}

fn pull(source: &str, name: &str, global: bool) -> Result<()> {
    let state = paths::state_dir().context("resolve the craft state dir")?;
    let root = if global {
        state.join("global")
    } else {
        let dir = local_argosy_dir()?;
        dir.parent().map(Path::to_path_buf).unwrap_or(dir)
    };
    let argosy = argosy::pull::clone_as_checkout(source, &root, name)
        .with_context(|| format!("pull `{source}` as checkout `{name}`"))?;
    let dest = root.join(name);
    println!(
        "pulled {} {} into {}",
        argosy.manifest().name(),
        argosy.manifest().argosy_version(),
        dest.display()
    );
    Ok(())
}

fn validate(path: &Path) -> Result<()> {
    if !path.join("argosy.md").is_file() {
        bail!(
            "no argosy at {} — run `craft argosy init` in the project root",
            path.display()
        );
    }
    let report = Argosy::validate(path);
    if report.is_conformant() {
        match Argosy::open(path) {
            Ok(argosy) => println!(
                "OK: {} {}",
                argosy.manifest().name(),
                argosy.manifest().argosy_version()
            ),
            Err(_) => println!("OK"),
        }
        return Ok(());
    }
    print!("{report}");
    bail!("validation failed: {} finding(s)", report.findings().len());
}

fn index_path(local: &Path) -> PathBuf {
    local.join(".argosy").join("index.json")
}

fn index_status() -> Result<()> {
    let local = local_argosy_dir()?;
    let db = index_path(&local);
    if !db.is_file() {
        println!(
            "no index at {} — run `craft argosy index build`",
            db.display()
        );
        return Ok(());
    }
    let store = FileVecStore::open(&db).context("open the index store")?;
    let units = store
        .unit_hashes()
        .context("read the index unit hashes")?
        .len();
    println!(
        "model: {}\nunits: {units}\ndb: {}",
        store.model_id().unwrap_or("<unrecorded>"),
        db.display()
    );
    Ok(())
}

fn index_build() -> Result<()> {
    let local = local_argosy_dir()?;
    if !local.join("argosy.md").is_file() {
        bail!(
            "no local argosy for this project at {} — run `craft argosy init` first",
            local.display()
        );
    }
    let context = open_context(&local)?;
    let db = index_path(&local);
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut index = Index::new(CraftEmbeddingProvider::new()?, FileVecStore::open(&db)?);
    let report = index
        .reconcile(&context)
        .with_context(|| format!("reconcile the index at {}", db.display()))?;
    println!(
        "index {}: {} upserted, {} removed, {} unchanged [{}]",
        if report.rebuilt { "rebuilt" } else { "updated" },
        report.upserted,
        report.removed,
        report.unchanged,
        report.model_id
    );
    Ok(())
}

fn index_query(text: &str, k: usize) -> Result<()> {
    let local = local_argosy_dir()?;
    let db = index_path(&local);
    if !db.is_file() {
        println!(
            "no index at {} — run `craft argosy index build`",
            db.display()
        );
        return Ok(());
    }
    let context = open_context(&local)?;
    let index = Index::new(CraftEmbeddingProvider::new()?, FileVecStore::open(&db)?);
    let query = Query {
        text: text.to_string(),
        k,
        filter: Filter::default(),
    };
    let hits = index
        .search(&context, &query)
        .with_context(|| format!("search the index at {}", db.display()))?;
    for hit in &hits {
        let description = hit.meta.description.as_deref().unwrap_or("");
        println!(
            "{:.4}  {}  —  {}",
            hit.score,
            hit.concept.to_uri(),
            description
        );
    }
    Ok(())
}

fn convert(yaml_dir: &Path, project_name: &str) -> Result<()> {
    let local = local_argosy_dir()?;
    if !local.join("argosy.md").is_file() {
        bail!(
            "no local argosy for this project at {} — run `craft argosy init` first",
            local.display()
        );
    }
    let store = ArgosyStore::open_or_init(&local, project_name)
        .with_context(|| format!("open the argosy at {}", local.display()))?;
    store.with(|argosy| {
        let report = import_styleguide_yaml(argosy, yaml_dir)
            .with_context(|| format!("import styleguide YAML from {}", yaml_dir.display()))?;
        if report.yaml_files_seen == 0 && report.findings.is_empty() {
            eprintln!(
                "warning: no .yaml or .yml files found in {} — nothing imported",
                yaml_dir.display()
            );
        }
        println!(
            "written: {} rule(s); skipped (existing): {}",
            report.written,
            report.skipped_existing.len()
        );
        for skipped in &report.skipped_existing {
            println!("skipped: {skipped}");
        }
        if !report.findings.is_empty() {
            print!(
                "{}",
                ValidationReport::from_findings(report.findings.clone())
            );
        } else if report.written > 0 {
            println!("note: run `craft argosy index build` to make the new rules searchable");
        }
        if !report.findings.is_empty() {
            bail!(
                "import produced {} validation finding(s)",
                report.findings.len()
            );
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_write_validate_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("argosy");
        init(&dir, "roundtrip-project").unwrap();
        let store = ArgosyStore::open_or_init(&dir, "roundtrip-project").unwrap();
        store
            .write_memory("decisions", "use argosy for notes")
            .unwrap();
        validate(&dir).unwrap();
    }

    #[test]
    fn validate_reports_missing_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(validate(&tmp.path().join("argosy")).is_err());
    }
}
