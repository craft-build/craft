use std::sync::LazyLock;

use regex::Regex;

use super::super::normalize::NormalizedBlock;
use super::super::util::{clip, non_empty_lines};

static PREF_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)\bprefer(?:s|red|ring)?\s+\w",
        r"(?i)\bdon'?t want\b",
        r"(?i)\balways (?:use|do|run|prefer|keep|make|format|write|add|set|put|prefix|start|include|append)\b",
        r"(?i)\bnever (?:use|do|run|push|commit|write|ignore|add|set|put|remove|delete|include|deploy)\b",
        r"(?i)\bplease (?:use|avoid|keep|make|don'?t|do not|format|write)\b",
        r"(?i)\b(?:style|format|language|naming)\s*[:=]\s*\S",
    ]
    .into_iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

pub(crate) fn extract_preferences(blocks: &[NormalizedBlock]) -> Vec<String> {
    let mut prefs: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for b in blocks {
        let NormalizedBlock::User { text, .. } = b else {
            continue;
        };
        for line in non_empty_lines(text) {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.len() < 5 || trimmed.len() > 200 {
                continue;
            }
            if trimmed.ends_with('?') {
                continue;
            }
            if !PREF_PATTERNS.iter().any(|p| p.is_match(trimmed)) {
                continue;
            }
            let clipped = clip(trimmed, 200);
            let key = clipped.to_lowercase();
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            prefs.push(clipped);
            break;
        }
    }

    prefs.truncate(10);
    prefs
}

pub(crate) fn dedup_preferences_against_goals(prefs: Vec<String>, goals: &[String]) -> Vec<String> {
    let goal_set: std::collections::HashSet<String> =
        goals.iter().map(|g| g.trim().to_lowercase()).collect();
    prefs
        .into_iter()
        .filter(|p| !goal_set.contains(&p.trim().to_lowercase()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_always_preference() {
        let blocks = vec![NormalizedBlock::User {
            text: "always use snake_case for tests".into(),
            source_index: 0,
        }];
        let prefs = extract_preferences(&blocks);
        assert_eq!(prefs, vec!["always use snake_case for tests"]);
    }

    #[test]
    fn dedup_against_goals() {
        let prefs = vec!["prefer rust".into(), "always use fmt".into()];
        let goals = vec!["prefer rust".to_lowercase()];
        let out = dedup_preferences_against_goals(prefs, &goals);
        assert_eq!(out, vec!["always use fmt"]);
    }
}
