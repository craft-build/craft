/// Compress diff output by keeping hunk headers and added/removed lines, dropping context if over budget.
pub(crate) fn compress_diff(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return text.to_owned();
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut in_hunk = false;

    for line in &lines {
        if line.starts_with("@@")
            || line.starts_with("diff ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
        {
            kept.push(line);
            in_hunk = true;
            continue;
        }
        if line.starts_with('+') || line.starts_with('-') {
            kept.push(line);
            continue;
        }
        if in_hunk && kept.len() < max_lines {
            continue;
        }
    }

    if kept.len() > max_lines {
        kept.truncate(max_lines);
        kept.push("... diff truncated ...");
    }

    kept.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_diff_unchanged() {
        let text = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-x\n+y\n";
        assert_eq!(compress_diff(text, 100), text);
    }

    #[test]
    fn long_diff_drops_context_and_truncates() {
        let mut text = String::from("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,60 +1,60 @@\n");
        for i in 0..60 {
            text.push_str(&format!(" context {i}\n"));
        }
        for i in 0..120 {
            text.push_str(&format!("+added {i}\n"));
        }
        let out = compress_diff(&text, 100);
        assert!(!out.contains("context"), "context lines must be dropped");
        assert!(out.contains("+added"));
        assert!(out.contains("diff truncated"));
    }
}
