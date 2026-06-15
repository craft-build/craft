/// Compress JSON array output by keeping first N + last N items, replacing middle with summary.
pub fn compress_json_array(
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
        result.push_str(&format!("  ... {} items omitted ...\n", omitted));
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
