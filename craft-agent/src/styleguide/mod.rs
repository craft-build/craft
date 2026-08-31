//! Styleguide rules served from the project's argosys: the local bundle
//! (opened via craft-storage's [`ArgosyStore`]) plus any imported bundles
//! discovered beside it under the project's state dir and in the global
//! store, mirroring `ProjectContext::open_project_with_state`'s layout.
//! Rule ids are argosy concept paths minus the `styleguide/` prefix,
//! qualified with the argosy manifest name for imported rules (e.g.
//! `acme-style/rust/naming/snake-case-vars`); local rules stay unqualified.
//! Lookups accept the bare path too, so ids cited before a rule was
//! imported keep resolving.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use argosy::{Argosy, StyleguideRule};
use craft_storage::argosy_store::ArgosyStore;

pub mod reviewer;

const DEFAULT_SEARCH_LIMIT: usize = 10;
const RULES_PER_SECTION: usize = 5;
const GLOBAL_STATE_DIR: &str = "global";

pub fn detect_language(file_path: &str) -> Option<&'static str> {
    let path = Path::new(file_path);

    let name = path.file_name()?.to_str()?;
    match name {
        "Makefile" | "makefile" => return Some("makefile"),
        "Dockerfile" => return Some("dockerfile"),
        _ => {}
    }

    let ext = path.extension()?.to_str()?.to_lowercase();

    match ext.as_str() {
        "rs" => Some("rust"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "ts" | "tsx" => Some("typescript"),
        "py" | "pyw" | "pyi" => Some("python"),
        "go" => Some("go"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" | "ixx" => Some("cpp"),
        "java" => Some("java"),
        "cs" => Some("csharp"),
        "rb" | "erb" => Some("ruby"),
        "php" => Some("php"),
        "swift" => Some("swift"),
        "kt" | "kts" => Some("kotlin"),
        "scala" => Some("scala"),
        "sh" | "bash" | "zsh" | "fish" => Some("shell"),
        "yaml" | "yml" => Some("yaml"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        "md" | "mdx" => Some("markdown"),
        _ => None,
    }
}

/// A styleguide rule paired with its source argosy: `None` is the local
/// bundle, `Some(name)` an imported one by manifest name.
struct RuleEntry {
    argosy: Option<String>,
    rule: StyleguideRule,
}

impl RuleEntry {
    /// The rule's stable id: the argosy concept path minus the `styleguide/`
    /// prefix (e.g. `rust/naming/snake-case-vars`), prefixed with the argosy
    /// manifest name when the rule comes from an import.
    fn key(&self) -> String {
        match &self.argosy {
            Some(name) => format!(
                "{name}/{}",
                self.rule
                    .id()
                    .as_str()
                    .strip_prefix("styleguide/")
                    .unwrap_or_else(|| self.rule.id().as_str())
            ),
            None => bare_key(&self.rule).to_string(),
        }
    }

    /// The rule's short display name: its producer `rule_id` when set, else
    /// the final path segment.
    fn name(&self) -> &str {
        if let Some(id) = self.rule.rule_id() {
            return id;
        }
        self.rule
            .id()
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or_else(|| self.rule.id().as_str())
    }
}

/// The concept path minus the `styleguide/` prefix, unqualified.
fn bare_key(rule: &StyleguideRule) -> &str {
    rule.id()
        .as_str()
        .strip_prefix("styleguide/")
        .unwrap_or_else(|| rule.id().as_str())
}

/// Local rules first, then rules from imported bundles in sorted order.
fn all_rules(store: &ArgosyStore) -> Vec<RuleEntry> {
    let mut entries: Vec<_> = store
        .with(|argosy| StyleguideRule::list(argosy).unwrap_or_default())
        .into_iter()
        .map(|rule| RuleEntry { argosy: None, rule })
        .collect();
    for (name, rules) in imported_rules(store.root()) {
        entries.extend(rules.into_iter().map(|rule| RuleEntry {
            argosy: Some(name.clone()),
            rule,
        }));
    }
    if entries.is_empty() {
        entries = builtin::rules();
    }
    entries
}

/// Imported bundles beside the local one under the project's state dir and
/// in the global store, as `(manifest name, rules)` in sorted path order.
/// Unreadable or invalid bundles are skipped: styleguide reads never
/// hard-fail on a broken import.
fn imported_rules(local_root: &Path) -> Vec<(String, Vec<StyleguideRule>)> {
    let Some(project_dir) = local_root.parent() else {
        return Vec::new();
    };
    let mut roots: Vec<PathBuf> = bundle_dirs(project_dir, Some(local_root));
    // Craft lays the local bundle out as `<state>/projects/<id>/argosy`; the
    // state root (and thus its `global/` store) may sit two levels up.
    for ancestor in project_dir.ancestors().skip(1).take(2) {
        roots.extend(bundle_dirs(&ancestor.join(GLOBAL_STATE_DIR), None));
    }
    roots
        .into_iter()
        .filter_map(|root| {
            let argosy = Argosy::open(&root).ok()?;
            let name = argosy.manifest().name().to_string();
            Some((name, StyleguideRule::list(&argosy).ok()?))
        })
        .collect()
}

/// Subdirectories of `dir` holding an `argosy.md` manifest, sorted; `skip`
/// (the local bundle) is excluded.
pub(crate) fn bundle_dirs(dir: &Path, skip: Option<&Path>) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("argosy.md").is_file() && skip.is_none_or(|s| *p != s))
        .collect();
    dirs.sort();
    dirs
}

fn rules(store: &ArgosyStore, language: Option<&str>, category: Option<&str>) -> Vec<RuleEntry> {
    all_rules(store)
        .into_iter()
        .filter(|entry| {
            language.is_none_or(|l| entry.rule.language() == Some(l))
                && category.is_none_or(|c| entry.rule.category() == Some(c))
        })
        .collect()
}

pub fn list_categories(store: &ArgosyStore, language: &str) -> String {
    let all = rules(store, None, None);
    let mut languages = BTreeSet::new();
    for entry in &all {
        if let Some(l) = entry.rule.language() {
            languages.insert(l.to_string());
        }
    }

    let matched = rules(store, Some(language), None);
    if matched.is_empty() {
        return format!(
            "No styleguide rules found for: {language}\nAvailable languages: {}",
            languages.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }

    let mut by_category: BTreeSet<&str> = BTreeSet::new();
    for entry in &matched {
        if let Some(c) = entry.rule.category() {
            by_category.insert(c);
        }
    }

    let mut lines = vec![
        format!("# Styleguide Categories for {language}"),
        String::new(),
    ];
    for cat in &by_category {
        let count = matched
            .iter()
            .filter(|e| e.rule.category() == Some(cat))
            .count();
        lines.push(format!("## {cat}"));
        lines.push(format!("Rules: {count}"));
        lines.push(String::new());
    }
    lines.push(
        "Use `styleguide_get` to fetch specific rules, or `styleguide_search` to find rules."
            .into(),
    );
    lines.join("\n")
}

fn relevance(entry: &RuleEntry, query: &str) -> Option<usize> {
    let lower = query.to_lowercase();
    let key = entry.key().to_lowercase();
    let name = entry.name().to_lowercase();
    let description = entry
        .rule
        .concept()
        .description()
        .unwrap_or_default()
        .to_lowercase();
    if key == lower || name == lower {
        Some(100)
    } else if key.contains(&lower) {
        Some(80)
    } else if name.contains(&lower) {
        Some(60)
    } else if description.contains(&lower) {
        Some(40)
    } else if entry
        .rule
        .concept()
        .tags()
        .iter()
        .any(|t| t.to_lowercase().contains(&lower))
    {
        Some(30)
    } else {
        None
    }
}

pub fn search_rules(
    store: &ArgosyStore,
    query: &str,
    language: Option<&str>,
    category: Option<&str>,
    tags: Option<&Vec<String>>,
    limit: Option<usize>,
) -> String {
    let filter_tags = tags.map(Vec::as_slice).unwrap_or(&[]);
    let mut results: Vec<(RuleEntry, usize)> = rules(store, language, category)
        .into_iter()
        .filter(|entry| {
            filter_tags.is_empty()
                || entry
                    .rule
                    .concept()
                    .tags()
                    .iter()
                    .any(|t| filter_tags.iter().any(|f| f == t))
        })
        .filter_map(|entry| relevance(&entry, query).map(|score| (entry, score)))
        .collect();
    results.sort_by_key(|(_, score)| std::cmp::Reverse(*score));

    let limit = limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    let displayed: Vec<_> = results.iter().take(limit).collect();

    if displayed.is_empty() {
        return format!("No rules found matching: \"{query}\"");
    }

    let mut lines = vec![format!("# Search Results for \"{query}\"")];
    if let Some(lang) = language {
        lines.push(format!("Language: {lang}"));
    }
    if let Some(cat) = category {
        lines.push(format!("Category: {cat}"));
    }
    lines.push(format!(
        "Found {} rules (showing top {})",
        results.len(),
        displayed.len()
    ));
    lines.push(String::new());

    for (entry, score) in &displayed {
        lines.push(format!(
            "## {} ({}/{})",
            entry.key(),
            entry.rule.language().unwrap_or("?"),
            entry.rule.category().unwrap_or("?")
        ));
        lines.push(format!("**{}**", entry.name()));
        lines.push(format!(
            "Priority: {} | Relevance: {score}%",
            entry.rule.priority().unwrap_or("info").to_uppercase()
        ));
        lines.push(
            entry
                .rule
                .concept()
                .description()
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or("")
                .to_string(),
        );
        if let Some(good) = entry.rule.good_examples() {
            lines.push(format!("Good: {}", good.lines().next().unwrap_or("")));
        }
        if let Some(bad) = entry.rule.bad_examples() {
            lines.push(format!("Bad: {}", bad.lines().next().unwrap_or("")));
        }
        let tags = entry.rule.concept().tags();
        if !tags.is_empty() {
            lines.push(format!("Tags: {}", tags.join(", ")));
        }
        lines.push(String::new());
    }

    if results.len() > limit {
        lines.push(format!(
            "---\n{} more results available.",
            results.len() - limit
        ));
    }
    lines.join("\n")
}

fn append_rule_detail(lines: &mut Vec<String>, entry: &RuleEntry) {
    lines.push(format!("### {}: {}", entry.key(), entry.name()));
    if let Some(priority) = entry.rule.priority() {
        lines.push(format!("Priority: {}", priority.to_uppercase()));
    }
    lines.push(
        entry
            .rule
            .concept()
            .description()
            .unwrap_or_default()
            .trim()
            .to_string(),
    );
    if let Some(pattern) = entry.rule.pattern() {
        lines.push(format!("Pattern: `{pattern}`"));
    }
    if let Some(good) = entry.rule.good_examples() {
        lines.push("\n**Good:**".into());
        for ex in good.lines() {
            lines.push(format!("  {ex}"));
        }
    }
    if let Some(bad) = entry.rule.bad_examples() {
        lines.push("\n**Bad:**".into());
        for ex in bad.lines() {
            lines.push(format!("  {ex}"));
        }
    }
    let tags = entry.rule.concept().tags();
    if !tags.is_empty() {
        lines.push(format!("\nTags: {}", tags.join(", ")));
    }
    lines.push(String::new());
}

/// Resolves `id` against `entries` with deterministic precedence: an exact
/// match on qualified key, bare key, or full concept path wins over a
/// display-name match, and within a tier the first entry wins — local rules
/// before imports, imports in sorted order (the order `all_rules` yields).
/// So a bare id colliding across argosys always resolves the same way, and
/// a qualified id always disambiguates.
fn resolve<'a>(entries: &'a [RuleEntry], id: &str) -> Option<&'a RuleEntry> {
    let exact =
        |e: &RuleEntry| e.key() == id || bare_key(&e.rule) == id || e.rule.id().as_str() == id;
    entries.iter().find(|e| exact(e)).or_else(|| {
        entries
            .iter()
            .find(|e| e.rule.rule_id().is_some_and(|r| r == id))
    })
}

pub fn get_rules(
    store: &ArgosyStore,
    language: &str,
    category: Option<&str>,
    rule_ids: Option<&Vec<String>>,
    file_path: Option<&str>,
) -> Result<String, String> {
    if let Some(fp) = file_path {
        let detected = detect_language(fp)
            .ok_or_else(|| format!("Could not detect language for file: {fp}"))?;
        let filename = Path::new(fp)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(fp);

        let mut sections = vec![
            format!("# Styleguide Context for {filename}"),
            format!("Detected language: {detected}"),
            String::new(),
        ];
        let langs: Vec<&str> = if detected == "general" {
            vec!["general"]
        } else {
            vec!["general", detected]
        };
        for lang in langs {
            for entry in rules(store, Some(lang), category)
                .iter()
                .take(RULES_PER_SECTION)
            {
                sections.push(format!("\n**{}**: {}", entry.key(), entry.name()));
                sections.push(
                    entry
                        .rule
                        .concept()
                        .description()
                        .unwrap_or_default()
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_string(),
                );
                if let Some(good) = entry.rule.good_examples() {
                    sections.push(format!("Good: {}", good.lines().next().unwrap_or("")));
                }
            }
        }
        return Ok(sections.join("\n"));
    }

    if let Some(ids) = rule_ids {
        let all = rules(store, None, None);
        let mut found = Vec::new();
        let mut not_found = Vec::new();
        for id in ids {
            match resolve(&all, id) {
                Some(entry) => found.push(entry),
                None => not_found.push(id.as_str()),
            }
        }

        if found.is_empty() {
            return Ok(format!("No rules found with IDs: {}", ids.join(", ")));
        }

        let mut lines = vec![format!("# Styleguide Rules: {language}"), String::new()];
        for entry in &found {
            append_rule_detail(&mut lines, entry);
        }
        if !not_found.is_empty() {
            lines.push(format!("---\nNot found: {}", not_found.join(", ")));
        }
        return Ok(lines.join("\n"));
    }

    let cat =
        category.ok_or_else(|| "Must provide category, rule_ids, or file_path".to_string())?;

    let matched = rules(store, Some(language), Some(cat));
    if matched.is_empty() {
        let available: BTreeSet<String> = rules(store, Some(language), None)
            .iter()
            .filter_map(|e| e.rule.category().map(str::to_string))
            .collect();
        return Err(format!(
            "Category \"{cat}\" not found for {language}.\nAvailable: {}",
            available.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    let mut lines = vec![
        format!("# Styleguide: {language}/{cat}"),
        format!("Rules: {}", matched.len()),
        String::new(),
    ];
    for entry in &matched {
        append_rule_detail(&mut lines, entry);
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNAKE_CASE_RULE: (&str, &str, &str) = (
        "type: Styleguide Rule\ndescription: Bindings use snake_case\nlanguage: rust\ncategory: naming\npriority: error\nrule_id: SNAKE-CASE-VARS\ntags: [naming, vars]\n",
        "## Good\nlet retry_count = 3;\n\n## Bad\nlet retryCount = 3;\n",
        "styleguide/rust/naming/snake-case-vars.md",
    );

    fn write_bundle(root: &Path, name: &str, rules: &[(&str, &str, &str)]) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join("argosy.md"),
            format!(
                "---\ntype: Argosy Manifest\nname: {name}\nargosy_version: \"1.0.0\"\nokf_version: \"0.2\"\ndescription: {name}\n---\n# {name}\n"
            ),
        )
        .unwrap();
        for (frontmatter, body, rel) in rules {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("---\n{frontmatter}---\n{body}")).unwrap();
        }
    }

    /// A project state layout: `<tmp>/projects/<id>/argosy` local bundle,
    /// optional sibling imports, optional `<tmp>/global` bundles.
    fn store_with_rules(
        dir: &Path,
        imports: &[(&str, &str, &str)],
        globals: &[(&str, &str, &str)],
    ) -> ArgosyStore {
        let project_dir = dir.join("projects").join("proj");
        write_bundle(
            &project_dir.join("argosy"),
            "proj",
            &[
                SNAKE_CASE_RULE,
                (
                    "type: Styleguide Rule\ndescription: Prefer ? over unwrap for recoverable errors\nlanguage: rust\ncategory: error-handling\npriority: warn\ntags: [errors]\n",
                    "## Good\nfoo()?\n\n## Bad\nfoo().unwrap()\n",
                    "styleguide/rust/error-handling/propagate-errors.md",
                ),
                (
                    "type: Styleguide Rule\ndescription: Keep it simple, avoid unnecessary indirection\nlanguage: general\ncategory: simplicity\npriority: info\n",
                    "Prefer the boring solution.\n",
                    "styleguide/general/simplicity/no-bloat.md",
                ),
                (
                    "type: Note\ndescription: skipped, wrong type\n",
                    "body\n",
                    "styleguide/rust/naming/not-a-rule.md",
                ),
            ],
        );
        if !imports.is_empty() {
            write_bundle(&project_dir.join("acme-style"), "acme-style", imports);
        }
        if !globals.is_empty() {
            write_bundle(
                &dir.join("global").join("shared-style"),
                "shared-style",
                globals,
            );
        }
        ArgosyStore::open_or_init(&project_dir.join("argosy"), "proj").unwrap()
    }

    #[test]
    fn detect_language_common_extensions() {
        assert_eq!(detect_language("foo.rs"), Some("rust"));
        assert_eq!(detect_language("foo.py"), Some("python"));
        assert_eq!(detect_language("foo.ts"), Some("typescript"));
        assert_eq!(detect_language("Makefile"), Some("makefile"));
        assert_eq!(detect_language("foo.xyz"), None);
    }

    #[test]
    fn list_reports_categories_and_skips_non_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_rules(tmp.path(), &[], &[]);
        let text = list_categories(&store, "rust");
        assert!(text.contains("naming"));
        assert!(text.contains("error-handling"));
        assert!(!text.contains("not-a-rule"));
    }

    #[test]
    fn list_unknown_language_names_available_languages() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_rules(tmp.path(), &[], &[]);
        let text = list_categories(&store, "cobol");
        assert!(text.contains("rust"));
        assert!(text.contains("general"));
    }

    #[test]
    fn imported_rules_are_listed_with_qualified_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_rules(
            tmp.path(),
            &[(
                "type: Styleguide Rule\ndescription: No wildcard imports\nlanguage: rust\ncategory: imports\npriority: error\n",
                "## Bad\nuse std::*;\n",
                "styleguide/rust/imports/no-wildcards.md",
            )],
            &[],
        );
        let text = list_categories(&store, "rust");
        assert!(text.contains("imports"));

        let found = search_rules(&store, "wildcard", None, None, None, None);
        assert!(found.contains("acme-style/rust/imports/no-wildcards"));
        assert!(!found.contains("## rust/naming/snake-case-vars"));
    }

    #[test]
    fn global_state_imports_are_included() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_rules(
            tmp.path(),
            &[],
            &[(
                "type: Styleguide Rule\ndescription: Prefer ? over unwrap for recoverable errors\nlanguage: rust\ncategory: error-handling\npriority: error\n",
                "## Bad\nunwrap()\n",
                "styleguide/rust/errors/global-rule.md",
            )],
        );
        let found = search_rules(&store, "global-rule", None, None, None, None);
        assert!(found.contains("shared-style/rust/errors/global-rule"));
    }

    #[test]
    fn broken_import_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_rules(tmp.path(), &[], &[]);
        let project_dir = tmp.path().join("projects").join("proj");
        std::fs::create_dir_all(project_dir.join("broken")).unwrap();
        std::fs::write(
            project_dir.join("broken").join("argosy.md"),
            "not frontmatter",
        )
        .unwrap();
        let text = list_categories(&store, "rust");
        assert!(text.contains("naming"));
    }

    #[test]
    fn search_finds_rules_by_rule_id_and_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_rules(tmp.path(), &[], &[]);
        let by_id = search_rules(&store, "SNAKE-CASE-VARS", None, None, None, None);
        assert!(by_id.contains("rust/naming/snake-case-vars"));

        let by_tag = search_rules(&store, "naming", Some("rust"), None, None, None);
        assert!(by_tag.contains("snake-case-vars"));
        assert!(!by_tag.contains("propagate-errors"));
    }

    #[test]
    fn search_tag_filter_narrows_results() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_rules(tmp.path(), &[], &[]);
        let tags = vec!["errors".to_string()];
        let text = search_rules(&store, "e", None, None, Some(&tags), None);
        assert!(text.contains("propagate-errors"));
        assert!(!text.contains("snake-case-vars"));
    }

    #[test]
    fn get_rules_by_category_includes_examples() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_rules(tmp.path(), &[], &[]);
        let text = get_rules(&store, "rust", Some("naming"), None, None).unwrap();
        assert!(text.contains("SNAKE-CASE-VARS"));
        assert!(text.contains("let retry_count"));
        assert!(text.contains("let retryCount"));
    }

    #[test]
    fn get_rules_by_ids_accepts_key_and_bare_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_rules(tmp.path(), &[], &[]);
        let ids = vec![
            "rust/naming/snake-case-vars".to_string(),
            "SNAKE-CASE-VARS".to_string(),
        ];
        let text = get_rules(&store, "rust", None, Some(&ids), None).unwrap();
        assert!(text.contains("snake_case"));
    }

    #[test]
    fn bare_id_collision_resolves_local_first_and_qualified_disambiguates() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_rules(
            tmp.path(),
            &[(
                "type: Styleguide Rule\ndescription: Imported copy of the naming rule\nlanguage: rust\ncategory: naming\n",
                "## Bad\nimported bad example\n",
                "styleguide/rust/naming/snake-case-vars.md",
            )],
            &[],
        );
        let bare = vec!["rust/naming/snake-case-vars".to_string()];
        let text = get_rules(&store, "rust", None, Some(&bare), None).unwrap();
        assert_eq!(text.matches("Bindings use snake_case").count(), 1);
        assert!(!text.contains("Imported copy"));

        let qualified = vec!["acme-style/rust/naming/snake-case-vars".to_string()];
        let text = get_rules(&store, "rust", None, Some(&qualified), None).unwrap();
        assert!(text.contains("Imported copy"));
        assert!(!text.contains("Bindings use snake_case"));
    }

    #[test]
    fn get_rules_by_ids_resolves_imported_rules_by_bare_and_qualified_key() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_rules(
            tmp.path(),
            &[(
                "type: Styleguide Rule\ndescription: No wildcard imports\nlanguage: rust\ncategory: imports\n",
                "## Bad\nuse std::*;\n",
                "styleguide/rust/imports/no-wildcards.md",
            )],
            &[],
        );
        for id in [
            "acme-style/rust/imports/no-wildcards",
            "rust/imports/no-wildcards",
        ] {
            let ids = vec![id.to_string()];
            let text = get_rules(&store, "rust", None, Some(&ids), None).unwrap();
            assert!(text.contains(id), "missing {id}:\n{text}");
        }
    }

    #[test]
    fn get_rules_by_ids_reports_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_rules(tmp.path(), &[], &[]);
        let ids = vec!["no-such-rule".to_string()];
        let text = get_rules(&store, "rust", None, Some(&ids), None).unwrap();
        assert!(text.contains("No rules found with IDs: no-such-rule"));
    }

    #[test]
    fn get_rules_by_file_path_covers_general_and_detected_language() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_rules(tmp.path(), &[], &[]);
        let text = get_rules(&store, "rust", None, None, Some("src/main.rs")).unwrap();
        assert!(text.contains("Detected language: rust"));
        assert!(text.contains("snake-case-vars"));
        assert!(text.contains("no-bloat"));
    }
    #[test]
    fn builtin_rules_used_when_no_argosy_rules_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("proj");
        let store = ArgosyStore::open_or_init(&project_dir.join("argosy"), "proj").unwrap();
        assert!(list_categories(&store, "rust").contains("naming"));
        let found = search_rules(&store, "SNAKE-CASE-VARS", None, None, None, None);
        assert!(found.contains("rust/naming/snake-case-vars"));
    }
}

mod builtin {
    use std::str::FromStr;

    use argosy::{Concept, ConceptId, StyleguideRule};
    use serde::Deserialize;
    use serde_yaml::{Mapping, Value};

    use super::RuleEntry;

    const FILES: &[(&str, &str)] = &[
        (
            "rust/attributes.yaml",
            include_str!("../../styleguides/rust/attributes.yaml"),
        ),
        (
            "rust/comments.yaml",
            include_str!("../../styleguides/rust/comments.yaml"),
        ),
        (
            "rust/control_flow.yaml",
            include_str!("../../styleguides/rust/control_flow.yaml"),
        ),
        (
            "rust/error_handling.yaml",
            include_str!("../../styleguides/rust/error_handling.yaml"),
        ),
        (
            "rust/formatting.yaml",
            include_str!("../../styleguides/rust/formatting.yaml"),
        ),
        (
            "rust/functional.yaml",
            include_str!("../../styleguides/rust/functional.yaml"),
        ),
        (
            "rust/memory.yaml",
            include_str!("../../styleguides/rust/memory.yaml"),
        ),
        (
            "rust/naming.yaml",
            include_str!("../../styleguides/rust/naming.yaml"),
        ),
        (
            "rust/optimization.yaml",
            include_str!("../../styleguides/rust/optimization.yaml"),
        ),
        (
            "rust/organization.yaml",
            include_str!("../../styleguides/rust/organization.yaml"),
        ),
        (
            "rust/ownership.yaml",
            include_str!("../../styleguides/rust/ownership.yaml"),
        ),
        (
            "rust/practices.yaml",
            include_str!("../../styleguides/rust/practices.yaml"),
        ),
        (
            "general/abstraction.yaml",
            include_str!("../../styleguides/general/abstraction.yaml"),
        ),
        (
            "general/comments.yaml",
            include_str!("../../styleguides/general/comments.yaml"),
        ),
        (
            "general/const_usage.yaml",
            include_str!("../../styleguides/general/const_usage.yaml"),
        ),
        (
            "general/control_flow.yaml",
            include_str!("../../styleguides/general/control_flow.yaml"),
        ),
        (
            "general/error_handling.yaml",
            include_str!("../../styleguides/general/error_handling.yaml"),
        ),
        (
            "general/functional_patterns.yaml",
            include_str!("../../styleguides/general/functional_patterns.yaml"),
        ),
        (
            "general/memory_management.yaml",
            include_str!("../../styleguides/general/memory_management.yaml"),
        ),
        (
            "general/naming.yaml",
            include_str!("../../styleguides/general/naming.yaml"),
        ),
        (
            "general/optimization.yaml",
            include_str!("../../styleguides/general/optimization.yaml"),
        ),
        (
            "general/organization.yaml",
            include_str!("../../styleguides/general/organization.yaml"),
        ),
        (
            "general/static_analysis.yaml",
            include_str!("../../styleguides/general/static_analysis.yaml"),
        ),
    ];

    #[derive(Deserialize)]
    struct File {
        metadata: Metadata,
        rules: Vec<Rule>,
    }

    #[derive(Deserialize)]
    struct Metadata {
        language: String,
        category: String,
    }

    #[derive(Deserialize)]
    struct Rule {
        id: String,
        name: String,
        description: String,
        #[serde(default)]
        priority: Option<String>,
        #[serde(default = "enabled_by_default")]
        enabled: bool,
        #[serde(default)]
        pattern: Option<String>,
        #[serde(default)]
        examples: Examples,
        #[serde(default)]
        tags: Vec<String>,
    }

    #[derive(Default, Deserialize)]
    struct Examples {
        #[serde(default)]
        good: Vec<String>,
        #[serde(default)]
        bad: Vec<String>,
    }

    fn enabled_by_default() -> bool {
        true
    }

    /// The built-in rules bundled with craft, used when the project has no
    /// styleguide rules of its own in any argosy.
    pub(super) fn rules() -> Vec<RuleEntry> {
        FILES
            .iter()
            .filter_map(|(path, text)| parse(path, text))
            .flatten()
            .collect()
    }

    fn parse(path: &str, text: &str) -> Option<Vec<RuleEntry>> {
        let file: File = serde_yaml::from_str(text)
            .inspect_err(
                |e| tracing::warn!(file = path, error = %e, "invalid built-in styleguide file"),
            )
            .ok()?;
        let meta = &file.metadata;
        Some(
            file.rules
                .into_iter()
                .filter(|r| r.enabled)
                .filter_map(|r| rule(meta, r))
                .collect(),
        )
    }

    fn rule(meta: &Metadata, r: Rule) -> Option<RuleEntry> {
        let id = ConceptId::from_str(&format!(
            "styleguide/{}/{}/{}",
            meta.language,
            meta.category,
            r.id.to_lowercase()
        ))
        .ok()?;
        let mut fm = Mapping::new();
        fm.insert(Value::from("type"), Value::from("Styleguide Rule"));
        fm.insert(Value::from("description"), Value::from(r.name));
        fm.insert(Value::from("language"), Value::from(meta.language.clone()));
        fm.insert(Value::from("category"), Value::from(meta.category.clone()));
        fm.insert(Value::from("rule_id"), Value::from(r.id));
        if let Some(priority) = &r.priority {
            fm.insert(Value::from("priority"), Value::from(priority.clone()));
        }
        if let Some(pattern) = &r.pattern {
            fm.insert(Value::from("pattern"), Value::from(pattern.clone()));
        }
        if !r.tags.is_empty() {
            fm.insert(
                Value::from("tags"),
                Value::Sequence(r.tags.into_iter().map(Value::from).collect()),
            );
        }
        let mut body = r.description;
        body.push_str(&section("Good", &r.examples.good, &meta.language));
        body.push_str(&section("Bad", &r.examples.bad, &meta.language));
        let concept = Concept::new(fm, body).ok()?;
        Some(RuleEntry {
            argosy: None,
            rule: StyleguideRule::from_parts(id, concept),
        })
    }

    fn section(title: &str, items: &[String], fence: &str) -> String {
        if items.is_empty() {
            return String::new();
        }
        let blocks: Vec<String> = items
            .iter()
            .map(|e| format!("```{fence}\n{e}\n```"))
            .collect();
        format!("\n\n## {title}\n\n{}", blocks.join("\n\n"))
    }
}
