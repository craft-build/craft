use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use serde::Deserialize;

const STYLEGUIDES_DIR: include_dir::Dir =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/styleguides");

#[derive(Debug, Clone, Deserialize)]
pub struct RuleExamples {
    good: Option<Vec<String>>,
    bad: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StyleguideRule {
    pub id: String,
    pub name: String,
    pub description: String,
    pub priority: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub pattern: Option<String>,
    pub examples: Option<RuleExamples>,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    pub language: String,
    pub category: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StyleguideCategory {
    pub metadata: Metadata,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub rules: Vec<StyleguideRule>,
}

#[derive(Debug, Clone)]
pub struct LoadedStyleguide {
    pub language: String,
    pub category: String,
    pub data: StyleguideCategory,
}

struct StyleguideRegistry {
    styleguides: HashMap<String, LoadedStyleguide>,
    languages: HashSet<String>,
    categories: HashSet<String>,
}

impl StyleguideRegistry {
    fn load() -> Self {
        let mut styleguides = HashMap::new();
        let mut languages = HashSet::new();
        let mut categories = HashSet::new();

        for dir in STYLEGUIDES_DIR.dirs() {
            let language = match dir.path().to_str() {
                Some(l) if !l.starts_with('.') => l,
                _ => continue,
            };
            languages.insert(language.to_string());

            for file in dir.files() {
                let path = file.path();
                if !matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("yaml" | "yml")
                ) {
                    continue;
                }
                let category = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(c) => c,
                    None => continue,
                };

                let content = match file.contents_utf8() {
                    Some(c) => c,
                    None => continue,
                };

                let data: StyleguideCategory = match serde_yaml::from_str(content) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "failed to parse styleguide");
                        continue;
                    }
                };

                categories.insert(category.to_string());
                styleguides.insert(
                    format!("{language}:{category}"),
                    LoadedStyleguide {
                        language: language.to_string(),
                        category: category.to_string(),
                        data,
                    },
                );
            }
        }

        Self {
            styleguides,
            languages,
            categories,
        }
    }

    fn languages(&self) -> Vec<&str> {
        let mut v: Vec<_> = self.languages.iter().map(|s| s.as_str()).collect();
        v.sort();
        v
    }

    fn categories(&self, language: Option<&str>) -> Vec<&str> {
        let mut v: Vec<_> = if let Some(lang) = language {
            self.styleguides
                .values()
                .filter(|sg| sg.language == lang)
                .map(|sg| sg.category.as_str())
                .collect()
        } else {
            self.categories.iter().map(|s| s.as_str()).collect()
        };
        v.sort();
        v
    }

    fn get(&self, language: &str, category: &str) -> Option<&LoadedStyleguide> {
        self.styleguides.get(&format!("{language}:{category}"))
    }

    fn all_for_language(&self, language: &str) -> Vec<&LoadedStyleguide> {
        self.styleguides
            .values()
            .filter(|sg| sg.language == language)
            .collect()
    }

    fn search_rules(
        &self,
        query: &str,
        language: Option<&str>,
        category: Option<&str>,
        filter_tags: &[String],
    ) -> Vec<(&LoadedStyleguide, &StyleguideRule, usize)> {
        let lower = query.to_lowercase();
        let mut results = Vec::new();

        for sg in self.styleguides.values() {
            if let Some(lang) = language
                && sg.language != lang
            {
                continue;
            }
            if let Some(cat) = category
                && sg.category != cat
            {
                continue;
            }

            for rule in &sg.data.rules {
                if !filter_tags.is_empty() && !filter_tags.iter().any(|t| rule.tags.contains(t)) {
                    continue;
                }

                let relevance = if rule.id.to_lowercase() == lower {
                    100
                } else if rule.id.to_lowercase().contains(&lower) {
                    80
                } else if rule.name.to_lowercase().contains(&lower) {
                    60
                } else if rule.description.to_lowercase().contains(&lower) {
                    40
                } else if rule.tags.iter().any(|t| t.to_lowercase().contains(&lower)) {
                    30
                } else {
                    continue;
                };

                results.push((sg, rule, relevance));
            }
        }

        results.sort_by_key(|b| std::cmp::Reverse(b.2));
        results
    }
}

static REGISTRY: LazyLock<StyleguideRegistry> = LazyLock::new(StyleguideRegistry::load);

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
        "cpp" | "hpp" | "cc" | "cxx" => Some("cpp"),
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

pub fn list_categories(language: &str) -> String {
    let registry = &REGISTRY;
    let categories = registry.categories(Some(language));

    if categories.is_empty() {
        let langs = registry.languages().join(", ");
        return format!(
            "No styleguide categories found for: {language}\nAvailable languages: {langs}"
        );
    }

    let mut lines = vec![
        format!("# Styleguide Categories for {language}"),
        String::new(),
    ];
    for cat in &categories {
        if let Some(sg) = registry.get(language, cat) {
            let rule_count = sg.data.rules.iter().filter(|r| r.enabled).count();
            let desc = sg.data.metadata.description.as_deref().unwrap_or("");
            lines.push(format!("## {cat}"));
            if !desc.is_empty() {
                lines.push(desc.to_string());
            }
            lines.push(format!("Rules: {rule_count} enabled"));
            lines.push(String::new());
        }
    }
    lines.push(
        "Use `styleguide_get` to fetch specific categories, or `styleguide_search` to find rules."
            .into(),
    );
    lines.join("\n")
}

pub fn search_rules(
    query: &str,
    language: Option<&str>,
    category: Option<&str>,
    tags: Option<&Vec<String>>,
    limit: Option<usize>,
) -> String {
    let registry = &REGISTRY;
    let filter_tags = match tags {
        Some(t) => t.as_slice(),
        None => &[],
    };
    let results = registry.search_rules(query, language, category, filter_tags);
    let limit = limit.unwrap_or(10);
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

    for (sg, rule, relevance) in &displayed {
        lines.push(format!("## {} ({}/{})", rule.id, sg.language, sg.category));
        lines.push(format!("**{}**", rule.name));
        lines.push(format!(
            "Priority: {} | Relevance: {}%",
            rule.priority.to_uppercase(),
            relevance
        ));
        lines.push(rule.description.lines().next().unwrap_or("").to_string());
        if let Some(ref examples) = rule.examples
            && let Some(ref good) = examples.good
            && let Some(first) = good.first()
        {
            lines.push(format!("Good: {first}"));
        }
        if let Some(ref examples) = rule.examples
            && let Some(ref bad) = examples.bad
            && let Some(first) = bad.first()
        {
            lines.push(format!("Bad: {first}"));
        }
        lines.push(format!(
            "Tags: {}",
            if rule.tags.is_empty() {
                "none".into()
            } else {
                rule.tags.join(", ")
            }
        ));
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

pub fn get_rules(
    language: &str,
    category: Option<&str>,
    rule_ids: Option<&Vec<String>>,
    file_path: Option<&str>,
) -> Result<String, String> {
    let registry = &REGISTRY;

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

        for sg in registry.all_for_language("general") {
            let active: Vec<_> = sg.data.rules.iter().filter(|r| r.enabled).collect();
            if active.is_empty() {
                continue;
            }
            sections.push(format!("## {}", sg.data.metadata.name));
            for rule in active.iter().take(5) {
                sections.push(format!("\n**{}**: {}", rule.id, rule.name));
                sections.push(rule.description.lines().next().unwrap_or("").to_string());
            }
            sections.push(String::new());
        }

        if detected != "general" {
            for sg in registry.all_for_language(detected) {
                let active: Vec<_> = sg.data.rules.iter().filter(|r| r.enabled).collect();
                if active.is_empty() {
                    continue;
                }
                sections.push(format!("## {}", sg.data.metadata.name));
                for rule in active.iter().take(5) {
                    sections.push(format!("\n**{}**: {}", rule.id, rule.name));
                    sections.push(rule.description.lines().next().unwrap_or("").to_string());
                    if let Some(ref examples) = rule.examples
                        && let Some(ref good) = examples.good
                        && let Some(first) = good.first()
                    {
                        sections.push(format!("Good: {first}"));
                    }
                }
                sections.push(String::new());
            }
        }

        return Ok(sections.join("\n"));
    }

    if let Some(ids) = rule_ids {
        let mut found = Vec::new();
        let mut not_found = Vec::new();

        for id in ids {
            let mut matched = false;
            for sg in registry.all_for_language(language) {
                if let Some(rule) = sg.data.rules.iter().find(|r| r.id == *id) {
                    found.push((sg, rule));
                    matched = true;
                    break;
                }
            }
            if !matched {
                not_found.push(id.as_str());
            }
        }

        if found.is_empty() {
            return Ok(format!("No rules found with IDs: {}", ids.join(", ")));
        }

        let mut lines = vec![format!("# Styleguide Rules: {language}"), String::new()];

        let mut by_cat: HashMap<&str, Vec<(&LoadedStyleguide, &StyleguideRule)>> = HashMap::new();
        for (sg, rule) in &found {
            by_cat.entry(&sg.category).or_default().push((*sg, *rule));
        }

        for (cat, items) in &by_cat {
            lines.push(format!("## {cat}"));
            lines.push(String::new());
            for (_, rule) in items {
                lines.push(format!("### {}: {}", rule.id, rule.name));
                lines.push(format!("Priority: {}", rule.priority.to_uppercase()));
                lines.push(String::new());
                lines.push(rule.description.trim().to_string());
                lines.push(String::new());

                if let Some(ref examples) = rule.examples {
                    if let Some(ref good) = examples.good {
                        lines.push("**Good:**".into());
                        for ex in good {
                            lines.push(format!("  {ex}"));
                        }
                        lines.push(String::new());
                    }
                    if let Some(ref bad) = examples.bad {
                        lines.push("**Bad:**".into());
                        for ex in bad {
                            lines.push(format!("  {ex}"));
                        }
                        lines.push(String::new());
                    }
                }
                if !rule.tags.is_empty() {
                    lines.push(format!("Tags: {}", rule.tags.join(", ")));
                }
                lines.push(String::new());
            }
        }

        if !not_found.is_empty() {
            lines.push(format!("---\nNot found: {}", not_found.join(", ")));
        }

        return Ok(lines.join("\n"));
    }

    let cat =
        category.ok_or_else(|| "Must provide category, rule_ids, or file_path".to_string())?;

    let sg = registry.get(language, cat).ok_or_else(|| {
        let available = registry.categories(Some(language));
        format!(
            "Category \"{cat}\" not found for {language}.\nAvailable: {}",
            available.join(", ")
        )
    })?;

    let mut lines = vec![
        format!("# {}", sg.data.metadata.name),
        format!(
            "Language: {} | Category: {}",
            sg.data.metadata.language, sg.data.metadata.category
        ),
    ];
    if let Some(ref desc) = sg.data.metadata.description {
        lines.push(format!("Description: {desc}"));
    }
    lines.push(String::new());

    let active: Vec<_> = sg.data.rules.iter().filter(|r| r.enabled).collect();
    lines.push(format!("## Rules ({} enabled)", active.len()));
    lines.push(String::new());

    for rule in &active {
        lines.push(format!("### {}: {}", rule.id, rule.name));
        lines.push(format!("Priority: {}", rule.priority.to_uppercase()));
        lines.push(rule.description.trim().to_string());

        if let Some(ref examples) = rule.examples {
            if let Some(ref good) = examples.good {
                lines.push("\nGood:".into());
                for ex in good {
                    lines.push(format!("  {ex}"));
                }
            }
            if let Some(ref bad) = examples.bad {
                lines.push("\nBad:".into());
                for ex in bad {
                    lines.push(format!("  {ex}"));
                }
            }
        }

        if !rule.tags.is_empty() {
            lines.push(format!("\nTags: {}", rule.tags.join(", ")));
        }
        lines.push(String::new());
    }

    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_loads_bundled_styleguides() {
        let registry = &REGISTRY;
        let langs = registry.languages();
        assert!(
            langs.contains(&"rust"),
            "expected rust in languages: {langs:?}"
        );
        assert!(
            langs.contains(&"general"),
            "expected general in languages: {langs:?}"
        );
    }

    #[test]
    fn rust_has_expected_categories() {
        let registry = &REGISTRY;
        let cats = registry.categories(Some("rust"));
        assert!(
            cats.contains(&"naming"),
            "expected naming in rust categories: {cats:?}"
        );
        assert!(
            cats.contains(&"ownership"),
            "expected ownership in rust categories: {cats:?}"
        );
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
    fn search_finds_rules_by_id() {
        let registry = &REGISTRY;
        let results = registry.search_rules("SNAKE-CASE", Some("rust"), None, &[]);
        assert!(
            !results.is_empty(),
            "expected results for SNAKE-CASE search"
        );
        assert_eq!(results[0].1.id, "SNAKE-CASE-VARS");
    }

    #[test]
    fn get_rules_by_category() {
        let result = get_rules("rust", Some("naming"), None, None);
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("SNAKE-CASE-VARS"));
        assert!(text.contains("UPPER-CAMEL-TYPES"));
    }

    #[test]
    fn get_rules_by_file_path() {
        let result = get_rules("rust", None, None, Some("src/main.rs"));
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("Detected language: rust"));
        assert!(text.contains("Carmack"));
    }

    #[test]
    fn list_categories_for_language() {
        let text = list_categories("rust");
        assert!(text.contains("naming"));
        assert!(text.contains("ownership"));
    }

    #[test]
    fn search_rules_by_keyword() {
        let text = search_rules("immutable", Some("rust"), None, None, None);
        assert!(text.contains("Results"));
    }
}
