/// Compress search/grep output by limiting files and matches per file.
pub fn compress_search(text: &str, max_files: usize, max_matches_per_file: usize) -> String {
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
        result.push_str(&format!("... {} more files omitted\n", remaining));
    }

    result
}
