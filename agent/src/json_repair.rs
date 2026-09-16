//! Shared JSON-repair pipeline for model-produced text.
//!
//! Models asked for JSON frequently wrap it in prose or markdown fences, or
//! emit slightly malformed JSON (trailing commas, single quotes). Consumers
//! with schema-constrained output — auto-review decisions now, the subagent
//! `task` tool later — extract via [`extract_json`] so they share one
//! tolerance policy instead of each rolling its own slicing.

use jsonrepair::{Options as RepairOpts, loads as repair_loads};
use serde_json::Value;

/// Extract the last JSON object/array from the model's text. Tolerates
/// leading prose and markdown fences, and repairs minor malformations via
/// `jsonrepair`. Returns `Err` with a short reason when no JSON can be
/// recovered or the result is not an object/array.
pub fn extract_json(text: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    if let Ok(v @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str(trimmed) {
        return Ok(v);
    }
    let start = trimmed
        .find(['{', '['])
        .ok_or("no JSON object found in model response")?;
    let open = trimmed.as_bytes()[start];
    let close = if open == b'{' { '}' } else { ']' };
    let end = trimmed
        .rfind(close)
        .ok_or("unterminated JSON in model response")?;
    if end <= start {
        return Err("malformed JSON in model response".into());
    }
    let slice = &trimmed[start..=end];
    repair_loads(slice, &RepairOpts::default())
        .map_err(|e| format!("invalid JSON: {e}"))
        .and_then(|v| match v {
            Value::Object(_) | Value::Array(_) => Ok(v),
            other => Err(format!("expected JSON object or array, got {other}")),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_object_passes_through() {
        assert_eq!(
            extract_json(r#"{"verdict":"allow"}"#).unwrap(),
            json!({"verdict": "allow"})
        );
    }

    #[test]
    fn plain_array_passes_through() {
        assert_eq!(extract_json("[1, 2]").unwrap(), json!([1, 2]));
    }

    #[test]
    fn prose_wrapped_object_is_extracted() {
        let text = "Sure, here is my decision:\n{\"verdict\": \"deny\"}\nThanks!";
        assert_eq!(extract_json(text).unwrap(), json!({"verdict": "deny"}));
    }

    #[test]
    fn nested_object_in_prose_is_not_truncated() {
        let text = r#"Here you go: {"verdict":"allow","opts":{"x":1}} hope that helps"#;
        let v = extract_json(text).unwrap();
        assert_eq!(v["verdict"], "allow");
        assert_eq!(v["opts"]["x"], 1);
    }

    #[test]
    fn markdown_fenced_object_is_extracted() {
        let text = "```json\n{\"verdict\": \"allow\"}\n```";
        assert_eq!(extract_json(text).unwrap(), json!({"verdict": "allow"}));
    }

    #[test]
    fn trailing_comma_is_repaired() {
        let text = "{\"verdict\": \"allow\", \"risk\": \"low\",}";
        assert_eq!(
            extract_json(text).unwrap(),
            json!({"verdict": "allow", "risk": "low"})
        );
    }

    #[test]
    fn single_quotes_are_repaired() {
        let text = "{'verdict': 'allow'}";
        assert_eq!(extract_json(text).unwrap(), json!({"verdict": "allow"}));
    }

    #[test]
    fn scalar_result_is_rejected() {
        assert!(extract_json("just the number 42").is_err());
    }

    #[test]
    fn no_json_is_rejected() {
        assert!(extract_json("I think it's fine").is_err());
        assert!(extract_json("").is_err());
    }
}
