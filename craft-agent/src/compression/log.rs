pub fn compress_log(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return text.to_owned();
    }

    let head = 3;
    let tail = 3;
    let mut kept: Vec<&str> = Vec::new();

    for line in lines.iter().take(head) {
        kept.push(line);
    }

    let middle: Vec<(usize, &str, i32)> = lines[head..lines.len().saturating_sub(tail)]
        .iter()
        .enumerate()
        .map(|(i, line)| (i + head, *line, super::keywords::score_log_line(line)))
        .collect();

    let mut important: Vec<(usize, &str)> = middle
        .iter()
        .filter(|(_, _, score)| *score > 0)
        .map(|(i, line, _)| (*i, *line))
        .collect();

    important.dedup_by(|a, b| {
        let a_trimmed =
            a.1.trim()
                .chars()
                .filter(|c| !c.is_whitespace())
                .take(40)
                .collect::<String>();
        let b_trimmed =
            b.1.trim()
                .chars()
                .filter(|c| !c.is_whitespace())
                .take(40)
                .collect::<String>();
        a_trimmed == b_trimmed
    });

    let remaining_budget = max_lines.saturating_sub(head + tail).min(important.len());
    important.truncate(remaining_budget);

    if !important.is_empty() {
        kept.push("... log lines omitted ...");
        for (_, line) in &important {
            kept.push(line);
        }
    }

    if tail > 0 {
        kept.push("... log lines omitted ...");
        for line in lines.iter().rev().take(tail).rev() {
            kept.push(line);
        }
    }

    kept.join("\n")
}
