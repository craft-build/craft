use std::sync::LazyLock;

use regex::Regex;

use super::super::normalize::NormalizedBlock;
use super::super::util::{clip, collapse_skill_lines, non_empty_lines};

static SCOPE_CHANGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(instead|actually|change of plan|forget that|new task|switch to|now I want|pivot|let'?s do|stop .* and)\b",
    )
    .unwrap()
});

static TASK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(fix|implement|add|create|build|refactor|debug|investigate|update|remove|delete|migrate|deploy|test|write|set up)\b",
    )
    .unwrap()
});

static NOISE_SHORT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(ok|yes|no|sure|yeah|yep|go|hi|hey|thx|thanks|ok\b.*|y|n|k)\s*[.!?]*$")
        .unwrap()
});

static NON_GOAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?xs)^\s*[\[\|]|```|^\s*(=[A-Z]+\(|function |const |let |var |import |export |class )|^(https?:|file:|/[A-Za-z])|\\n|^\s*For each\b|\bin full\b[^\n]*\b(comments|issue|issues|PRs?|linked)\b",
    )
    .unwrap()
});

static TEMPLATE_SIGNAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(For each\b|Do NOT implement\b|Analyze and propose\b|If Task/context\b|Output:\s*$)").unwrap()
});

const LEADING_CHARS: usize = 200;
const MAX_GOAL_CHARS: usize = 200;

fn truncate_at_template(lines: &[String]) -> Vec<String> {
    match lines.iter().position(|l| TEMPLATE_SIGNAL_RE.is_match(l)) {
        Some(idx) => lines[..idx].to_vec(),
        None => lines.to_vec(),
    }
}

fn strip_leading_bullet(line: &str) -> String {
    let trimmed = line.trim_start();
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.trim().to_string();
        }
    }
    if let Some(rest) = trimmed.strip_prefix(|c: char| c.is_ascii_digit())
        && let Some(rest2) = rest.strip_prefix(". ")
    {
        return rest2.trim().to_string();
    }
    trimmed.trim().to_string()
}

fn is_substantive_goal(text: &str) -> bool {
    let t = text.trim();
    if t.len() <= 5 || t.len() > MAX_GOAL_CHARS {
        return false;
    }
    if NOISE_SHORT_RE.is_match(t) || NON_GOAL_RE.is_match(t) {
        return false;
    }
    true
}

pub(crate) fn extract_goals(blocks: &[NormalizedBlock]) -> Vec<String> {
    let mut goals: Vec<String> = Vec::new();
    let mut latest_scope_change: Option<Vec<String>> = None;

    for b in blocks {
        let NormalizedBlock::User { text, .. } = b else {
            continue;
        };
        let raw_lines = non_empty_lines(text);
        let truncated = truncate_at_template(&raw_lines);
        let substantive: Vec<String> = truncated
            .into_iter()
            .filter(|l| is_substantive_goal(l))
            .collect();
        let collapsed = collapse_skill_lines(&substantive);
        let lines: Vec<String> = collapsed
            .into_iter()
            .map(|l| strip_leading_bullet(&l))
            .filter(|l| l.len() > 5)
            .collect();
        if lines.is_empty() {
            continue;
        }

        if goals.is_empty() {
            goals.extend(lines.into_iter().take(6));
            continue;
        }

        let leading = &text[..text.len().min(LEADING_CHARS)];
        if SCOPE_CHANGE_RE.is_match(leading) {
            latest_scope_change = Some(
                lines
                    .iter()
                    .take(3)
                    .map(|l| clip(l, MAX_GOAL_CHARS))
                    .collect(),
            );
        } else if TASK_RE.is_match(leading) && lines[0].len() > 15 {
            latest_scope_change = Some(
                lines
                    .iter()
                    .take(2)
                    .map(|l| clip(l, MAX_GOAL_CHARS))
                    .collect(),
            );
        }
    }

    if let Some(scope) = latest_scope_change.filter(|s| !s.is_empty()) {
        goals.push("[Scope change]".into());
        goals.extend(scope);
    }

    goals.truncate(8);
    goals
}
