/// Compress JSON array output by keeping first N + last N items, replacing middle with summary.
pub(crate) fn compress_json_array(
    text: &str,
    max_items: usize,
    first_keep: usize,
    last_keep: usize,
) -> String {
    let trimmed = text.trim();

    if let Ok(arr) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(items) = arr.as_array()
    {
        if items.len() <= max_items {
            return text.to_owned();
        }

        let first: Vec<&serde_json::Value> = items.iter().take(first_keep).collect();
        let last: Vec<&serde_json::Value> = items.iter().rev().take(last_keep).rev().collect();
        let omitted = items
            .len()
            .saturating_sub(first_keep)
            .saturating_sub(last_keep);

        let mut result = String::from("[\n");
        for item in &first {
            result.push_str("  ");
            result.push_str(&serde_json::to_string(item).unwrap_or_default());
            result.push_str(",\n");
        }
        result.push_str(&format!("  ... {omitted} items omitted ...\n"));
        for (i, item) in last.iter().enumerate() {
            result.push_str("  ");
            result.push_str(&serde_json::to_string(item).unwrap_or_default());
            if i + 1 < last.len() {
                result.push(',');
            }
            result.push('\n');
        }
        result.push(']');
        return result;
    }

    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_array_unchanged() {
        let text = "[\n  {\"a\": 1},\n  {\"a\": 2}\n]";
        assert_eq!(compress_json_array(text, 15, 5, 3), text);
    }

    #[test]
    fn long_array_keeps_first_and_last_items() {
        let items: Vec<String> = (0..30).map(|i| format!("{{\"n\": {i}}}")).collect();
        let text = format!("[\n{}\n]", items.join(",\n"));
        let out = compress_json_array(&text, 15, 5, 3);
        assert!(out.contains("\"n\":0"));
        assert!(out.contains("\"n\":4"));
        assert!(out.contains("\"n\":27"));
        assert!(out.contains("\"n\":29"));
        assert!(!out.contains("\"n\":10"));
        assert!(out.contains("22 items omitted"));
    }

    #[test]
    fn non_array_json_unchanged() {
        let text = "{\"not\": \"an array\"}";
        assert_eq!(compress_json_array(text, 15, 5, 3), text);
    }
}
