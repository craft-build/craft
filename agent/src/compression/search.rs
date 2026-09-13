/// Compress search/grep output by limiting files and matches per file.
pub(crate) fn compress_search(text: &str, max_files: usize, max_matches_per_file: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let mut file_groups: Vec<(String, Vec<&str>)> = Vec::new();
    let mut current_file = String::new();
    let mut current_matches: Vec<&str> = Vec::new();

    for line in &lines {
        let trimmed = line.trim();
        if !line.starts_with(' ')
            && !line.starts_with('\t')
            && !trimmed.is_empty()
            && !trimmed.starts_with('.')
            && !trimmed.starts_with('-')
        {
            if !current_file.is_empty() {
                file_groups.push((
                    std::mem::take(&mut current_file),
                    std::mem::take(&mut current_matches),
                ));
            }
            current_file = trimmed.to_owned();
        } else {
            current_matches.push(*line);
        }
    }
    if !current_file.is_empty() {
        file_groups.push((current_file, current_matches));
    }

    let display_count = max_files.min(file_groups.len());
    let mut result = String::new();

    for (file, matches) in file_groups.iter().take(display_count) {
        result.push_str(file);
        result.push('\n');
        for m in matches.iter().take(max_matches_per_file) {
            result.push_str(m);
            result.push('\n');
        }
        if matches.len() > max_matches_per_file {
            result.push_str(&format!(
                "  ... {} more matches in this file\n",
                matches.len() - max_matches_per_file
            ));
        }
    }

    let remaining = file_groups.len().saturating_sub(max_files);
    if remaining > 0 {
        result.push_str(&format!("... {remaining} more files omitted\n"));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grouped(files: usize, matches: usize) -> String {
        let mut text = String::new();
        for f in 0..files {
            text.push_str(&format!("src/file{f}.rs\n"));
            for m in 0..matches {
                text.push_str(&format!("  {m}: match text\n"));
            }
        }
        text
    }

    #[test]
    fn caps_files_and_matches_per_file() {
        let text = grouped(25, 8);
        let out = compress_search(&text, 20, 5);
        assert!(out.contains("src/file19.rs"));
        assert!(!out.contains("src/file20.rs"));
        assert!(out.contains("5 more files omitted"));
        assert!(out.contains("3 more matches in this file"));
    }

    #[test]
    fn small_result_unchanged_in_shape() {
        let text = "a.rs\n  1: m\n";
        let out = compress_search(text, 20, 5);
        assert!(out.contains("a.rs"));
        assert!(out.contains("1: m"));
    }
}
