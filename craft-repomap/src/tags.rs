use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use regex::Regex;
use tracing::warn;
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

const MAX_FILE_BYTES: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TagKind {
    Def,
    Ref,
}

#[derive(Debug, Clone)]
pub struct Tag {
    pub rel_path: String,
    pub ident: String,
    pub kind: TagKind,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct FileTags {
    pub rel_path: String,
    pub tags: Vec<Tag>,
    pub mtime: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    Css,
    Fish,
    Perl,
    Sql,
}

impl LangId {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => Some(Self::TypeScript),
            "py" | "pyi" => Some(Self::Python),
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            "c" | "h" => Some(Self::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "ixx" => Some(Self::Cpp),
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
            "css" => Some(Self::Css),
            "fish" => Some(Self::Fish),
            "perl" => Some(Self::Perl),
            "sql" => Some(Self::Sql),
            _ => None,
        }
    }

    pub fn ts_language(&self) -> Language {
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
            Self::Css => tree_sitter_css::LANGUAGE.into(),
            Self::Fish => tree_sitter_fish::language(),
            Self::Perl => tree_sitter_perl::LANGUAGE.into(),
            Self::Sql => tree_sitter_sequel::LANGUAGE.into(),
        }
    }
}

fn ident_regex() -> &'static Regex {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b[a-zA-Z_][a-zA-Z0-9_]{2,}\b").unwrap());
    &RE
}

pub fn extract_tags(content: &str, lang: LangId, rel_path: &str) -> Vec<Tag> {
    let mut parser = Parser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        warn!("repomap parser rejected language abi");
        return vec![];
    }
    let Some(tree) = parser.parse(content, None) else {
        return vec![];
    };
    let query = match tags_query(lang) {
        Some(q) => q,
        None => return vec![],
    };

    let root = tree.root_node();
    let mut cursor = QueryCursor::new();
    cursor.set_match_limit(65536);
    let mut matches = cursor.matches(query, root, content.as_bytes());

    let mut tags = Vec::new();
    let mut seen: std::collections::HashSet<(TagKind, usize, String)> =
        std::collections::HashSet::new();

    while let Some(m) = matches.next() {
        for cap in m.captures {
            let names = query.capture_names();
            let capture_name = names[cap.index as usize];
            let node = cap.node;
            let kind = match capture_name {
                n if n.starts_with("name.definition") => TagKind::Def,
                n if n.starts_with("name.reference") => TagKind::Ref,
                _ => continue,
            };

            let ident = content[node.byte_range()].trim().to_string();
            if ident.is_empty() {
                continue;
            }

            let line = node.start_position().row + 1;
            let key = (kind, line, ident.clone());
            if !seen.insert(key) {
                continue;
            }

            tags.push(Tag {
                rel_path: rel_path.to_string(),
                ident,
                kind,
                line,
            });
        }
    }

    let ref_tags: Vec<Tag> = tags
        .iter()
        .filter(|t| t.kind == TagKind::Ref)
        .map(|t| t.ident.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .map(|ident| Tag {
            rel_path: rel_path.to_string(),
            ident,
            kind: TagKind::Ref,
            line: 0,
        })
        .collect();

    let mut def_tags: Vec<Tag> = tags
        .into_iter()
        .filter(|t| t.kind == TagKind::Def)
        .collect();
    def_tags.extend(ref_tags);
    def_tags
}

fn walk_tracked_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .build();

    for entry in walker.flatten() {
        if entry.file_type().is_some_and(|ft| ft.is_file())
            && let Some(ext) = entry.path().extension().and_then(|e| e.to_str())
            && LangId::from_extension(ext).is_some()
        {
            files.push(entry.path().to_path_buf());
        }
    }
    files
}

pub fn collect_all_tags(root: &Path) -> Vec<FileTags> {
    let mut result = Vec::new();
    for path in walk_tracked_files(root) {
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().to_string(),
            Err(_) => continue,
        };
        let lang = match path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(LangId::from_extension)
        {
            Some(l) => l,
            None => continue,
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if content.len() > MAX_FILE_BYTES {
            continue;
        }
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let tags = extract_tags(&content, lang, &rel);
        if !tags.is_empty() {
            result.push(FileTags {
                rel_path: rel,
                tags,
                mtime,
            });
        }
    }
    result
}

pub fn extract_mentioned_idents(text: &str) -> Vec<String> {
    ident_regex()
        .find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect()
}

pub fn file_path_to_idents(rel_path: &str) -> Vec<String> {
    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mut idents = vec![stem.to_string()];
    let parts: Vec<&str> = stem
        .split(['_', '-', '.'])
        .filter(|s| !s.is_empty())
        .collect();
    for part in parts {
        if part != stem {
            idents.push(part.to_string());
        }
    }
    idents
}

fn tags_query(lang: LangId) -> Option<&'static Query> {
    lang_tags_query(lang).as_ref()
}

fn build_tags_query(lang_name: &str, language: &Language, src: &'static str) -> Option<Query> {
    match Query::new(language, src) {
        Ok(q) => Some(q),
        Err(e) => {
            warn!(error = %e, lang = lang_name, "repomap tags query failed to compile");
            None
        }
    }
}

fn lang_tags_query(lang: LangId) -> &'static LazyLock<Option<Query>> {
    match lang {
        LangId::Rust => &RUST_TAGS_QUERY,
        LangId::TypeScript => &TS_TAGS_QUERY,
        LangId::Python => &PY_TAGS_QUERY,
        LangId::Go => &GO_TAGS_QUERY,
        LangId::Java => &JAVA_TAGS_QUERY,
        LangId::C => &C_TAGS_QUERY,
        LangId::Cpp => &CPP_TAGS_QUERY,
        LangId::Ruby => &RUBY_TAGS_QUERY,
        LangId::Lua => &LUA_TAGS_QUERY,
        LangId::Bash => &BASH_TAGS_QUERY,
        LangId::Kotlin => &KT_TAGS_QUERY,
        LangId::Swift => &SWIFT_TAGS_QUERY,
        LangId::CSharp => &CSHARP_TAGS_QUERY,
        LangId::Elixir => &ELIXIR_TAGS_QUERY,
        LangId::Scala => &SCALA_TAGS_QUERY,
        LangId::Php => &PHP_TAGS_QUERY,
        LangId::Html => &HTML_TAGS_QUERY,
        LangId::Gleam => &GLEAM_TAGS_QUERY,
        LangId::Dart => &DART_TAGS_QUERY,
        LangId::Starlark => &STARLARK_TAGS_QUERY,
        LangId::Nix => &NIX_TAGS_QUERY,
        LangId::Zig => &ZIG_TAGS_QUERY,
        LangId::Css => &CSS_TAGS_QUERY,
        LangId::Fish => &FISH_TAGS_QUERY,
        LangId::Perl => &PERL_TAGS_QUERY,
        LangId::Sql => &SQL_TAGS_QUERY,
    }
}

const RUST_TAGS_SRC: &str = r#"
(function_item name: (identifier) @name.definition.function) @definition.function
(impl_item type: (type_identifier) @name.definition.class) @definition.class
(struct_item name: (type_identifier) @name.definition.class) @definition.class
(enum_item name: (type_identifier) @name.definition.class) @definition.class
(trait_item name: (type_identifier) @name.definition.class) @definition.class
(type_item name: (type_identifier) @name.definition.class) @definition.class
(const_item name: (identifier) @name.definition.constant) @definition.constant
(mod_item name: (identifier) @name.definition.module) @definition.module
(macro_definition name: (identifier) @name.definition.macro) @definition.macro
(call_expression function: (identifier) @name.reference)
(call_expression function: (field_expression field: (field_identifier) @name.reference))
(use_declaration argument: (scoped_identifier name: (identifier) @name.reference))
(use_declaration argument: (identifier) @name.reference)
"#;
static RUST_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("rust", &tree_sitter_rust::LANGUAGE.into(), RUST_TAGS_SRC));

const TS_TAGS_SRC: &str = r#"
(function_declaration name: (identifier) @name.definition.function) @definition.function
(method_definition name: (property_identifier) @name.definition.function) @definition.function
(class_declaration name: (type_identifier) @name.definition.class) @definition.class
(interface_declaration name: (type_identifier) @name.definition.class) @definition.class
(type_alias_declaration name: (type_identifier) @name.definition.class) @definition.class
(variable_declarator name: (identifier) @name.definition.constant) @definition.constant
(enum_declaration name: (identifier) @name.definition.class) @definition.class
(call_expression function: (identifier) @name.reference)
(call_expression function: (member_expression property: (property_identifier) @name.reference))
(new_expression constructor: (identifier) @name.reference)
(type_annotation (type_identifier) @name.reference)
"#;
static TS_TAGS_QUERY: LazyLock<Option<Query>> = LazyLock::new(|| {
    build_tags_query(
        "typescript",
        &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        TS_TAGS_SRC,
    )
});

const PY_TAGS_SRC: &str = r#"
(function_definition name: (identifier) @name.definition.function) @definition.function
(class_definition name: (identifier) @name.definition.class) @definition.class
(assignment left: (identifier) @name.definition.constant) @definition.constant
(call function: (identifier) @name.reference)
(call function: (attribute attribute: (identifier) @name.reference))
"#;
static PY_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("python", &tree_sitter_python::LANGUAGE.into(), PY_TAGS_SRC));

const GO_TAGS_SRC: &str = r#"
(function_declaration name: (identifier) @name.definition.function) @definition.function
(method_declaration name: (field_identifier) @name.definition.function) @definition.function
(type_declaration (type_spec name: (type_identifier) @name.definition.class)) @definition.class
(type_declaration (type_alias name: (type_identifier) @name.definition.class)) @definition.class
"#;
static GO_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("go", &tree_sitter_go::LANGUAGE.into(), GO_TAGS_SRC));

const JAVA_TAGS_SRC: &str = r#"
(class_declaration name: (identifier) @name.definition.class) @definition.class
(method_declaration name: (identifier) @name.definition.function) @definition.function
(interface_declaration name: (identifier) @name.definition.class) @definition.class
(enum_declaration name: (identifier) @name.definition.class) @definition.class
(method_invocation object: (identifier) @name.reference)
(method_invocation name: (identifier) @name.reference)
(object_creation_expression type: (type_identifier) @name.reference)
"#;
static JAVA_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("java", &tree_sitter_java::LANGUAGE.into(), JAVA_TAGS_SRC));

const C_TAGS_SRC: &str = r#"
(function_definition declarator: (function_declarator declarator: (identifier) @name.definition.function)) @definition.function
(declaration type: (primitive_type) declarator: (identifier) @name.definition.constant) @definition.constant
(declaration type: (type_identifier) declarator: (identifier) @name.definition.constant) @definition.constant
(type_definition declarator: (type_identifier) @name.definition.class) @definition.class
(call_expression function: (identifier) @name.reference)
"#;
static C_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("c", &tree_sitter_c::LANGUAGE.into(), C_TAGS_SRC));

const CPP_TAGS_SRC: &str = r#"
(function_definition declarator: (function_declarator declarator: (identifier) @name.definition.function)) @definition.function
(class_specifier name: (type_identifier) @name.definition.class) @definition.class
(struct_specifier name: (type_identifier) @name.definition.class) @definition.class
(declaration type: (type_identifier) declarator: (identifier) @name.definition.constant) @definition.constant
(call_expression function: (identifier) @name.reference)
"#;
static CPP_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("cpp", &tree_sitter_cpp::LANGUAGE.into(), CPP_TAGS_SRC));

const RUBY_TAGS_SRC: &str = r#"
(class name: (constant) @name.definition.class) @definition.class
(method name: (identifier) @name.definition.function) @definition.function
(module name: (constant) @name.definition.module) @definition.module
(call method: (identifier) @name.reference)
"#;
static RUBY_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("ruby", &tree_sitter_ruby::LANGUAGE.into(), RUBY_TAGS_SRC));

const LUA_TAGS_SRC: &str = r#"
(function_declaration name: (identifier) @name.definition.function) @definition.function
(function name: (identifier) @name.definition.function) @definition.function
(assignment (identifier) @name.definition.constant) @definition.constant
"#;
static LUA_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("lua", &tree_sitter_lua::LANGUAGE.into(), LUA_TAGS_SRC));

const BASH_TAGS_SRC: &str = r#"
(function_definition name: (word) @name.definition.function) @definition.function
(variable_assignment name: (variable_name) @name.definition.constant) @definition.constant
"#;
static BASH_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("bash", &tree_sitter_bash::LANGUAGE.into(), BASH_TAGS_SRC));

const KT_TAGS_SRC: &str = r#"
(class_declaration name: (type_identifier) @name.definition.class) @definition.class
(function_declaration (simple_identifier) @name.definition.function) @definition.function
(object_declaration name: (type_identifier) @name.definition.class) @definition.class
(interface_declaration name: (type_identifier) @name.definition.class) @definition.class
"#;
static KT_TAGS_QUERY: LazyLock<Option<Query>> = LazyLock::new(|| {
    build_tags_query(
        "kotlin",
        &tree_sitter_kotlin_ng::LANGUAGE.into(),
        KT_TAGS_SRC,
    )
});

const SWIFT_TAGS_SRC: &str = r#"
(function_declaration name: (simple_identifier) @name.definition.function) @definition.function
(class_declaration name: (type_identifier) @name.definition.class) @definition.class
(struct_declaration name: (type_identifier) @name.definition.class) @definition.class
(protocol_declaration name: (type_identifier) @name.definition.class) @definition.class
(enum_declaration name: (type_identifier) @name.definition.class) @definition.class
"#;
static SWIFT_TAGS_QUERY: LazyLock<Option<Query>> = LazyLock::new(|| {
    build_tags_query("swift", &tree_sitter_swift::LANGUAGE.into(), SWIFT_TAGS_SRC)
});

const CSHARP_TAGS_SRC: &str = r#"
(class_declaration name: (identifier) @name.definition.class) @definition.class
(method_declaration name: (identifier) @name.definition.function) @definition.function
(interface_declaration name: (identifier) @name.definition.class) @definition.class
(struct_declaration name: (identifier) @name.definition.class) @definition.class
(enum_declaration name: (identifier) @name.definition.class) @definition.class
(invocation_expression function: (identifier) @name.reference)
(object_creation_expression type: (identifier) @name.reference)
"#;
static CSHARP_TAGS_QUERY: LazyLock<Option<Query>> = LazyLock::new(|| {
    build_tags_query(
        "csharp",
        &tree_sitter_c_sharp::LANGUAGE.into(),
        CSHARP_TAGS_SRC,
    )
});

const ELIXIR_TAGS_SRC: &str = r#"
(call target: (identifier) @ignore)
(unary_operator operand: (call target: (identifier) @name.definition.function)) @definition.function
"#;
static ELIXIR_TAGS_QUERY: LazyLock<Option<Query>> = LazyLock::new(|| {
    build_tags_query(
        "elixir",
        &tree_sitter_elixir::LANGUAGE.into(),
        ELIXIR_TAGS_SRC,
    )
});

const SCALA_TAGS_SRC: &str = r#"
(class_definition name: (identifier) @name.definition.class) @definition.class
(object_definition name: (identifier) @name.definition.class) @definition.class
(trait_definition name: (identifier) @name.definition.class) @definition.class
(function_definition name: (identifier) @name.definition.function) @definition.function
"#;
static SCALA_TAGS_QUERY: LazyLock<Option<Query>> = LazyLock::new(|| {
    build_tags_query("scala", &tree_sitter_scala::LANGUAGE.into(), SCALA_TAGS_SRC)
});

const PHP_TAGS_SRC: &str = r#"
(function_definition name: (name) @name.definition.function) @definition.function
(class_declaration name: (name) @name.definition.class) @definition.class
(interface_declaration name: (name) @name.definition.class) @definition.class
(method_declaration name: (name) @name.definition.function) @definition.function
(function_call_expression function: (name) @name.reference)
"#;
static PHP_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("php", &tree_sitter_php::LANGUAGE_PHP.into(), PHP_TAGS_SRC));

const HTML_TAGS_SRC: &str = r#"
(element (start_tag (tag_name) @name.definition.class)) @definition.class
"#;
static HTML_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("html", &tree_sitter_html::LANGUAGE.into(), HTML_TAGS_SRC));

const GLEAM_TAGS_SRC: &str = r#"
(function_definition name: (identifier) @name.definition.function) @definition.function
(custom_type_definition name: (type_identifier) @name.definition.class) @definition.class
(constant_definition name: (identifier) @name.definition.constant) @definition.constant
"#;
static GLEAM_TAGS_QUERY: LazyLock<Option<Query>> = LazyLock::new(|| {
    build_tags_query("gleam", &tree_sitter_gleam::LANGUAGE.into(), GLEAM_TAGS_SRC)
});

const DART_TAGS_SRC: &str = r#"
(class_definition name: (identifier) @name.definition.class) @definition.class
(method_signature name: (identifier) @name.definition.function) @definition.function
(function_signature name: (identifier) @name.definition.function) @definition.function
(enum_declaration name: (identifier) @name.definition.class) @definition.class
"#;
static DART_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("dart", &tree_sitter_dart::LANGUAGE.into(), DART_TAGS_SRC));

const STARLARK_TAGS_SRC: &str = r#"
(function_statement name: (identifier) @name.definition.function) @definition.function
(assignment left: (identifier) @name.definition.constant) @definition.constant
"#;
static STARLARK_TAGS_QUERY: LazyLock<Option<Query>> = LazyLock::new(|| {
    build_tags_query(
        "starlark",
        &tree_sitter_starlark::LANGUAGE.into(),
        STARLARK_TAGS_SRC,
    )
});

const NIX_TAGS_SRC: &str = r#"
(binding name: (attrpath) @name.definition.constant) @definition.constant
"#;
static NIX_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("nix", &tree_sitter_nix::LANGUAGE.into(), NIX_TAGS_SRC));

const ZIG_TAGS_SRC: &str = r#"
(function_declaration name: (identifier) @name.definition.function) @definition.function
(struct_declaration name: (identifier) @name.definition.class) @definition.class
(enum_declaration name: (identifier) @name.definition.class) @definition.class
(const_declaration name: (identifier) @name.definition.constant) @definition.constant
"#;
static ZIG_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("zig", &tree_sitter_zig::LANGUAGE.into(), ZIG_TAGS_SRC));

const CSS_TAGS_SRC: &str = r#"
(rule_set (selectors (class_selector (class_name) @name.definition.class))) @definition.class
(rule_set (selectors (id_selector (id_name) @name.definition.class))) @definition.class
"#;
static CSS_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("css", &tree_sitter_css::LANGUAGE.into(), CSS_TAGS_SRC));

const FISH_TAGS_SRC: &str = r#"
(function_definition name: (word) @name.definition.function) @definition.function
"#;
static FISH_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("fish", &tree_sitter_fish::language(), FISH_TAGS_SRC));

const PERL_TAGS_SRC: &str = r#"
(subroutine_declaration_statement name: (bareword) @name.definition.function) @definition.function
(package_statement (package_name) @name.definition.class) @definition.class
"#;
static PERL_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("perl", &tree_sitter_perl::LANGUAGE.into(), PERL_TAGS_SRC));

// SQL DDL: surface the names of schema objects an agent would navigate by.
// DML (select/insert/update/delete) and ALTER/DROP are intentionally not
// matched, so they contribute no tags -- same noise filtering as other
// extractors that ignore usage nodes.
// Note: tree-sitter-sequel as published has no `create_procedure` node, so
// procedures are not captured here either.
const SQL_TAGS_SRC: &str = r#"
(create_table (object_reference) @name.definition.class) @definition.class
(create_view (object_reference) @name.definition.class) @definition.class
(create_materialized_view (object_reference) @name.definition.class) @definition.class
(create_type (object_reference) @name.definition.class) @definition.class
(create_function (object_reference) @name.definition.function) @definition.function
(create_trigger (object_reference) @name.definition.function) @definition.function
(create_index (object_reference) @name.definition.function) @definition.function
(create_schema (identifier) @name.definition.module) @definition.module
"#;
static SQL_TAGS_QUERY: LazyLock<Option<Query>> =
    LazyLock::new(|| build_tags_query("sql", &tree_sitter_sequel::LANGUAGE.into(), SQL_TAGS_SRC));

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_SAMPLE: &str = r#"
use std::sync::Arc;

pub fn hello() -> u32 { 42 }
fn helper() {}

struct Foo { x: u32 }
enum Bar { A, B }

impl Foo {
    pub fn method(&self) -> u32 { self.x }
}

fn main() {
    let f = Foo { x: 1 };
    hello();
    f.method();
}
"#;

    #[test]
    fn rust_tags_extract_defs_and_refs() {
        let tags = extract_tags(RUST_SAMPLE, LangId::Rust, "test.rs");
        let defs: Vec<&str> = tags
            .iter()
            .filter(|t| t.kind == TagKind::Def)
            .map(|t| t.ident.as_str())
            .collect();
        assert!(defs.contains(&"hello"));
        assert!(defs.contains(&"Foo"));
        assert!(defs.contains(&"Bar"));
        assert!(defs.contains(&"main"));
    }

    #[test]
    fn python_tags_extract_defs() {
        let src = "def foo():\n    pass\nclass Bar:\n    def baz(self):\n        pass\n";
        let tags = extract_tags(src, LangId::Python, "test.py");
        let def_names: Vec<&str> = tags
            .iter()
            .filter(|t| t.kind == TagKind::Def)
            .map(|t| t.ident.as_str())
            .collect();
        assert!(def_names.contains(&"foo"));
        assert!(def_names.contains(&"Bar"));
        assert!(def_names.contains(&"baz"));
    }

    #[test]
    fn sql_tags_extract_ddl_definitions() {
        let src = r#"
CREATE TABLE public.users (id INT PRIMARY KEY);
CREATE VIEW active_users AS SELECT id FROM users;
CREATE FUNCTION add_one(x INT) RETURNS INT LANGUAGE plpgsql AS $$ BEGIN RETURN x + 1; END; $$;
CREATE TRIGGER set_updated_at BEFORE UPDATE ON users EXECUTE FUNCTION update_timestamp();
CREATE INDEX idx_users_email ON users (email);
CREATE TYPE mood AS ENUM ('sad', 'ok');
CREATE SCHEMA analytics;
SELECT * FROM users;
INSERT INTO users VALUES (1);
"#;
        let tags = extract_tags(src, LangId::Sql, "schema.sql");
        let defs: Vec<&str> = tags
            .iter()
            .filter(|t| t.kind == TagKind::Def)
            .map(|t| t.ident.as_str())
            .collect();
        assert!(defs.contains(&"users"));
        assert!(defs.contains(&"active_users"));
        assert!(defs.contains(&"add_one"));
        assert!(defs.contains(&"mood"));
        assert!(defs.contains(&"analytics"));
    }

    #[test]
    fn mentioned_idents_extracts_tokens() {
        let idents = extract_mentioned_idents("please look at the hello function and Foo struct");
        let set: std::collections::HashSet<&str> = idents.iter().map(|s| s.as_str()).collect();
        assert!(set.contains("hello"));
        assert!(set.contains("Foo"));
        assert!(set.contains("function"));
        assert!(!set.contains("at"));
    }

    #[test]
    fn file_path_to_idents_stem_and_parts() {
        let idents = file_path_to_idents("src/foo_bar.rs");
        let set: std::collections::HashSet<&str> = idents.iter().map(|s| s.as_str()).collect();
        assert!(set.contains("foo_bar"));
        assert!(set.contains("foo"));
        assert!(set.contains("bar"));
    }
}
