use std::collections::{HashMap, HashSet};

use regex::Regex;

use super::super::normalize::NormalizedBlock;
use super::super::util::extract_path;

static FILE_WRITE_TOOLS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "Edit",
        "Write",
        "edit",
        "write",
        "edit_file",
        "write_file",
        "MultiEdit",
    ]
    .into_iter()
    .collect()
});
static FILE_READ_TOOLS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| ["Read", "read", "read_file", "View"].into_iter().collect());
static FILE_CREATE_TOOLS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| ["Write", "write", "write_file"].into_iter().collect());

use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(dead_code)]
pub(crate) enum SymbolKind {
    Function,
    Type,
    Class,
    Variable,
    Unknown,
}

#[derive(Debug, Clone)]
pub(crate) struct SymbolInfo {
    pub name: String,
    pub kind: SymbolKind,
    pub signature: Option<String>,
}

#[derive(Debug, Default)]
#[expect(dead_code)]
pub(crate) struct FileActivity {
    pub read: HashSet<String>,
    pub modified: HashSet<String>,
    pub created: HashSet<String>,
    pub symbols: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone)]
#[expect(dead_code)]
pub(crate) struct ExportSig {
    pub file: String,
    pub signatures: Vec<String>,
    pub modified: bool,
}

#[derive(Debug, Clone)]
#[expect(dead_code)]
pub(crate) struct SymbolRef {
    pub name: String,
    pub file: String,
    pub kind: SymbolKind,
    pub modified: bool,
}

#[derive(Debug, Default)]
pub(crate) struct UnifiedExtractResult {
    pub file_activity: FileActivity,
    pub type_catalog: Vec<ExportSig>,
    #[cfg_attr(not(test), expect(dead_code))]
    pub symbol_changes: Vec<SymbolRef>,
}

static DECL_SCREEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:export|pub|func|def|class|type|interface|async|abstract|static|public|private|protected|struct|enum|trait|impl|module|const|fn|sealed|record|typedef|union|virtual|extern|inline)").unwrap()
});

static TS_EXPORT_DECL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*export\s+(?:default\s+)?(?:async\s+)?(?:function|class|type|interface|const|let|enum)\s+(\w+)").unwrap()
});
static TS_TYPE_DECL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:export\s+)?(?:type|interface)\s+(\w+)").unwrap());
static TS_EXPORT_SIG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*export\s+(?:default\s+)?(?:async\s+)?(?:function|class|type|interface|const|let|enum)\s+\w+[^;{]*[;{]?").unwrap()
});

static RUST_DECL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:fn|struct|enum|trait|type|const|union|var)\s+(\w+)",
    )
    .unwrap()
});
static RUST_IMPL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?impl\s+(?:<[^>]+>\s+)?(\w+)(?:\s+for\s+(\w+))?")
        .unwrap()
});
static RUST_SIG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*pub\s+(?:async\s+)?(?:fn|struct|enum|trait|type)\s+\w+").unwrap()
});

static PY_DECL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:async\s+)?def\s+(\w+)|^\s*class\s+(\w+)").unwrap());
static PY_SIG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:async\s+)?(?:def|class)\s+\w+\s*(?:\([^)]*\))?").unwrap());

static GO_DECL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*func\s+(?:\(\w+\s+\*?\w+\)\s+)?(\w+)").unwrap());
static GO_SIG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*func\s+(?:\(\w+\s+\*?\w+\)\s+)?\w+\s*(?:\([^)]*\))?\s*(?:\([^)]*\))?").unwrap()
});

fn parse_decl_name(line: &str) -> Option<(String, SymbolKind)> {
    if !DECL_SCREEN_RE.is_match(line) {
        return None;
    }
    if let Some(c) = TS_EXPORT_DECL_RE.captures(line) {
        let kind = if line.contains("function") {
            SymbolKind::Function
        } else if line.contains("class") {
            SymbolKind::Class
        } else if line.contains("type") || line.contains("interface") {
            SymbolKind::Type
        } else {
            SymbolKind::Variable
        };
        return Some((c[1].to_string(), kind));
    }
    if let Some(c) = TS_TYPE_DECL_RE.captures(line) {
        return Some((c[1].to_string(), SymbolKind::Type));
    }
    if let Some(c) = RUST_DECL_RE.captures(line) {
        return Some((c[1].to_string(), SymbolKind::Function));
    }
    if RUST_IMPL_RE.is_match(line)
        && let Some(c) = RUST_IMPL_RE.captures(line)
    {
        return Some((c[1].to_string(), SymbolKind::Class));
    }
    if let Some(c) = PY_DECL_RE.captures(line) {
        return if c.get(2).is_some() {
            Some((c[2].to_string(), SymbolKind::Class))
        } else {
            Some((c[1].to_string(), SymbolKind::Function))
        };
    }
    if let Some(c) = GO_DECL_RE.captures(line)
        && c[1].chars().next().is_some_and(|ch| ch.is_uppercase())
    {
        return Some((c[1].to_string(), SymbolKind::Function));
    }
    None
}

fn parse_signature(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if TS_EXPORT_SIG_RE.is_match(trimmed) {
        return Some(trimmed.to_string());
    }
    if PY_SIG_RE.is_match(trimmed)
        && !trimmed.starts_with("def _")
        && !trimmed.starts_with("class _")
    {
        return Some(trimmed.to_string());
    }
    if GO_SIG_RE.is_match(trimmed)
        && let Some(c) = GO_DECL_RE.captures(trimmed)
        && c[1].chars().next().is_some_and(|ch| ch.is_uppercase())
    {
        return Some(trimmed.to_string());
    }
    if RUST_SIG_RE.is_match(trimmed) {
        return Some(trimmed.to_string());
    }
    None
}

fn extract_symbols_from_text(text: &str, max_lines: usize, include_sigs: bool) -> Vec<SymbolInfo> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for line in text.lines().take(max_lines) {
        if let Some((name, kind)) = parse_decl_name(line)
            && seen.insert(name.clone())
        {
            let sig = include_sigs.then(|| parse_signature(line)).flatten();
            names.push(SymbolInfo {
                name,
                kind,
                signature: sig,
            });
        }
    }
    names
}

fn find_result_text(blocks: &[NormalizedBlock], call_idx: usize) -> Option<(&str, bool)> {
    let end = (call_idx + 4).min(blocks.len());
    for blk in blocks.iter().take(end).skip(call_idx + 1) {
        if let NormalizedBlock::ToolResult { text, is_error, .. } = blk {
            return Some((text.as_str(), *is_error));
        }
    }
    None
}

const TYPE_CATALOG_MAX_FILES: usize = 12;
const TYPE_CATALOG_MAX_SIGS_PER_FILE: usize = 8;
const TYPE_CATALOG_MAX_TOTAL_SIGS: usize = 30;
const FILE_ACTIVITY_CAP: usize = 10;

pub(crate) fn extract_file_and_symbol_data(blocks: &[NormalizedBlock]) -> UnifiedExtractResult {
    let mut read: HashSet<String> = HashSet::new();
    let mut modified: HashSet<String> = HashSet::new();
    let mut created: HashSet<String> = HashSet::new();
    let mut symbols: HashMap<String, Vec<String>> = HashMap::new();
    let mut symbols_seen: HashMap<String, HashSet<String>> = HashMap::new();
    let mut symbol_refs: Vec<SymbolRef> = Vec::new();
    let mut ref_seen: HashSet<String> = HashSet::new();

    let mut file_sigs: HashMap<String, (Vec<String>, bool)> = HashMap::new();
    let mut file_order: Vec<String> = Vec::new();

    for (i, b) in blocks.iter().enumerate() {
        let NormalizedBlock::ToolCall { name, args, .. } = b else {
            continue;
        };
        let Some(p) = extract_path(args) else {
            continue;
        };
        let is_read = FILE_READ_TOOLS.contains(name.as_str());
        let is_write = FILE_WRITE_TOOLS.contains(name.as_str());
        let is_create = FILE_CREATE_TOOLS.contains(name.as_str());

        if is_read {
            read.insert(p.clone());
        }
        if is_write {
            modified.insert(p.clone());
        }
        if is_create {
            created.insert(p.clone());
        }

        if is_write {
            let new_text = args
                .get("newText")
                .or_else(|| args.get("new_text"))
                .or_else(|| args.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !new_text.is_empty() {
                let syms = extract_symbols_from_text(new_text, 100, true);
                add_symbols(&mut symbols, &mut symbols_seen, &p, &syms);
                let entry = file_sigs.entry(p.clone()).or_insert_with(|| {
                    file_order.push(p.clone());
                    (Vec::new(), true)
                });
                entry.1 = true;
                for s in &syms {
                    if let Some(sig) = &s.signature
                        && !entry.0.contains(sig)
                    {
                        entry.0.push(sig.clone());
                    }
                }
                for s in &syms {
                    let key = format!("{}@{}", s.name, p);
                    if ref_seen.insert(key) {
                        symbol_refs.push(SymbolRef {
                            name: s.name.clone(),
                            file: p.clone(),
                            kind: s.kind,
                            modified: true,
                        });
                    }
                }
            }
        }

        if (is_read || is_write)
            && let Some((result_text, is_error)) = find_result_text(blocks, i)
            && !is_error
            && !result_text.is_empty()
        {
            let syms = extract_symbols_from_text(result_text, 200, true);
            add_symbols(&mut symbols, &mut symbols_seen, &p, &syms);
            if is_read {
                file_sigs.entry(p.clone()).or_insert_with(|| {
                    file_order.push(p.clone());
                    (Vec::new(), false)
                });
                let entry = file_sigs.get_mut(&p).unwrap();
                for s in &syms {
                    if let Some(sig) = &s.signature
                        && !entry.0.contains(sig)
                    {
                        entry.0.push(sig.clone());
                    }
                }
            }
            for s in &syms {
                let key = format!("{}@{}", s.name, p);
                if ref_seen.insert(key) {
                    symbol_refs.push(SymbolRef {
                        name: s.name.clone(),
                        file: p.clone(),
                        kind: s.kind,
                        modified: is_write,
                    });
                }
            }
        }
    }

    for p in &modified {
        created.remove(p);
    }

    let mut modified_sigs: Vec<ExportSig> = Vec::new();
    let mut read_sigs: Vec<ExportSig> = Vec::new();
    for file in &file_order {
        let Some((sigs, modified)) = file_sigs.get(file) else {
            continue;
        };
        if sigs.is_empty() {
            continue;
        }
        let esig = ExportSig {
            file: file.clone(),
            signatures: sigs
                .iter()
                .take(TYPE_CATALOG_MAX_SIGS_PER_FILE)
                .cloned()
                .collect(),
            modified: *modified,
        };
        if *modified {
            modified_sigs.push(esig);
        } else {
            read_sigs.push(esig);
        }
    }
    let mut type_catalog: Vec<ExportSig> = modified_sigs;
    type_catalog.extend(read_sigs);
    type_catalog.truncate(TYPE_CATALOG_MAX_FILES);

    let mut total = 0;
    for entry in type_catalog.iter_mut() {
        let remaining = TYPE_CATALOG_MAX_TOTAL_SIGS.saturating_sub(total);
        if remaining == 0 {
            entry.signatures.clear();
            continue;
        }
        entry.signatures.truncate(remaining);
        total += entry.signatures.len();
    }
    type_catalog.retain(|e| !e.signatures.is_empty());

    UnifiedExtractResult {
        file_activity: FileActivity {
            read,
            modified,
            created,
            symbols,
        },
        type_catalog,
        symbol_changes: symbol_refs,
    }
}

fn add_symbols(
    symbols: &mut HashMap<String, Vec<String>>,
    seen: &mut HashMap<String, HashSet<String>>,
    path: &str,
    syms: &[SymbolInfo],
) {
    let set = seen.entry(path.to_string()).or_default();
    let vec = symbols.entry(path.to_string()).or_default();
    for s in syms {
        if set.insert(s.name.clone()) {
            vec.push(s.name.clone());
        }
    }
}

pub(crate) fn format_file_activity(data: &UnifiedExtractResult) -> Vec<String> {
    let act = &data.file_activity;
    let mut lines = Vec::new();
    let mod_line = format_category("Modified", &act.modified);
    if let Some(l) = mod_line {
        lines.push(l);
    }
    let create_line = format_category("Created", &act.created);
    if let Some(l) = create_line {
        lines.push(l);
    }
    let read_line = format_category("Read", &act.read);
    if let Some(l) = read_line {
        lines.push(l);
    }
    lines
}

fn format_category(label: &str, set: &HashSet<String>) -> Option<String> {
    if set.is_empty() {
        return None;
    }
    let mut arr: Vec<&String> = set.iter().collect();
    arr.sort();
    let kept: Vec<&str> = arr
        .iter()
        .take(FILE_ACTIVITY_CAP)
        .map(|s| s.as_str())
        .collect();
    if arr.len() > FILE_ACTIVITY_CAP {
        let omitted: Vec<&str> = arr[FILE_ACTIVITY_CAP..]
            .iter()
            .map(|s| s.as_str())
            .collect();
        Some(format!(
            "{}: {}, +recall: {}",
            label,
            kept.join(", "),
            omitted.join(", ")
        ))
    } else {
        Some(format!("{}: {}", label, kept.join(", ")))
    }
}

pub(crate) fn format_type_catalog(data: &UnifiedExtractResult) -> Vec<String> {
    let catalog = &data.type_catalog;
    if catalog.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut total_sigs = 0;
    let mut omitted_files = Vec::new();
    for entry in catalog {
        if total_sigs >= TYPE_CATALOG_MAX_TOTAL_SIGS {
            omitted_files.push(entry.file.clone());
            continue;
        }
        lines.push(format!("{}:", entry.file));
        for sig in &entry.signatures {
            if total_sigs >= TYPE_CATALOG_MAX_TOTAL_SIGS {
                break;
            }
            lines.push(format!("  {sig}"));
            total_sigs += 1;
        }
    }
    if !omitted_files.is_empty() {
        lines.push(format!(
            "({} more files with signatures omitted)",
            omitted_files.len()
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rust_fn_decl() {
        let (name, kind) = parse_decl_name("pub fn compact(&self) -> String {").unwrap();
        assert_eq!(name, "compact");
        assert_eq!(kind, SymbolKind::Function);
    }

    #[test]
    fn parse_python_class_decl() {
        let (name, kind) = parse_decl_name("class Foo(Bar):").unwrap();
        assert_eq!(name, "Foo");
        assert_eq!(kind, SymbolKind::Class);
    }

    #[test]
    fn extract_collects_read_and_modified() {
        let blocks = vec![
            NormalizedBlock::ToolCall {
                name: "Read".into(),
                args: serde_json::json!({"file_path": "src/a.rs"}),
                source_index: 0,
            },
            NormalizedBlock::ToolResult {
                name: "Read".into(),
                text: "pub fn alpha() {}".into(),
                is_error: false,
                source_index: 1,
            },
            NormalizedBlock::ToolCall {
                name: "Edit".into(),
                args: serde_json::json!({"file_path": "src/b.rs", "newText": "pub fn beta() {}"}),
                source_index: 2,
            },
            NormalizedBlock::ToolResult {
                name: "Edit".into(),
                text: "old".into(),
                is_error: false,
                source_index: 3,
            },
        ];
        let data = extract_file_and_symbol_data(&blocks);
        assert!(data.file_activity.read.contains("src/a.rs"));
        assert!(data.file_activity.modified.contains("src/b.rs"));
        assert!(data.symbol_changes.iter().any(|s| s.name == "beta"));
    }
}
