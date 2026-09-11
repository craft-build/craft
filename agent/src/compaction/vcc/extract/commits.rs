use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::super::normalize::NormalizedBlock;

type Lazy = LazyLock<Regex>;

#[derive(Debug, Clone)]
pub(crate) struct CommitInfo {
    pub hash: Option<String>,
    pub message: String,
}

static COMMIT_MSG_RE: Lazy = Lazy::new(|| {
    Regex::new(r#"(?s)git\s+commit[^\n]*?-m\s+(?:"((?:[^"\\]|\\.)*)"|'((?:[^'\\]|\\.)*)')"#)
        .unwrap()
});

static GIT_COMMIT_RE: Lazy = Lazy::new(|| Regex::new(r"\bgit\s+commit\b").unwrap());
static HASH_RE: Lazy = Lazy::new(|| Regex::new(r"\b([0-9a-f]{7,12})\b").unwrap());
static BRACKET_RE: Lazy = Lazy::new(|| Regex::new(r"\[\S+\s+([0-9a-f]{7,12})\]").unwrap());
static RANGE_RE: Lazy =
    Lazy::new(|| Regex::new(r"\b([0-9a-f]{7,12})\.\.([0-9a-f]{7,12})\b").unwrap());

fn first_line_of(text: &str) -> String {
    let line = text.split(['\n']).next().unwrap_or("");
    line.trim().to_string()
}

fn clean_message(msg: &str) -> String {
    msg.replace(r#"\""#, "\"")
        .replace(r"\'", "'")
        .trim()
        .to_string()
}
fn find_hash(output: &str) -> Option<String> {
    if let Some(c) = BRACKET_RE.captures(output) {
        return Some(c[1].to_string());
    }
    if let Some(c) = RANGE_RE.captures(output) {
        return Some(c[2].to_string());
    }
    HASH_RE.captures(output).map(|c| c[1].to_string())
}

/// Extract git commits from bash blocks (`git commit -m "..."`), pairing each
/// with a hash found in the immediately following bash block's output.
pub(crate) fn extract_commits(blocks: &[NormalizedBlock]) -> Vec<CommitInfo> {
    let mut commits: Vec<CommitInfo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for (i, b) in blocks.iter().enumerate() {
        let NormalizedBlock::Bash {
            command, output, ..
        } = b
        else {
            continue;
        };
        if !GIT_COMMIT_RE.is_match(command) {
            continue;
        }
        let caps = match COMMIT_MSG_RE.captures(command) {
            Some(c) => c,
            None => continue,
        };
        let raw = caps
            .get(1)
            .or_else(|| caps.get(2))
            .map(|m| m.as_str())
            .unwrap_or("");
        let message = first_line_of(&clean_message(raw));
        if message.is_empty() {
            continue;
        }

        let mut hash: Option<String> = find_hash(output);
        if hash.is_none() {
            for blk in blocks.iter().take((i + 3).min(blocks.len())).skip(i + 1) {
                let NormalizedBlock::Bash { output, .. } = blk else {
                    continue;
                };
                if let Some(h) = find_hash(output) {
                    hash = Some(h);
                    break;
                }
            }
        }

        let key = format!("{}::{}", hash.as_deref().unwrap_or(""), message);
        if seen.insert(key) {
            commits.push(CommitInfo { hash, message });
        }
    }

    commits
}

pub(crate) fn format_commits(commits: &[CommitInfo], limit: usize) -> Vec<String> {
    let start = commits.len().saturating_sub(limit);
    commits[start..]
        .iter()
        .map(|c| match &c.hash {
            Some(h) => format!("{h}: {}", c.message),
            None => c.message.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_commit_with_hash() {
        let blocks = vec![NormalizedBlock::Bash {
            command: r#"git commit -m "fix bug""#.into(),
            output: "[main abc1234] fix bug".into(),
            exit_code: Some(0),
            source_index: 0,
        }];
        let commits = extract_commits(&blocks);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].message, "fix bug");
        assert_eq!(commits[0].hash.as_deref(), Some("abc1234"));
    }

    #[test]
    fn format_commits_keeps_recent() {
        let commits = vec![
            CommitInfo {
                hash: None,
                message: "old".into(),
            },
            CommitInfo {
                hash: Some("deadbee".into()),
                message: "new".into(),
            },
        ];
        let out = format_commits(&commits, 8);
        assert_eq!(out, vec!["old", "deadbee: new"]);
    }
}
