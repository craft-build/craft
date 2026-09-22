//! Shared JSON-repair pipeline for model-produced text.
//!
//! Models asked for JSON frequently wrap it in prose or markdown fences, or
//! emit slightly malformed JSON (trailing commas, single quotes). Consumers
//! with schema-constrained output — auto-review decisions now, the subagent
//! `task` tool later — extract via [`extract_json`] so they share one
//! tolerance policy instead of each rolling its own slicing.

use jsonrepair::{Options as RepairOpts, loads as repair_loads};
use serde_json::Value;

/// Find the last balanced object/array region in `text`: the outermost
/// `{...}`/`[...]` that closes at (or before) the last closing bracket.
/// Scanning from the end keeps earlier sibling objects out of the slice,
/// which a first-open→last-close span would stitch together.
fn last_balanced(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut end = None;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for i in (0..bytes.len()).rev() {
        let byte = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'}' | b']' => {
                if end.is_none() {
                    end = Some(i);
                }
                depth += 1;
            }
            b'{' | b'[' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[i..=end?]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract the last JSON object/array from the model's text. Tolerates
/// leading prose and markdown fences, and repairs minor malformations via
/// `jsonrepair`. Returns `Err` with a short reason when no JSON can be
/// recovered or the result is not an object/array.
pub fn extract_json(text: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    if let Ok(v @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str(trimmed) {
        return Ok(v);
    }
    let slice = last_balanced(trimmed).ok_or("no JSON object found in model response")?;
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

    /// Two JSON values in one response: the slice must span only the last
    /// object, not stitch both together.
    #[test]
    fn multiple_objects_extract_only_the_last() {
        let text = r#"First draft: {"x":1} Final: {"verdict":"deny"}"#;
        assert_eq!(extract_json(text).unwrap(), json!({"verdict": "deny"}));
    }

    #[test]
    fn braces_inside_strings_do_not_confuse_extraction() {
        let text = r#"{"note":"contains } bracket"} {"verdict":"allow"}"#;
        assert_eq!(extract_json(text).unwrap(), json!({"verdict": "allow"}));
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
