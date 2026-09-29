//! BarkML configuration loading and projection helpers.
//!
//! Craft's global config is a single merged BarkML document assembled from
//! `craft.bml` plus every `*.bml` in each config search dir (later files
//! deep-merge, later definitions win). Per-concern sections (`agent`,
//! `provider`, `mcp`, `permissions`, ...) are extracted from that one
//! document, so unrelated blocks never trip `deny_unknown_fields`.
//!
//! BarkML's generic serde path projects labeled blocks under opaque uid
//! keys, so extraction is done by hand: a `Statement` subtree is projected
//! onto `serde_json::Value`, and the existing serde-derived structs are
//! deserialized from that with `serde_json::from_value`. All defaults,
//! `deny_unknown_fields`, and custom `Deserialize` impls keep working.

use std::path::{Path, PathBuf};

use barkml::{Data, Loader, Metadata, Statement, StatementData, Value as BmlValue};
use indexmap::IndexMap;
use serde_json::{Map, Value as Json};

/// Parse a standalone BarkML document.
pub fn parse(text: &str) -> Result<Statement, barkml::Error> {
    let mut doc = barkml::from_str(text)?;
    normalize(&mut doc);
    Ok(doc)
}

/// Apply [`normalize`] to a loaded document and return it.
pub fn finalize(mut doc: Statement) -> Statement {
    normalize(&mut doc);
    doc
}

/// Deep-merge labeled blocks that share an identity.
///
/// The loader merges by storage key, and labeled blocks key by uid, so
/// `provider "x"` arriving from two files survives as two siblings. Later
/// definitions must win per-field, so they are folded together here.
pub fn normalize(stmt: &mut Statement) {
    for child in stmt.children_mut() {
        normalize(child);
    }
    let Some(children) = group_mut(stmt) else {
        return;
    };
    let entries: Vec<(String, Statement)> = children.drain(..).collect();
    let mut merged: IndexMap<String, Statement> = IndexMap::new();
    let mut identity_keys: IndexMap<String, String> = IndexMap::new();
    for (key, child) in entries {
        let identity = {
            let (id, labels) = child.identity();
            format!(
                "{}\u{0}{}",
                id,
                labels
                    .iter()
                    .map(|l| l.as_string().cloned().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\u{0}")
            )
        };
        match identity_keys.get(&identity) {
            Some(existing_key) => {
                let existing_key = existing_key.clone();
                if let Some(dst) = merged.get_mut(&existing_key) {
                    deep_merge(dst, &child);
                }
            }
            None => {
                identity_keys.insert(identity, key.clone());
                merged.insert(key, child);
            }
        }
    }
    *children = merged;
}

/// Merge `src` into `dst`, later (src) values winning, recursively.
fn deep_merge(dst: &mut Statement, src: &Statement) {
    let Some(src_children) = src.get_grouped() else {
        return;
    };
    let Some(dst_children) = group_mut(dst) else {
        return;
    };
    for (key, src_child) in src_children {
        match dst_children.get_mut(key) {
            Some(dst_child) if dst_child.data.has_children() && src_child.data.has_children() => {
                deep_merge(dst_child, src_child);
            }
            _ => {
                dst_children.insert(key.clone(), src_child.clone());
            }
        }
    }
}

/// One file that failed to load into the merged document.
pub type LoadError = (PathBuf, barkml::Error);

/// The merged global BarkML document plus any per-file load failures.
pub struct LoadedGlobal {
    /// The merged document, or `None` when no bml source existed at all.
    pub doc: Option<Statement>,
    pub errors: Vec<LoadError>,
}

/// Load and merge every global bml source, preserving the legacy search
/// order (legacy `~/.craft/` first, then the XDG config dir).
///
/// Within each search dir `D`: `D/craft.bml` (for the XDG dir,
/// `~/.config/craft.bml` — the dir's sibling) is loaded first, then every
/// `*.bml` in `D` itself in sorted order. Everything merges into one main
/// module with deep merge and later-wins collisions.
pub fn load_global() -> LoadedGlobal {
    let xdg = crate::paths::xdg_config_dir().ok();
    load_global_from(&crate::paths::config_search_dirs(), xdg.as_deref())
}

/// Pure core of [`load_global`], taking the search dirs explicitly so tests
/// can hand it tempdirs. `xdg` is the dir whose single-file spelling is the
/// sibling `craft.bml`.
pub fn load_global_from(dirs: &[PathBuf], xdg: Option<&Path>) -> LoadedGlobal {
    let mut loader = barkml::StandardLoader::builder()
        .allow_collisions(true)
        .validate_on_load(true)
        .build();
    let mut errors = Vec::new();
    let mut found = false;

    for dir in dirs {
        // The XDG layout keeps the single file next to the split dir.
        let single = if xdg == Some(dir.as_path()) {
            dir.parent()
                .map(|p| p.join("craft.bml"))
                .unwrap_or_else(|| dir.join("craft.bml"))
        } else {
            dir.join("craft.bml")
        };
        if single.is_file() {
            found = true;
            if let Err(e) = loader.add_file(&single) {
                errors.push((single, e));
            }
        }
        if dir.is_dir() {
            match barkml::utils::discover_files(dir) {
                Ok(files) if !files.is_empty() => {
                    found = true;
                    for file in files {
                        if let Err(e) = loader.add_file(&file) {
                            errors.push((file, e));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let doc = if found && errors.is_empty() {
        loader.load().ok().map(|mut doc| {
            normalize(&mut doc);
            doc
        })
    } else {
        None
    };
    LoadedGlobal {
        doc: if found { doc } else { None },
        errors,
    }
}

/// Legacy TOML config files that are now ignored, for migration warnings.
pub fn legacy_toml_files() -> Vec<PathBuf> {
    let dirs = crate::paths::config_search_dirs();
    legacy_toml_files_in(&dirs)
}

/// Pure core of [`legacy_toml_files`].
pub fn legacy_toml_files_in(dirs: &[PathBuf]) -> Vec<PathBuf> {
    ["agent.toml", "mcp.toml", "permissions.toml"]
        .into_iter()
        .filter_map(|name| dirs.iter().map(|d| d.join(name)).find(|p| p.exists()))
        .collect()
}

/// Warn (once per load site) when a legacy TOML config exists and no bml
/// source was found. Not an error: a stale file must never block startup.
pub fn warn_legacy_toml(bml_found: bool) {
    if bml_found {
        return;
    }
    let legacy = legacy_toml_files();
    if legacy.is_empty() {
        return;
    }
    let first = legacy[0].display();
    tracing::warn!(
        "ignoring legacy TOML config {first}: rename it to ~/.config/craft.bml or split it \
         into ~/.config/craft/*.bml (craft now reads BarkML only); using defaults"
    );
}

// ---------------------------------------------------------------------------
// Statement → serde_json::Value projection
// ---------------------------------------------------------------------------

/// Project a BarkML value onto JSON.
pub fn value_json(value: &BmlValue) -> Json {
    match &value.data {
        Data::String(s) | Data::Symbol(s) => Json::String(s.clone()),
        Data::Signed(v) | Data::I64(v) => Json::Number((*v).into()),
        Data::I128(v) => serde_json::Number::from_i128(*v)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Data::I8(v) => Json::Number(i64::from(*v).into()),
        Data::I16(v) => Json::Number(i64::from(*v).into()),
        Data::I32(v) => Json::Number(i64::from(*v).into()),
        Data::Unsigned(v) | Data::U64(v) => Json::Number((*v).into()),
        Data::U128(v) => serde_json::Number::from_u128(*v)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Data::U8(v) => Json::Number(u64::from(*v).into()),
        Data::U16(v) => Json::Number(u64::from(*v).into()),
        Data::U32(v) => Json::Number(u64::from(*v).into()),
        Data::Float(v) | Data::F64(v) => serde_json::Number::from_f64(*v)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Data::F32(v) => serde_json::Number::from_f64(*v as f64)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Data::Bool(b) => Json::Bool(*b),
        Data::Null => Json::Null,
        Data::Version(v) => Json::String(v.to_string()),
        Data::Require(v) => Json::String(v.to_string()),
        Data::Array(items) => Json::Array(items.iter().map(value_json).collect()),
        Data::Table(table) => {
            let mut map = Map::new();
            for (k, v) in table {
                map.insert(k.clone(), value_json(v));
            }
            Json::Object(map)
        }
        Data::Bytes(_) | Data::Reference(_) | Data::Template(_) => Json::Null,
    }
}

/// Project a container statement's children onto a JSON object.
///
/// Assignments become fields; zero-label blocks become nested objects
/// (repeated same-id blocks become an array); labeled blocks are grouped by
/// their id into an object keyed by the first label, later definitions
/// winning. This matches the schema shapes of the serde-derived config
/// structs (nested tables, `Vec<...>` stages, labeled-block maps).
pub fn container_json(stmt: &Statement) -> Json {
    enum Slot {
        Scalar(Json),
        Labeled(Map<String, Json>),
        Repeated(Vec<Json>),
        Object(Map<String, Json>),
    }
    let mut slots: IndexMap<String, Slot> = IndexMap::new();

    for child in stmt.children() {
        match &child.data {
            StatementData::Single(value) => {
                slots.insert(child.id.clone(), Slot::Scalar(value_json(value)));
            }
            StatementData::Labeled(labels, _) if !labels.is_empty() => {
                let Some(label) = labels.first().and_then(|l| l.as_string().cloned()) else {
                    continue;
                };
                let json = container_json(child);
                if !matches!(slots.get(&child.id), Some(Slot::Labeled(_))) {
                    slots.insert(child.id.clone(), Slot::Labeled(Map::new()));
                }
                if let Some(Slot::Labeled(map)) = slots.get_mut(&child.id) {
                    map.insert(label, json);
                }
            }
            // Zero-label blocks and module children are plain objects.
            StatementData::Group(_) | StatementData::Labeled(_, _) => {
                let json = container_json(child);
                let slot = slots.shift_remove(&child.id);
                match slot {
                    Some(Slot::Repeated(mut list)) => {
                        list.push(json);
                        slots.insert(child.id.clone(), Slot::Repeated(list));
                    }
                    Some(Slot::Object(prev)) => {
                        slots.insert(
                            child.id.clone(),
                            Slot::Repeated(vec![Json::Object(prev), json]),
                        );
                    }
                    _ => {
                        let obj = match json {
                            Json::Object(o) => o,
                            _ => Map::new(),
                        };
                        slots.insert(child.id.clone(), Slot::Object(obj));
                    }
                }
            }
        }
    }

    let mut out = Map::new();
    for (id, slot) in slots {
        let json = match slot {
            Slot::Scalar(v) => v,
            Slot::Labeled(map) => Json::Object(map),
            Slot::Repeated(list) => Json::Array(list),
            Slot::Object(map) => Json::Object(map),
        };
        out.insert(id, json);
    }
    Json::Object(out)
}

// ---------------------------------------------------------------------------
// AST mutation for write-back
// ---------------------------------------------------------------------------

fn meta() -> Metadata {
    Metadata::default()
}

pub fn string_value(text: &str) -> BmlValue {
    BmlValue::new_string(text.to_string(), meta())
}

pub fn bool_value(value: bool) -> BmlValue {
    BmlValue::new_bool(value, meta())
}

pub fn string_array(values: &[String]) -> BmlValue {
    BmlValue::new_array(values.iter().map(|v| string_value(v)).collect(), meta())
}

pub fn new_assign(id: &str, value: BmlValue) -> Statement {
    Statement::new_assign(id, None, value, meta()).expect("untyped assignment cannot fail")
}

pub fn new_block(id: &str, label: Option<&str>) -> Statement {
    let labels = label.map(|l| vec![string_value(l)]).unwrap_or_default();
    Statement::new_block(id, labels, IndexMap::new(), meta())
}

/// The mutable child map of a container statement.
pub fn group_mut(stmt: &mut Statement) -> Option<&mut IndexMap<String, Statement>> {
    match &mut stmt.data {
        StatementData::Group(children) | StatementData::Labeled(_, children) => Some(children),
        StatementData::Single(_) => None,
    }
}

/// Find a child block by id and single label (mutable).
pub fn find_block_mut<'a>(
    stmt: &'a mut Statement,
    id: &str,
    label: Option<&str>,
) -> Option<&'a mut Statement> {
    let children = group_mut(stmt)?;
    let key = children
        .iter()
        .find(|(_, child)| {
            let (child_id, labels) = child.identity();
            child_id == id
                && match label {
                    Some(expected) => {
                        labels.len() == 1
                            && labels[0]
                                .as_string()
                                .map(|l| l == expected)
                                .unwrap_or(false)
                    }
                    None => labels.is_empty(),
                }
        })
        .map(|(key, _)| key.clone())?;
    children.get_mut(&key)
}

/// Insert or replace an assignment under `stmt` (appended last).
pub fn upsert_assign(stmt: &mut Statement, id: &str, value: BmlValue) {
    if let Some(children) = group_mut(stmt) {
        // Remove any existing entry with this id, whatever its map key.
        if let Some(key) = children
            .iter()
            .find(|(_, child)| child.id == id)
            .map(|(key, _)| key.clone())
        {
            children.shift_remove(&key);
        }
        children.insert(id.to_string(), new_assign(id, value));
    }
}

/// Get the existing `id = [...]` string array, or empty.
pub fn string_array_children(stmt: &Statement, id: &str) -> Vec<String> {
    stmt.find_child(id)
        .and_then(|c| c.get_value())
        .and_then(|v| v.as_array().cloned())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_string().cloned())
                .collect()
        })
        .unwrap_or_default()
}

/// Set `id = [values]`, replacing any existing assignment.
pub fn set_string_array(stmt: &mut Statement, id: &str, values: &[String]) {
    upsert_assign(stmt, id, string_array(values));
}

/// Append `value` to the string array `id`, keeping entries unique.
pub fn push_unique_string(stmt: &mut Statement, id: &str, value: &str) {
    let mut items = string_array_children(stmt, id);
    if !items.iter().any(|v| v == value) {
        items.push(value.to_string());
    }
    set_string_array(stmt, id, &items);
}

/// Find or create a labeled child block `id "label" {}` under `stmt`.
pub fn ensure_labeled_block<'a>(
    stmt: &'a mut Statement,
    id: &str,
    label: &str,
) -> &'a mut Statement {
    let existing = find_block_mut(stmt, id, Some(label)).is_some();
    if !existing {
        let block = new_block(id, Some(label));
        if let Some(children) = group_mut(stmt) {
            children.insert(block.storage_key(), block);
        }
    }
    find_block_mut(stmt, id, Some(label)).expect("just inserted")
}

/// Find or create a zero-label child block `id {}` under `stmt`.
pub fn ensure_block<'a>(stmt: &'a mut Statement, id: &str) -> Option<&'a mut Statement> {
    let exists = find_block_mut(stmt, id, None).is_some();
    if !exists {
        if group_mut(stmt)?.iter().any(|(_, child)| child.id == id) {
            // An assignment occupies the name; callers treat this as a
            // type error rather than replacing user data.
            return None;
        }
        let block = new_block(id, None);
        group_mut(stmt)?.insert(block.storage_key(), block);
    }
    find_block_mut(stmt, id, None)
}

/// Parse the file at `path` into a module statement; a missing file yields
/// a fresh empty module. Parse errors are returned to the caller.
pub fn parse_file_or_empty(path: &Path) -> Result<Statement, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Statement::new_module("main", IndexMap::new(), meta()));
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    if content.trim().is_empty() {
        return Ok(Statement::new_module("main", IndexMap::new(), meta()));
    }
    parse(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

/// Serialize a statement tree back to BarkML text.
///
/// Hand-written rather than `Display` because the derived `Display` emits
/// type annotations (`x: [] = [...]`) that the parser cannot re-read for
/// array and table values. Comment metadata is not preserved; write-back
/// regenerates the file.
pub fn to_text(doc: &Statement) -> String {
    let mut out = String::new();
    write_statement(&mut out, doc, 0, true);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn write_statement(out: &mut String, stmt: &Statement, indent: usize, is_root: bool) {
    let pad = "  ".repeat(indent);
    match &stmt.data {
        StatementData::Single(value) => {
            out.push_str(&format!("{pad}{} = {}\n", stmt.id, value));
        }
        _ => {
            let labels = stmt
                .get_labeled()
                .map(|(labels, _)| {
                    labels
                        .iter()
                        .map(|l| {
                            format!(
                                " \"{}\"",
                                l.as_string().map(String::as_str).unwrap_or_default()
                            )
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            if !is_root {
                out.push_str(&format!("{pad}{}{} {{\n", stmt.id, labels));
            }
            for child in stmt.children() {
                write_statement(out, child, if is_root { 0 } else { indent + 1 }, false);
            }
            if !is_root {
                out.push_str(&format!("{pad}}}\n"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use barkml::{Loader, StatementData};

    fn find_label(doc: &Statement, id: &str) -> Option<String> {
        doc.blocks()
            .find(|(b_id, _, _)| *b_id == id)
            .and_then(|(_, labels, _)| labels.first().and_then(|l| l.as_string().cloned()))
    }

    #[test]
    fn spike_repeated_and_labeled_blocks_project() {
        // Zero-label same-id blocks cannot repeat within one file, so
        // `compaction` stages are labeled blocks (label = kind).
        let doc = parse(
            r#"
            agent { temperature = 0.2 }
            provider "openrouter" { kind = "openai_compatible" }
            provider "ollama" { kind = "ollama" }
            compaction "vcc" { context = 0.6 }
            compaction "llm" { context = 0.8 }
            compaction_buffer = "20%"
            "#,
        )
        .unwrap();
        let stages: Vec<_> = doc
            .blocks()
            .filter(|(id, _, _)| *id == "compaction")
            .map(|(_, labels, block)| {
                let mut json = container_json(block);
                json.as_object_mut().unwrap().insert(
                    "kind".into(),
                    Json::String(labels[0].as_string().unwrap().clone()),
                );
                json
            })
            .collect();
        let json = container_json(&doc);
        let obj = json.as_object().unwrap();
        assert_eq!(obj["agent"]["temperature"], Json::from(0.2));
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0]["kind"], Json::String("vcc".into()));
        assert_eq!(stages[0]["context"], Json::from(0.6));
        assert_eq!(stages[1]["kind"], Json::String("llm".into()));
        assert_eq!(
            obj["provider"]["openrouter"]["kind"],
            Json::String("openai_compatible".into())
        );
        assert_eq!(obj["compaction_buffer"], Json::String("20%".into()));
    }

    #[test]
    fn spike_labeled_override_and_repeat_across_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("craft.bml"),
            "agent { temperature = 0.2 }\nprovider \"x\" { kind = \"openai\" }\ncompaction \"vcc\" { context = 0.5 }",
        )
        .unwrap();
        let sub = dir.path().join("split");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("extra.bml"),
            "agent { max_tokens = 512 }\nprovider \"x\" { base_url = \"https://llm.example.com\" }\ncompaction \"llm\" { context = 0.9 }",
        )
        .unwrap();

        let mut loader = barkml::StandardLoader::builder()
            .allow_collisions(true)
            .validate_on_load(true)
            .build();
        loader.add_file(dir.path().join("craft.bml")).unwrap();
        loader.add_dir(&sub).unwrap();
        let doc = finalize(loader.load().unwrap());

        let json = container_json(&doc);
        let obj = json.as_object().unwrap();
        // Deep merge: both agent fields apply.
        assert_eq!(obj["agent"]["temperature"], Json::from(0.2));
        assert_eq!(obj["agent"]["max_tokens"], Json::from(512));
        // Repeated labeled blocks accumulate across files.
        let stages = doc
            .blocks()
            .filter(|(id, _, _)| *id == "compaction")
            .count();
        assert_eq!(stages, 2);
        // Labeled block override merges fields (loader deep-merge).
        assert_eq!(obj["provider"]["x"]["kind"], Json::String("openai".into()));
        assert_eq!(
            obj["provider"]["x"]["base_url"],
            Json::String("https://llm.example.com".into())
        );
    }

    #[test]
    fn write_back_round_trips_through_display() {
        let mut doc = parse(
            "mcp \"srv\" { command = [\"echo\"]
  timeout = 5000 }\n",
        )
        .unwrap();
        let block = ensure_labeled_block(&mut doc, "mcp", "srv");
        upsert_assign(block, "enabled", bool_value(false));
        let text = to_text(&doc);
        let reparsed = parse(&text).unwrap();
        let block = reparsed.blocks().find(|(id, _, _)| *id == "mcp").unwrap().2;
        let enabled = block.find_child("enabled").unwrap().get_value().unwrap();
        assert_eq!(enabled.as_bool(), Some(&false));
        let command = block.find_child("command").unwrap().get_value().unwrap();
        assert_eq!(command.as_array().unwrap().len(), 1);
        assert!(matches!(reparsed.data, StatementData::Group(_)));
    }

    #[test]
    fn empty_module_serializes_and_parses() {
        let doc = Statement::new_module("main", IndexMap::new(), meta());
        let text = to_text(&doc);
        assert!(text.trim().is_empty() || parse(&text).is_ok(), "{text}");
    }

    #[test]
    fn labeled_blocks_expose_labels() {
        let doc = parse("provider \"a.b\" { kind = \"openai\" }").unwrap();
        assert_eq!(find_label(&doc, "provider").as_deref(), Some("a.b"));
    }
}
