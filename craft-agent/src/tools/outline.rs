use std::borrow::Cow;
use std::path::Path;
use std::sync::LazyLock;

use crate::ToolOutput;
use craft_tool_macro::Tool;
use serde::Deserialize;
use tree_sitter::{Query, StreamingIterator};

use super::relative_path;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Outline {
    #[param(
        description = "Absolute path to a file or directory",
        alias = "file_path"
    )]
    path: String,
    #[param(
        description = "When path is a directory, return a flat file table instead of nested symbols"
    )]
    files: Option<bool>,
}

impl Outline {
    pub const NAME: &str = "outline";
    pub const DESCRIPTION: &str = include_str!("outline.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"path": "/project/src/main.rs"},
  {"path": "/project/src/", "files": true}
]"#,
    );

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = super::resolve_path(&self.path)?;
        let p = Path::new(&path);

        if p.is_dir() {
            return self.outline_dir(path, self.files.unwrap_or(false)).await;
        }

        if !p.is_file() {
            return Err(format!("path does not exist: {}", relative_path(&path)));
        }

        let content = ctx.fs.read_text_file(p).await?;
        ctx.file_tracker.record_read(p);

        let lang = LangId::from_path(p);
        let Some(lang) = lang else {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("file");
            return Ok(ToolOutput::Plain(format!("{name}: unsupported language")));
        };

        let symbols = extract_symbols(&content, lang);
        let tree = build_outline_tree(&symbols);
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let text = render_file_outline(name, &tree, lang);

        Ok(ToolOutput::Plain(text))
    }

    async fn outline_dir(&self, path: String, files_mode: bool) -> Result<ToolOutput, String> {
        tokio::task::spawn_blocking(move || outline_dir_blocking(&path, files_mode))
            .await
            .map_err(|e| format!("outline walk failed: {e}"))?
    }

    pub fn start_header(&self) -> String {
        relative_path(&self.path)
    }
}

fn outline_dir_blocking(path: &str, files_mode: bool) -> Result<ToolOutput, String> {
    let mut entries: Vec<DirEntry> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut total_bytes: usize = 0;

    for entry in walk_source_files(path) {
        let p = Path::new(&entry);
        let content = match std::fs::read_to_string(p) {
            Ok(c) => c,
            Err(_) => {
                skipped.push(relative_path(&entry));
                continue;
            }
        };
        total_bytes += content.len();

        let lang = LangId::from_path(p);
        let Some(lang) = lang else {
            if content.len() > MAX_FILE_BYTES {
                skipped.push(format!("{} (too large)", relative_path(&entry)));
            } else {
                skipped.push(format!("{} (unsupported)", relative_path(&entry)));
            }
            continue;
        };

        if content.len() > MAX_FILE_BYTES {
            skipped.push(format!("{} (too large)", relative_path(&entry)));
            continue;
        }

        let symbols = extract_symbols(&content, lang);
        let tree = build_outline_tree(&symbols);
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        entries.push(DirEntry {
            rel_path: relative_path(&entry),
            name: name.to_string(),
            lang,
            symbol_count: count_leaves(&tree),
            bytes: content.len(),
            tree,
        });
    }

    entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

    let text = if files_mode {
        render_files_table(&entries, &skipped)
    } else {
        render_dir_outline(&entries, &skipped, total_bytes)
    };

    Ok(ToolOutput::Plain(text))
}

const MAX_FILE_BYTES: usize = 1_000_000;
const MAX_OUTPUT_BYTES: usize = 30_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LangId {
    Rust,
    TypeScript,
    Python,
    Go,
    Java,
    C,
    Cpp,
    Ruby,
    Lua,
    Bash,
    Kotlin,
    Swift,
    CSharp,
    Elixir,
    Scala,
    Php,
    Html,
    Gleam,
    Dart,
    Starlark,
    Nix,
    Zig,
    Markdown,
    Css,
    Fish,
    Gdscript,
    Gdshader,
    GodotResource,
    ObjC,
    Perl,
    SvelteNext,
    Zsh,
}

impl LangId {
    pub fn from_path(p: &Path) -> Option<Self> {
        let ext = p.extension().and_then(|e| e.to_str())?;
        Self::from_extension(ext)
    }

    fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "ts" | "tsx" => Some(Self::TypeScript),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::TypeScript),
            "py" | "pyi" => Some(Self::Python),
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            "c" | "h" => Some(Self::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(Self::Cpp),
            "rb" => Some(Self::Ruby),
            "lua" => Some(Self::Lua),
            "sh" | "bash" => Some(Self::Bash),
            "kt" | "kts" => Some(Self::Kotlin),
            "swift" => Some(Self::Swift),
            "cs" => Some(Self::CSharp),
            "ex" | "exs" => Some(Self::Elixir),
            "scala" => Some(Self::Scala),
            "php" => Some(Self::Php),
            "html" | "htm" => Some(Self::Html),
            "gleam" => Some(Self::Gleam),
            "dart" => Some(Self::Dart),
            "bzl" | "bazel" | "build" => Some(Self::Starlark),
            "nix" => Some(Self::Nix),
            "zig" => Some(Self::Zig),
            "md" | "mdx" => Some(Self::Markdown),
            "css" => Some(Self::Css),
            "fish" => Some(Self::Fish),
            "gd" => Some(Self::Gdscript),
            "gdshader" => Some(Self::Gdshader),
            "tscn" | "tres" => Some(Self::GodotResource),
            "objc" => Some(Self::ObjC),
            "perl" => Some(Self::Perl),
            "svelte-next" => Some(Self::SvelteNext),
            "zsh" => Some(Self::Zsh),
            _ => None,
        }
    }

    pub fn ts_language(&self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Lua => tree_sitter_lua::LANGUAGE.into(),
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Self::Swift => tree_sitter_swift::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Elixir => tree_sitter_elixir::LANGUAGE.into(),
            Self::Scala => tree_sitter_scala::LANGUAGE.into(),
            Self::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Self::Html => tree_sitter_html::LANGUAGE.into(),
            Self::Gleam => tree_sitter_gleam::LANGUAGE.into(),
            Self::Dart => tree_sitter_dart::LANGUAGE.into(),
            Self::Starlark => tree_sitter_starlark::LANGUAGE.into(),
            Self::Nix => tree_sitter_nix::LANGUAGE.into(),
            Self::Zig => tree_sitter_zig::LANGUAGE.into(),
            Self::Markdown => tree_sitter_md::LANGUAGE.into(),
            Self::Css => tree_sitter_css::LANGUAGE.into(),
            Self::Fish => tree_sitter_fish::language(),
            Self::Gdscript => tree_sitter_gdscript::LANGUAGE.into(),
            Self::Gdshader => tree_sitter_gdshader::LANGUAGE.into(),
            Self::GodotResource => tree_sitter_godot_resource::LANGUAGE.into(),
            Self::ObjC => tree_sitter_objc::LANGUAGE.into(),
            Self::Perl => tree_sitter_perl::LANGUAGE.into(),
            Self::SvelteNext => tree_sitter_svelte_next::LANGUAGE.into(),
            Self::Zsh => tree_sitter_zsh::LANGUAGE.into(),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Java => "java",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Ruby => "ruby",
            Self::Lua => "lua",
            Self::Bash => "bash",
            Self::Kotlin => "kotlin",
            Self::Swift => "swift",
            Self::CSharp => "csharp",
            Self::Elixir => "elixir",
            Self::Scala => "scala",
            Self::Php => "php",
            Self::Html => "html",
            Self::Gleam => "gleam",
            Self::Dart => "dart",
            Self::Starlark => "starlark",
            Self::Nix => "nix",
            Self::Zig => "zig",
            Self::Markdown => "markdown",
            Self::Css => "css",
            Self::Fish => "fish",
            Self::Gdscript => "gd",
            Self::Gdshader => "gdshader",
            Self::GodotResource => "godot-resource",
            Self::ObjC => "objc",
            Self::Perl => "perl",
            Self::SvelteNext => "svelte",
            Self::Zsh => "zsh",
        }
    }

    fn import_separator(&self) -> &'static str {
        match self {
            Self::Rust => "::",
            Self::Python => ".",
            Self::Go => ".",
            Self::Ruby => "::",
            Self::Php => "\\",
            Self::CSharp => ".",
            Self::Dart => ".",
            Self::Starlark => ".",
            Self::Nix => ".",
            Self::Zig => ".",
            Self::Markdown => ".",
            Self::Css => ".",
            Self::Fish => ".",
            Self::Gdscript => ".",
            Self::Gdshader => ".",
            Self::GodotResource => ".",
            Self::ObjC => ".",
            Self::Perl => ".",
            Self::SvelteNext => ".",
            Self::Zsh => ".",
            _ => "/",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    TypeAlias,
    Constant,
    Module,
    Impl,
    Macro,
    Class,
    Interface,
    Variable,
    Heading,
    Import,
}

impl SymbolKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Function => "fn",
            Self::Method => "me",
            Self::Struct => "st",
            Self::Enum => "en",
            Self::Trait => "tr",
            Self::TypeAlias => "ta",
            Self::Constant => "co",
            Self::Module => "mo",
            Self::Impl => "im",
            Self::Macro => "ma",
            Self::Class => "cl",
            Self::Interface => "if",
            Self::Variable => "va",
            Self::Heading => "hd",
            Self::Import => "im",
        }
    }
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct Range {
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub range: Range,
    pub signature: Option<String>,
    pub scope_chain: Vec<String>,
    pub exported: bool,
    pub import_segments: Vec<Vec<String>>,
    pub is_child: bool,
}

#[derive(Debug, Clone)]
struct OutlineEntry {
    name: String,
    kind: SymbolKind,
    range: Range,
    signature: Option<String>,
    exported: bool,
    members: Vec<OutlineEntry>,
    import_segments: Vec<Vec<String>>,
}

struct DirEntry {
    rel_path: String,
    #[allow(dead_code)]
    name: String,
    lang: LangId,
    #[allow(dead_code)]
    symbol_count: usize,
    #[allow(dead_code)]
    bytes: usize,
    tree: Vec<OutlineEntry>,
}

pub fn extract_symbols(content: &str, lang: LangId) -> Vec<Symbol> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.ts_language())
        .expect("language grammar error");

    let tree = parser.parse(content, None);
    let Some(tree) = tree else {
        return vec![];
    };

    let query = match lang_query(lang) {
        Some(q) => q,
        None => return vec![],
    };

    let root = tree.root_node();
    let mut cursor = tree_sitter::QueryCursor::new();
    cursor.set_match_limit(65536);
    let mut matches = cursor.matches(query, root, content.as_bytes());

    let mut symbols = Vec::new();
    let mut seen_ranges = std::collections::HashSet::new();
    let import_sep = lang.import_separator();

    while let Some(m) = matches.next() {
        let mut name = String::new();
        let mut def_node: Option<tree_sitter::Node> = None;
        let mut kind = SymbolKind::Function;
        let mut is_child = false;

        for c in m.captures {
            let idx = c.index;
            let node = c.node;

            if is_name_capture(idx, query) {
                name = content[node.byte_range()].to_string();
            }

            if is_def_capture(idx, query) {
                def_node = Some(node);
                kind = def_capture_to_kind(idx, query);
                is_child = is_child_capture(idx, query);
            }
        }

        let Some(def_node) = def_node else { continue };
        if name.is_empty() {
            name = content[def_node.byte_range()]
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
        }

        let start = def_node.start_position();
        let end = def_node.end_position();
        let key = (start.row, start.column, end.row, end.column);
        if !seen_ranges.insert(key) {
            continue;
        }

        let sig = content[def_node.byte_range()].to_string();
        let exported = is_exported(def_node, lang, content.as_bytes());
        let scope_chain = build_scope_chain(def_node, content.as_bytes());

        let import_segments = if kind == SymbolKind::Import {
            parse_import_segments(&sig, import_sep)
        } else {
            Vec::new()
        };

        symbols.push(Symbol {
            name,
            kind,
            range: Range {
                start_row: start.row,
                start_col: start.column,
                end_row: end.row,
                end_col: end.column,
            },
            signature: Some(sig),
            scope_chain,
            exported,
            import_segments,
            is_child,
        });
    }

    symbols.sort_by_key(|s| (s.range.start_row, s.range.start_col));

    if matches!(lang, LangId::Rust | LangId::Python) {
        for sym in &mut symbols {
            if sym.kind == SymbolKind::Function && sym.scope_chain.iter().any(|s| !s.is_empty()) {
                sym.kind = SymbolKind::Method;
            }
        }
    }

    symbols
}

fn is_name_capture(idx: u32, query: &Query) -> bool {
    const NAMES: &[&str] = &[
        "fn.name",
        "method.name",
        "struct.name",
        "enum.name",
        "trait.name",
        "type.name",
        "const.name",
        "mod.name",
        "impl.name",
        "macro.name",
        "class.name",
        "iface.name",
        "var.name",
        "heading.name",
        "import.name",
        "field.name",
        "variant.name",
    ];
    NAMES
        .iter()
        .any(|n| query.capture_index_for_name(n).unwrap_or(u32::MAX) == idx)
}

fn is_def_capture(idx: u32, query: &Query) -> bool {
    const DEFS: &[&str] = &[
        "fn.def",
        "method.def",
        "struct.def",
        "enum.def",
        "trait.def",
        "type.def",
        "const.def",
        "mod.def",
        "impl.def",
        "macro.def",
        "class.def",
        "iface.def",
        "var.def",
        "heading.def",
        "import.def",
        "field.def",
        "variant.def",
    ];
    DEFS.iter()
        .any(|n| query.capture_index_for_name(n).unwrap_or(u32::MAX) == idx)
}

fn def_capture_to_kind(idx: u32, query: &Query) -> SymbolKind {
    const PAIRS: &[(&str, SymbolKind)] = &[
        ("fn.def", SymbolKind::Function),
        ("method.def", SymbolKind::Method),
        ("struct.def", SymbolKind::Struct),
        ("enum.def", SymbolKind::Enum),
        ("trait.def", SymbolKind::Trait),
        ("type.def", SymbolKind::TypeAlias),
        ("const.def", SymbolKind::Constant),
        ("mod.def", SymbolKind::Module),
        ("impl.def", SymbolKind::Impl),
        ("macro.def", SymbolKind::Macro),
        ("class.def", SymbolKind::Class),
        ("iface.def", SymbolKind::Interface),
        ("var.def", SymbolKind::Variable),
        ("heading.def", SymbolKind::Heading),
        ("import.def", SymbolKind::Import),
        ("field.def", SymbolKind::Variable),
        ("variant.def", SymbolKind::Variable),
    ];
    PAIRS
        .iter()
        .find(|(n, _)| query.capture_index_for_name(n).unwrap_or(u32::MAX) == idx)
        .map(|(_, k)| *k)
        .unwrap_or(SymbolKind::Variable)
}

fn is_child_capture(idx: u32, query: &Query) -> bool {
    const CHILD_DEFS: &[&str] = &["field.def", "variant.def"];
    CHILD_DEFS
        .iter()
        .any(|n| query.capture_index_for_name(n).unwrap_or(u32::MAX) == idx)
}

fn is_exported(node: tree_sitter::Node, lang: LangId, source: &[u8]) -> bool {
    match lang {
        LangId::Rust => {
            if node.child_by_field_name("visibility").is_some() {
                return true;
            }
            if let Some(sibling) = node.prev_named_sibling()
                && sibling.kind() == "visibility_modifier"
            {
                return true;
            }
            false
        }
        LangId::Go => {
            if let Some(name_node) = node.child_by_field_name("name") {
                name_node
                    .utf8_text(source)
                    .is_ok_and(|s| s.chars().next().is_some_and(|c| c.is_uppercase()))
            } else {
                false
            }
        }
        _ => false,
    }
}

fn build_scope_chain(node: tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        match parent.kind() {
            "source_file" | "declaration_list" => {}
            "impl_item" => {
                if let Some(type_node) = parent.child_by_field_name("type")
                    && let Ok(txt) = type_node.utf8_text(source)
                {
                    chain.push(txt.to_string());
                }
            }
            _ => {
                if let Some(name_node) = parent.child_by_field_name("name")
                    && let Ok(txt) = name_node.utf8_text(source)
                    && !txt.is_empty()
                {
                    chain.push(txt.to_string());
                }
            }
        }
        current = parent.parent();
    }

    chain.reverse();
    chain
}

fn parse_import_segments(sig: &str, sep: &str) -> Vec<Vec<String>> {
    let cleaned = sig
        .trim()
        .trim_end_matches(';')
        .trim_start_matches("use ")
        .trim_start_matches("pub use ")
        .trim_start_matches("import ")
        .trim_start_matches("from ");
    expand_import_paths(cleaned, sep)
}

fn expand_import_paths(text: &str, sep: &str) -> Vec<Vec<String>> {
    let mut results = Vec::new();
    let mut stack: Vec<(Vec<String>, &str)> = vec![(Vec::new(), text.trim())];

    while let Some((prefix, remaining)) = stack.pop() {
        let remaining = remaining.trim();
        if remaining.is_empty() {
            if !prefix.is_empty() {
                results.push(prefix);
            }
            continue;
        }

        if let Some(pos) = find_sep_top_level(remaining, sep) {
            let segment = remaining[..pos].trim();
            let rest = remaining[pos + sep.len()..].trim();
            let mut new_prefix = prefix.clone();
            new_prefix.push(segment.to_string());

            if let Some(inner) = strip_braces(rest) {
                for item in split_top_level(inner, ',').into_iter().rev() {
                    let cp = new_prefix.clone();
                    stack.push((cp, item));
                }
            } else {
                stack.push((new_prefix, rest));
            }
        } else {
            let mut path = prefix;
            path.push(remaining.to_string());
            results.push(path);
        }
    }

    results
}

fn find_sep_top_level(text: &str, sep: &str) -> Option<usize> {
    let mut depth = 0usize;
    let sep_bytes = sep.as_bytes();
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        match bytes[i] {
            b'{' | b'(' => depth += 1,
            b'}' | b')' => {
                depth = depth.saturating_sub(1);
            }
            _ if depth == 0
                && i + sep_bytes.len() <= bytes.len()
                && &bytes[i..i + sep_bytes.len()] == sep_bytes =>
            {
                return Some(i);
            }
            _ => {}
        }
    }
    None
}

fn strip_braces(text: &str) -> Option<&str> {
    let t = text.trim();
    if t.starts_with('{') && t.ends_with('}') {
        Some(&t[1..t.len() - 1])
    } else {
        None
    }
}

fn split_top_level(text: &str, delim: char) -> Vec<&str> {
    let mut depth = 0usize;
    let mut start = 0;
    let mut results = Vec::new();
    for (i, c) in text.char_indices() {
        match c {
            '{' | '(' => depth += 1,
            '}' | ')' => {
                depth = depth.saturating_sub(1);
            }
            _ if c == delim && depth == 0 => {
                results.push(text[start..i].trim());
                start = i + delim.len_utf8();
            }
            _ => {}
        }
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        results.push(last);
    }
    results
}

struct TrieNode {
    children: std::collections::BTreeMap<String, TrieNode>,
    is_leaf: bool,
}

impl TrieNode {
    fn new() -> Self {
        Self {
            children: std::collections::BTreeMap::new(),
            is_leaf: false,
        }
    }

    fn insert(&mut self, segments: &[String]) {
        let mut node = self;
        for seg in segments {
            node = node
                .children
                .entry(seg.clone())
                .or_insert_with(TrieNode::new);
        }
        node.is_leaf = true;
    }
}

fn render_trie(node: &TrieNode, sep: &str) -> Vec<String> {
    let mut result = Vec::new();
    for (seg, child) in &node.children {
        let rendered = render_trie(child, sep);
        if rendered.is_empty() {
            result.push(seg.clone());
        } else if child.is_leaf {
            result.push(format!("{seg}{sep}{}", rendered.join(", ")));
            result.push(seg.clone());
        } else if rendered.len() == 1 {
            result.push(format!("{seg}{sep}{}", rendered[0]));
        } else {
            result.push(format!("{seg}{sep}{{{}}}", rendered.join(", ")));
        }
    }
    result
}

fn build_outline_tree(symbols: &[Symbol]) -> Vec<OutlineEntry> {
    let mut root: Vec<OutlineEntry> = Vec::new();

    for sym in symbols {
        let entry = OutlineEntry {
            name: sym.name.clone(),
            kind: sym.kind,
            range: sym.range.clone(),
            signature: sym.signature.clone(),
            exported: sym.exported,
            members: vec![],
            import_segments: sym.import_segments.clone(),
        };

        if sym.is_child {
            attach_as_member(&mut root, sym);
        } else {
            insert_at_scope(&mut root, entry, &sym.scope_chain);
        }
    }

    root
}

fn attach_as_member(root: &mut [OutlineEntry], sym: &Symbol) {
    for entry in root.iter_mut().rev() {
        if range_contains(&entry.range, &sym.range)
            && matches!(
                entry.kind,
                SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Class | SymbolKind::Impl
            )
        {
            entry.members.push(OutlineEntry {
                name: sym.name.clone(),
                kind: sym.kind,
                range: sym.range.clone(),
                signature: None,
                exported: false,
                members: vec![],
                import_segments: vec![],
            });
            return;
        }
        if !entry.members.is_empty() {
            attach_as_member(&mut entry.members, sym);
            return;
        }
    }
}

fn range_contains(outer: &Range, inner: &Range) -> bool {
    (inner.start_row, inner.start_col) >= (outer.start_row, outer.start_col)
        && (inner.end_row, inner.end_col) <= (outer.end_row, outer.end_col)
}

fn insert_at_scope(entries: &mut Vec<OutlineEntry>, entry: OutlineEntry, scope: &[String]) {
    if scope.is_empty() {
        entries.push(entry);
        return;
    }

    let head = &scope[0];
    if let Some(parent) = entries.iter_mut().find(|e| e.name == *head) {
        insert_at_scope(&mut parent.members, entry, &scope[1..]);
    } else {
        entries.push(entry);
    }
}

fn count_leaves(entries: &[OutlineEntry]) -> usize {
    entries
        .iter()
        .map(|e| {
            if e.members.is_empty() {
                1
            } else {
                count_leaves(&e.members)
            }
        })
        .sum()
}

fn render_file_outline(filename: &str, entries: &[OutlineEntry], lang: LangId) -> String {
    let mut out = String::new();
    out.push_str(filename);
    out.push('\n');

    let imports: Vec<&OutlineEntry> = entries
        .iter()
        .filter(|e| e.kind == SymbolKind::Import)
        .collect();
    let non_imports: Vec<&OutlineEntry> = entries
        .iter()
        .filter(|e| e.kind != SymbolKind::Import)
        .collect();
    if !imports.is_empty() {
        let mut min_line = usize::MAX;
        let mut max_line = 0usize;
        for e in &imports {
            min_line = min_line.min(e.range.start_row);
            max_line = max_line.max(e.range.end_row);
        }
        let _ = std::fmt::write(
            &mut out,
            format_args!("  imports: [{}-{}]\n", min_line + 1, max_line + 1),
        );
        let mut trie = TrieNode::new();
        for e in &imports {
            for path in &e.import_segments {
                trie.insert(path);
            }
        }
        let sep = lang.import_separator();
        for line in render_trie(&trie, sep) {
            let _ = std::fmt::write(&mut out, format_args!("    {line}\n"));
        }
        out.push('\n');
    }

    render_entries(&non_imports, 1, &mut out);
    truncate_outline(&mut out)
}

fn render_entries(entries: &[&OutlineEntry], depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    for entry in entries {
        if entry.kind == SymbolKind::Import {
            continue;
        }
        let exp = if entry.exported { "E" } else { " " };
        let kind = entry.kind.label();
        let sig = entry
            .signature
            .as_deref()
            .map(truncate_signature)
            .unwrap_or_else(|| entry.name.clone());

        let _ = std::fmt::write(
            out,
            format_args!(
                "{indent}{exp} {kind:2} {sig} {}:{}\n",
                entry.range.start_row + 1,
                entry.range.start_col + 1,
            ),
        );

        if !entry.members.is_empty() {
            render_members(&entry.members, depth + 1, out);
        }
    }
}

const MAX_INLINE_MEMBERS: usize = 8;

fn render_entries_owned(entries: &[OutlineEntry], depth: usize, out: &mut String) {
    let refs: Vec<&OutlineEntry> = entries.iter().collect();
    render_entries(&refs, depth, out);
}

fn render_members(members: &[OutlineEntry], depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    let total = members.len();
    let show = total.min(MAX_INLINE_MEMBERS);
    let mut names: Vec<Cow<'_, str>> = members[..show]
        .iter()
        .map(|m| Cow::Borrowed(m.name.as_str()))
        .collect();
    if total > MAX_INLINE_MEMBERS {
        names.push(Cow::Owned(format!("[{} more]", total - MAX_INLINE_MEMBERS)));
    }
    let mut line = indent.clone();
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            line.push_str(", ");
        }
        line.push_str(name);
        if line.len() > 100 {
            let _ = std::fmt::write(out, format_args!("{line}\n"));
            line = indent.clone();
            line.push_str(name);
        }
    }
    if !line.trim().is_empty() {
        let _ = std::fmt::write(out, format_args!("{line}\n"));
    }
}

fn truncate_signature(sig: &str) -> String {
    let first_line = sig.lines().next().unwrap_or(sig);
    if first_line.len() > 80 {
        let boundary = first_line.floor_char_boundary(79);
        format!("{}…", &first_line[..boundary])
    } else {
        first_line.to_string()
    }
}

fn render_dir_outline(entries: &[DirEntry], skipped: &[String], total_bytes: usize) -> String {
    let mut out = String::new();
    for e in entries {
        out.push_str(&e.rel_path);
        out.push('\n');
        render_entries_owned(&e.tree, 1, &mut out);
        out.push('\n');
    }
    if !skipped.is_empty() {
        out.push_str("skipped:\n");
        for s in skipped {
            let _ = std::fmt::write(&mut out, format_args!("  {s}\n"));
        }
    }
    let _ = std::fmt::write(
        &mut out,
        format_args!("total: {} files, {} bytes\n", entries.len(), total_bytes),
    );
    truncate_outline(&mut out)
}

fn render_files_table(entries: &[DirEntry], skipped: &[String]) -> String {
    let mut out = String::new();
    out.push_str("path                                   lang        symbols  bytes\n");
    out.push_str("─────────────────────────────────────────────────────────────────────\n");
    for e in entries {
        let _ = std::fmt::write(
            &mut out,
            format_args!(
                "{:<40} {:<12} {:>7}  {:>6}\n",
                e.rel_path,
                e.lang.name(),
                e.symbol_count,
                e.bytes,
            ),
        );
    }
    if !skipped.is_empty() {
        out.push('\n');
        out.push_str("skipped:\n");
        for s in skipped {
            let _ = std::fmt::write(&mut out, format_args!("  {s}\n"));
        }
    }
    truncate_outline(&mut out)
}

fn truncate_outline(out: &mut String) -> String {
    if out.len() > MAX_OUTPUT_BYTES {
        let truncation_hint = "\n… (output truncated, narrow the path to see more)";
        out.truncate(MAX_OUTPUT_BYTES - truncation_hint.len());
        out.push_str(truncation_hint);
    }
    std::mem::take(out)
}

fn walk_source_files(dir: &str) -> Vec<String> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    for entry in walker.flatten() {
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            files.push(entry.path().to_string_lossy().into_owned());
        }
    }
    files
}

fn lang_query(lang: LangId) -> Option<&'static Query> {
    match lang {
        LangId::Rust => Some(&RUST_QUERY),
        LangId::TypeScript => Some(&TS_QUERY),
        LangId::Python => Some(&PY_QUERY),
        LangId::Go => Some(&GO_QUERY),
        LangId::Java => Some(&JAVA_QUERY),
        LangId::C => Some(&C_QUERY),
        LangId::Cpp => Some(&CPP_QUERY),
        LangId::Ruby => Some(&RUBY_QUERY),
        LangId::Lua => Some(&LUA_QUERY),
        LangId::Bash => Some(&BASH_QUERY),
        LangId::Kotlin => Some(&KT_QUERY),
        LangId::Swift => Some(&SWIFT_QUERY),
        LangId::CSharp => Some(&CSHARP_QUERY),
        LangId::Elixir => Some(&ELIXIR_QUERY),
        LangId::Scala => Some(&SCALA_QUERY),
        LangId::Php => Some(&PHP_QUERY),
        LangId::Html => Some(&HTML_QUERY),
        LangId::Gleam => Some(&GLEAM_QUERY),
        LangId::Dart => Some(&DART_QUERY),
        LangId::Starlark => Some(&STARLARK_QUERY),
        LangId::Nix => Some(&NIX_QUERY),
        LangId::Zig => Some(&ZIG_QUERY),
        LangId::Markdown => Some(&MD_QUERY),
        LangId::Css => Some(&CSS_QUERY),
        LangId::Fish => Some(&FISH_QUERY),
        LangId::Gdscript => Some(&GDSCRIPT_QUERY),
        LangId::Gdshader => Some(&GDSHADER_QUERY),
        LangId::GodotResource => Some(&GODOT_RESOURCE_QUERY),
        LangId::ObjC => Some(&OBJC_QUERY),
        LangId::Perl => Some(&PERL_QUERY),
        LangId::SvelteNext => Some(&SVELTE_NEXT_QUERY),
        LangId::Zsh => Some(&ZSH_QUERY),
    }
}

static RUST_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_rust::LANGUAGE.into(),
        r#"
(function_item name: (identifier) @fn.name) @fn.def
(impl_item type: (type_identifier) @impl.name) @impl.def
(struct_item name: (type_identifier) @struct.name) @struct.def
(enum_item name: (type_identifier) @enum.name) @enum.def
(trait_item name: (type_identifier) @trait.name) @trait.def
(type_item name: (type_identifier) @type.name) @type.def
(const_item name: (identifier) @const.name) @const.def
(mod_item name: (identifier) @mod.name) @mod.def
(macro_definition name: (identifier) @macro.name) @macro.def
(use_declaration) @import.def
(field_declaration_list (field_declaration name: (field_identifier) @field.name)) @field.def
(enum_variant_list (enum_variant name: (identifier) @variant.name)) @variant.def
"#,
    )
    .expect("rust query")
});

static TS_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        r#"
(function_declaration name: (identifier) @fn.name) @fn.def
(method_definition name: (property_identifier) @method.name) @method.def
(class_declaration name: (type_identifier) @class.name) @class.def
(interface_declaration name: (type_identifier) @iface.name) @iface.def
(type_alias_declaration name: (type_identifier) @type.name) @type.def
(variable_declaration declarator: (variable_declarator name: (identifier) @var.name)) @var.def
(lexical_declaration declarator: (variable_declarator name: (identifier) @var.name)) @var.def
(import_statement) @import.def
(class_body (public_field_definition name: (property_identifier) @field.name)) @field.def
"#,
    )
    .expect("typescript query")
});

static PY_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_python::LANGUAGE.into(),
        r#"
(function_definition name: (identifier) @fn.name) @fn.def
(class_definition name: (identifier) @class.name) @class.def
(import_statement) @import.def
(import_from_statement) @import.def
"#,
    )
    .expect("python query")
});

static GO_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_go::LANGUAGE.into(),
        r#"
(function_declaration name: (field_identifier) @fn.name) @fn.def
(method_declaration name: (field_identifier) @method.name) @method.def
(type_declaration name: (type_identifier) @type.name) @type.def
(import_declaration) @import.def
"#,
    )
    .expect("go query")
});

static JAVA_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_java::LANGUAGE.into(),
        r#"
(class_declaration name: (identifier) @class.name) @class.def
(method_declaration name: (identifier) @method.name) @method.def
(interface_declaration name: (identifier) @iface.name) @iface.def
(import_declaration) @import.def
"#,
    )
    .expect("java query")
});

static C_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_c::LANGUAGE.into(),
        r#"
(function_definition declarator: (function_declarator declarator: (identifier) @fn.name)) @fn.def
"#,
    )
    .expect("c query")
});

static CPP_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_cpp::LANGUAGE.into(),
        r#"
(function_definition declarator: (function_declarator declarator: (identifier) @fn.name)) @fn.def
(class_specifier name: (type_identifier) @class.name) @class.def
"#,
    )
    .expect("cpp query")
});

static RUBY_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_ruby::LANGUAGE.into(),
        r#"
(method name: (identifier) @method.name) @method.def
(class name: (constant) @class.name) @class.def
(module name: (constant) @mod.name) @mod.def
(call method: (identifier) @_require arguments: (argument_list (string) @import.name) (#eq? @_require "require")) @import.def
"#,
    )
    .expect("ruby query")
});

static LUA_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_lua::LANGUAGE.into(),
        r#"
(function_declaration name: (identifier) @fn.name) @fn.def
"#,
    )
    .expect("lua query")
});

static BASH_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_bash::LANGUAGE.into(),
        r#"
(function_definition name: (word) @fn.name) @fn.def
"#,
    )
    .expect("bash query")
});

static KT_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_kotlin_ng::LANGUAGE.into(),
        r#"
(function_declaration (simple_identifier) @fn.name) @fn.def
(class_declaration (type_identifier) @class.name) @class.def
"#,
    )
    .expect("kotlin query")
});

static SWIFT_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_swift::LANGUAGE.into(),
        r#"
(function_declaration name: (identifier) @fn.name) @fn.def
"#,
    )
    .expect("swift query")
});

static CSHARP_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_c_sharp::LANGUAGE.into(),
        r#"
(class_declaration name: (identifier) @class.name) @class.def
(method_declaration name: (identifier) @method.name) @method.def
(struct_declaration name: (identifier) @struct.name) @struct.def
(interface_declaration name: (identifier) @iface.name) @iface.def
(enum_declaration name: (identifier) @enum.name) @enum.def
(using_directive) @import.def
"#,
    )
    .expect("csharp query")
});

static ELIXIR_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_elixir::LANGUAGE.into(),
        r#"
(call target: (identifier) @_def arguments: (arguments (alias) @fn.name) (#eq? @_def "def")) @fn.def
"#,
    )
    .expect("elixir query")
});

static SCALA_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_scala::LANGUAGE.into(),
        r#"
(function_definition name: (identifier) @fn.name) @fn.def
(class_definition name: (identifier) @class.name) @class.def
(object_definition name: (identifier) @mod.name) @mod.def
"#,
    )
    .expect("scala query")
});

static PHP_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_php::LANGUAGE_PHP.into(),
        r#"
(function_definition name: (name) @fn.name) @fn.def
(class_declaration name: (name) @class.name) @class.def
(method_declaration name: (name) @method.name) @method.def
(use_declaration) @import.def
"#,
    )
    .expect("php query")
});

static HTML_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_html::LANGUAGE.into(),
        r#"
(element (start_tag (tag_name) @heading.name)) @heading.def
"#,
    )
    .expect("html query")
});

static GLEAM_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_gleam::LANGUAGE.into(),
        r#"
(function_definition name: (identifier) @fn.name) @fn.def
"#,
    )
    .expect("gleam query")
});

static DART_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_dart::LANGUAGE.into(),
        r#"
(function_signature name: (identifier) @fn.name) @fn.def
(class_definition name: (identifier) @class.name) @class.def
"#,
    )
    .expect("dart query")
});

static STARLARK_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_starlark::LANGUAGE.into(),
        r#"
(function_definition name: (identifier) @fn.name) @fn.def
"#,
    )
    .expect("starlark query")
});

static NIX_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_nix::LANGUAGE.into(),
        r#"
(function_definition name: (identifier) @fn.name) @fn.def
"#,
    )
    .expect("nix query")
});

static ZIG_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_zig::LANGUAGE.into(),
        r#"
(function_declaration name: (identifier) @fn.name) @fn.def
"#,
    )
    .expect("zig query")
});

static MD_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_md::LANGUAGE.into(),
        r#"
(atx_heading (atx_h1_marker) (inline) @heading.name) @heading.def
(atx_heading (atx_h2_marker) (inline) @heading.name) @heading.def
(atx_heading (atx_h3_marker) (inline) @heading.name) @heading.def
"#,
    )
    .expect("markdown query")
});

static CSS_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_css::LANGUAGE.into(),
        r#"
(rule_set (selectors) @class.name) @class.def
"#,
    )
    .expect("css query")
});

static FISH_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_fish::language(),
        r#"
(function_definition name: (word) @fn.name) @fn.def
"#,
    )
    .expect("fish query")
});

static GDSCRIPT_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_gdscript::LANGUAGE.into(),
        r#"
(class_definition name: (name) @class.name) @class.def
(function_definition name: (name) @fn.name) @fn.def
"#,
    )
    .expect("gdscript query")
});

static GDSHADER_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_gdshader::LANGUAGE.into(),
        r#"
(function_definition declarator: (identifier) @fn.name) @fn.def
(struct_definition name: (identifier) @struct.name) @struct.def
"#,
    )
    .expect("gdshader query")
});

static GODOT_RESOURCE_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_godot_resource::LANGUAGE.into(),
        r#"
(section (identifier) @class.name) @class.def
"#,
    )
    .expect("godot resource query")
});

static OBJC_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_objc::LANGUAGE.into(),
        r#"
(class_interface (identifier) @class.name) @class.def
(class_implementation (identifier) @class.name) @class.def
(protocol_declaration (identifier) @iface.name) @iface.def
(method_declaration) @method.def
(function_definition) @fn.def
"#,
    )
    .expect("objc query")
});

static PERL_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_perl::LANGUAGE.into(),
        r#"
(package_statement (package_name) @mod.name) @mod.def
(require_statement package_name: (package_name) @import.name) @import.def
"#,
    )
    .expect("perl query")
});

static SVELTE_NEXT_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_svelte_next::LANGUAGE.into(),
        r#"
(element (start_tag (tag_name) @heading.name)) @heading.def
"#,
    )
    .expect("svelte-next query")
});

static ZSH_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_zsh::LANGUAGE.into(),
        r#"
(function_definition name: (word) @fn.name) @fn.def
"#,
    )
    .expect("zsh query")
});

super::impl_tool!(Outline, kind = "outline", tier = super::ToolTier::Core);

impl super::ToolInvocation for Outline {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Outline::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Outline::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_SRC: &str = r#"
use std::fs;

pub struct Config {
    name: String,
}

impl Config {
    pub fn new() -> Self {
        Self { name: String::new() }
    }
}

fn main() {
    let config = Config::new();
}
"#;

    #[test]
    fn rust_outline_extracts_struct_and_fn() {
        let symbols = extract_symbols(RUST_SRC, LangId::Rust);
        assert!(
            symbols
                .iter()
                .any(|s| s.name == "Config" && s.kind == SymbolKind::Struct)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name == "new" && s.kind == SymbolKind::Method)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name == "main" && s.kind == SymbolKind::Function)
        );
    }

    #[test]
    fn rust_outline_builds_tree() {
        let symbols = extract_symbols(RUST_SRC, LangId::Rust);
        let tree = build_outline_tree(&symbols);
        assert!(
            tree.iter()
                .any(|e| e.name == "Config" && !e.members.is_empty())
        );
    }

    #[test]
    fn rust_outline_renders() {
        let symbols = extract_symbols(RUST_SRC, LangId::Rust);
        let tree = build_outline_tree(&symbols);
        let text = render_file_outline("main.rs", &tree, LangId::Rust);
        assert!(text.contains("main.rs"));
        assert!(text.contains("Config"));
        assert!(text.contains("main"));
    }

    #[test]
    fn lang_from_extension() {
        assert_eq!(LangId::from_extension("rs"), Some(LangId::Rust));
        assert_eq!(LangId::from_extension("py"), Some(LangId::Python));
        assert_eq!(LangId::from_extension("txt"), None);
    }

    #[test]
    fn truncate_signature_long() {
        let sig = "fn very_long_function_name(with: many, arguments: that, make: it, exceed: the, limit: of, eighty: characters) -> Result<Type, Error>";
        let truncated = truncate_signature(sig);
        assert!(truncated.chars().count() <= 81);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn python_outline_extracts_class_and_fn() {
        let src = "class Foo:\n    def bar(self):\n        pass\n\ndef baz():\n    pass\n";
        let symbols = extract_symbols(src, LangId::Python);
        assert!(
            symbols
                .iter()
                .any(|s| s.name == "Foo" && s.kind == SymbolKind::Class)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name == "bar" && s.kind == SymbolKind::Method)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name == "baz" && s.kind == SymbolKind::Function)
        );
    }
}
