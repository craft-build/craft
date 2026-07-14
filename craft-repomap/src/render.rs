use std::collections::BTreeMap;

const CHARS_PER_TOKEN: usize = 4;

pub fn render_map(defs: &[(String, String, usize)], max_tokens: u32) -> String {
    let max_chars = (max_tokens as usize) * CHARS_PER_TOKEN;

    let by_file: BTreeMap<&str, Vec<(&str, usize)>> = {
        let mut m: BTreeMap<&str, Vec<(&str, usize)>> = BTreeMap::new();
        for (path, ident, line) in defs {
            m.entry(path.as_str())
                .or_default()
                .push((ident.as_str(), *line));
        }
        m
    };

    let mut result = String::new();
    let mut current_chars = 0;

    let header_len = "```\n".len() + "\n```".len();
    let budget = max_chars.saturating_sub(header_len);

    for (path, idents) in &by_file {
        let mut file_text = format!("{path}:\n");
        for (ident, line) in idents {
            file_text.push_str(&format!("│ {ident} (line {line})\n"));
        }
        file_text.push('\n');

        if current_chars + file_text.len() > budget && !result.is_empty() {
            break;
        }

        result.push_str(&file_text);
        current_chars += file_text.len();
    }

    if result.is_empty() {
        String::new()
    } else {
        format!("```\n{result}```")
    }
}

pub fn count_tokens(text: &str) -> u32 {
    (text.len() / CHARS_PER_TOKEN) as u32
}

pub fn render_with_budget(all_defs: &[(String, String, usize)], max_tokens: u32) -> String {
    let target = ((max_tokens as f64) * 0.85) as u32;
    let mut lo = 1usize;
    let mut hi = all_defs.len();
    let mut best = String::new();

    while lo <= hi {
        let mid = (lo + hi) / 2;
        let subset = &all_defs[..mid.min(all_defs.len())];
        let rendered = render_map(subset, max_tokens);
        let tokens = count_tokens(&rendered);
        if tokens <= target {
            best = rendered;
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }

    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_map_fits_budget() {
        let defs: Vec<(String, String, usize)> = (0..50)
            .map(|i| (format!("file_{i}.rs"), format!("fn_{i}"), i + 1))
            .collect();
        let rendered = render_with_budget(&defs, 200);
        let tokens = count_tokens(&rendered);
        assert!(
            tokens <= 220,
            "rendered map should be within ~10% of budget: got {tokens} tokens"
        );
    }

    #[test]
    fn render_map_shows_file_headers() {
        let defs = vec![
            ("a.rs".to_string(), "foo".to_string(), 1),
            ("b.rs".to_string(), "bar".to_string(), 5),
        ];
        let rendered = render_map(&defs, 1000);
        assert!(rendered.contains("a.rs:"));
        assert!(rendered.contains("b.rs:"));
        assert!(rendered.contains("foo"));
        assert!(rendered.contains("bar"));
    }

    #[test]
    fn empty_defs_produce_empty_map() {
        let rendered = render_with_budget(&[], 1000);
        assert!(rendered.is_empty());
    }
}
