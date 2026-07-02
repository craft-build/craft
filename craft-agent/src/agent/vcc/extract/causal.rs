use std::sync::LazyLock;

use regex::Regex;

use super::super::util::{clip, refine_breadcrumb_key};

const CAUSE_MARKERS: &[&str] = &[
    "the issue is",
    "the problem is",
    "the problem was",
    "the bug is",
    "the bug was",
    "the cause is",
    "root cause:",
    "root cause is",
    "the reason is",
    "fails because",
    "fails when",
    "fails due to",
    "crashes because",
    "crashes when",
    "crashes due to",
    "breaks because",
    "breaks when",
    "breaks due to",
    "because ",
    "since ",
    "due to ",
    "missing ",
    "lacking ",
    "lack of ",
    "absence of ",
    "can't ",
    "cannot ",
    "not properly ",
    "not correctly ",
    "not validating ",
    "not returning ",
    "not handling ",
    "not releasing ",
    "not checking ",
    "wrong ",
    "incorrect ",
    "stale ",
    "outdated ",
    "unhandled ",
    "uncaught ",
];

const RESOLUTION_MARKERS: &[&str] = &[
    "fix this by",
    "fix it by",
    "resolve this by",
    "resolve it by",
    "resolve by",
    "handle this by",
    "handle by",
    "address this by",
    "address by",
    "by adding",
    "by creating",
    "by implementing",
    "by introducing",
    "by applying",
    "by inserting",
    "by using",
    "by swapping",
    "by migrating",
    "by isolating",
    "by splitting",
    "by extracting",
    "by replacing",
    "by refactoring",
    "by wrapping",
    "by moving",
    "by removing",
    "added ",
    "created ",
    "implemented ",
    "introduced ",
    "applied ",
    "inserted ",
    "changed to",
    "updated to",
    "switched to",
    "migrated to",
    "replaced with",
    "replaced by",
    "refactored to",
    "extracted into",
    "set up ",
    "configured ",
    "enabled ",
    "swapped ",
    "isolated ",
    "splitting ",
    "split ",
    "wrapped ",
    "guarded ",
    "moved ",
    "removed ",
];

const FRAGMENT_MAX: usize = 60;
const CAUSAL_BREADCRUMB_MAX: usize = 40;
const SENTINEL_CHARS: &[char] = &[',', '.', ';', '!', '?', '\n'];

static SENTENCE_SPLIT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[.!?]").unwrap());

#[derive(Debug, Default, Clone)]
pub(crate) struct CausalChain {
    pub cause: Option<String>,
    pub resolution: Option<String>,
}

fn extract_fragment(text: &str, markers: &[&str]) -> Option<String> {
    let lower = text.to_lowercase();
    for marker in markers {
        let Some(idx) = lower.find(marker) else {
            continue;
        };
        let start = idx + marker.len();
        if start >= text.len() {
            continue;
        }
        let mut end = start;
        while end < text.len() && end - start < FRAGMENT_MAX {
            let ch = text[end..].chars().next().unwrap();
            if SENTINEL_CHARS.contains(&ch) {
                break;
            }
            end += ch.len_utf8();
        }
        let fragment = text[start..end].trim();
        if fragment.len() < 4 {
            continue;
        }
        return Some(fragment.to_string());
    }
    None
}

pub(crate) fn extract_causal_chain(text: &str) -> CausalChain {
    let mut cause = extract_fragment(text, CAUSE_MARKERS);
    let mut resolution = extract_fragment(text, RESOLUTION_MARKERS);

    if cause.is_none() || resolution.is_none() {
        for sentence in SENTENCE_SPLIT_RE.split(text) {
            let s = sentence.trim();
            if s.len() <= 3 {
                continue;
            }
            if cause.is_none() {
                cause = extract_fragment(s, CAUSE_MARKERS);
            }
            if resolution.is_none() {
                resolution = extract_fragment(s, RESOLUTION_MARKERS);
            }
            if cause.is_some() && resolution.is_some() {
                break;
            }
        }
    }

    CausalChain {
        cause: cause.map(|c| clip(&c, CAUSAL_BREADCRUMB_MAX)),
        resolution: resolution.map(|r| clip(&r, CAUSAL_BREADCRUMB_MAX)),
    }
}

#[cfg_attr(not(test), expect(dead_code))]
pub(crate) fn build_causal_breadcrumb(turn_summary: &str, chain: &CausalChain) -> String {
    let file_re =
        regex::Regex::new(r"(?:edited |read |wrote |created |deleted )?([^\s.\u2192]+\.\w{1,12})")
            .unwrap();
    let file = file_re.captures(turn_summary).map(|c| {
        let f = c[1].to_string();
        let parts: Vec<&str> = f.split('/').collect();
        if parts.len() > 2 {
            parts[parts.len() - 2..].join("/")
        } else {
            f
        }
    });

    let resolution_key = chain.resolution.as_deref().map(refine_breadcrumb_key);

    if let (Some(f), Some(rk)) = (&file, &resolution_key)
        && !rk.is_empty()
    {
        return format!("{f}|{rk}");
    }
    if let Some(rk) = &resolution_key
        && !rk.is_empty()
    {
        return rk.clone();
    }
    let before_arrow = turn_summary.split('\u{2192}').next().unwrap_or("").trim();
    let words: Vec<&str> = before_arrow
        .split_whitespace()
        .filter(|w| w.len() > 2)
        .take(3)
        .collect();
    if !words.is_empty() {
        return words.join(" ");
    }
    file.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_cause_and_resolution() {
        let chain =
            extract_causal_chain("The issue is a race condition. I fixed it by adding a mutex.");
        assert_eq!(chain.cause.as_deref(), Some("a race condition"));
        assert_eq!(chain.resolution.as_deref(), Some("a mutex"));
    }

    #[test]
    fn breadcrumb_combines_file_and_resolution() {
        let chain = CausalChain {
            cause: None,
            resolution: Some("a mutex".into()),
        };
        let crumb = build_causal_breadcrumb("edited auth.rs \u{2192} a mutex", &chain);
        assert_eq!(crumb, "auth.rs|mutex");
    }
}
