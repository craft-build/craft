use animist::agent::tool::{ToolContent, ToolOutput};
use serde_json::{Value, json};

const INSTRUCTION_TRAILER: &str = "\n\n---\nInstructions from: ";
const UNTRUSTED_OPEN: &str = "<untrusted-content>\n";
const UNTRUSTED_CLOSE: &str = "\n</untrusted-content>";

/// Converts a tool's full contents into the value exposed to codemode.
pub fn adapt(tool_name: &str, output: &ToolOutput) -> Result<Value, String> {
    let text = joined_text(output);
    match tool_name {
        "read_text" => Ok(Value::String(
            first_text(output).unwrap_or_default().to_owned(),
        )),
        "read" => Ok(Value::String(text)),
        "glob" => path_list(tool_name, &text, "No files found"),
        "grep" => grep(&text),
        "list" | "list_dir" => path_list(tool_name, &text, "(empty directory)"),
        "bash" => Ok(bash(&text)),
        _ if tool_name.contains("__") => Ok(mcp(&text)),
        _ => Ok(Value::String(text)),
    }
}

/// Joins stdout and the final expression under one UTF-8-safe byte cap.
pub fn shape_output(stdout: &str, final_repr: Option<&str>, max_bytes: usize) -> String {
    let mut combined = match final_repr {
        Some(repr) if stdout.is_empty() => repr.to_owned(),
        Some(repr) => {
            let separator = if stdout.ends_with('\n') { "" } else { "\n" };
            format!("{stdout}{separator}{repr}")
        }
        None if stdout.is_empty() => "(no output: print or end with an expression)".to_owned(),
        None => stdout.to_owned(),
    };
    let mut end = max_bytes.min(combined.len());
    while !combined.is_char_boundary(end) {
        end -= 1;
    }
    combined.truncate(end);
    combined
}

fn first_text(output: &ToolOutput) -> Option<&str> {
    output.content.0.iter().find_map(|block| match block {
        ToolContent::Text { text } => Some(text.as_str()),
        ToolContent::Image { .. } => None,
    })
}

fn joined_text(output: &ToolOutput) -> String {
    output
        .content
        .0
        .iter()
        .filter_map(|block| match block {
            ToolContent::Text { text } => Some(text.as_str()),
            ToolContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn path_list(tool_name: &str, text: &str, empty: &str) -> Result<Value, String> {
    reject_truncation(tool_name, text)?;
    let text = text
        .split_once(INSTRUCTION_TRAILER)
        .map_or(text, |head| head.0);
    if text.trim().is_empty() || text.trim() == empty {
        return Ok(Value::Array(Vec::new()));
    }
    Ok(Value::Array(
        text.lines()
            .map(|line| Value::String(line.to_owned()))
            .collect(),
    ))
}

fn reject_truncation(tool_name: &str, text: &str) -> Result<(), String> {
    let truncated = text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("[Truncated:")
            || line.starts_with("[Search truncated:")
            || line.starts_with("[line truncated;")
            || line.starts_with("... [output truncated]")
    });
    if truncated {
        Err(format!("{tool_name} output was truncated"))
    } else {
        Ok(())
    }
}

fn grep(text: &str) -> Result<Value, String> {
    reject_truncation("grep", text)?;
    if text.trim().is_empty() || text.trim() == "No files found" {
        return Ok(Value::Array(Vec::new()));
    }
    if text.lines().any(|line| line.starts_with("[Skipped ")) {
        return Err("grep output omitted files".to_owned());
    }

    let mut path = None::<String>;
    let mut matches = Vec::new();
    for row in text.lines() {
        if row.is_empty() {
            continue;
        }
        if let Some(row) = row.strip_prefix("  ") {
            let Some(current_path) = path.as_deref() else {
                return Err(format!("grep match has no path: {row}"));
            };
            let Some((line, matched_text)) = row.split_once(": ") else {
                return Err(format!("invalid grep match: {row}"));
            };
            let line = line
                .parse::<u64>()
                .map_err(|_| format!("invalid grep line number: {row}"))?;
            matches.push(json!({
                "path": current_path,
                "line": line,
                "text": matched_text,
            }));
        } else if let Some(header) = row.strip_suffix(':')
            && !header.is_empty()
        {
            path = Some(header.to_owned());
        } else {
            return Err(format!("invalid grep output: {row}"));
        }
    }
    Ok(Value::Array(matches))
}

fn mcp(text: &str) -> Value {
    terminal_json_object(text).unwrap_or_else(|| Value::String(text.to_owned()))
}

fn terminal_json_object(text: &str) -> Option<Value> {
    let without_close = text.trim_end().strip_suffix("```")?;
    let opening = without_close.rfind("```json")?;
    let body = &without_close[opening + "```json".len()..];
    let body = body
        .strip_prefix("\r\n")
        .or_else(|| body.strip_prefix('\n'))?;
    match serde_json::from_str(body.trim()).ok()? {
        value @ Value::Object(_) => Some(value),
        _ => None,
    }
}

fn bash(text: &str) -> Value {
    let (without_exit, exit_code) = strip_exit_code(text);
    match unwrap_bash_output(without_exit) {
        Some(output) => json!({
            "exit_code": exit_code.or(Some(0)),
            "output": output,
        }),
        None if exit_code.is_some() => json!({
            "exit_code": exit_code,
            "output": without_exit,
        }),
        None => json!({
            "exit_code": null,
            "output": text,
        }),
    }
}

fn strip_exit_code(text: &str) -> (&str, Option<i64>) {
    if let Some(code) = text.strip_prefix("Exit code: ")
        && let Ok(code) = code.parse()
    {
        return ("", Some(code));
    }
    if let Some((body, code)) = text.rsplit_once("\nExit code: ")
        && let Ok(code) = code.parse()
    {
        return (body, Some(code));
    }
    (text, None)
}

fn unwrap_bash_output(text: &str) -> Option<String> {
    let (prefix, rest) = if let Some(rest) = text.strip_prefix(UNTRUSTED_OPEN) {
        ("", rest)
    } else {
        let start = text.find(&format!("\n{UNTRUSTED_OPEN}"))? + 1;
        (&text[..start], &text[start + UNTRUSTED_OPEN.len()..])
    };
    let body = rest.strip_suffix(UNTRUSTED_CLOSE)?;
    Some(format!("{prefix}{body}"))
}

#[cfg(test)]
mod tests {
    use animist::agent::tool::ToolContents;
    use serde_json::json;

    use super::*;

    fn output(blocks: &[&str]) -> ToolOutput {
        ToolOutput::builder()
            .content(ToolContents(
                blocks
                    .iter()
                    .map(|text| ToolContent::Text {
                        text: (*text).to_owned(),
                    })
                    .collect(),
            ))
            .build()
    }

    #[test]
    fn read_text_returns_only_the_exact_first_text_block() {
        let result = adapt(
            "read_text",
            &output(&["body\n", "Instructions from: AGENTS.md\nrules"]),
        )
        .expect("read_text adapts");
        assert_eq!(result, json!("body\n"));
    }

    #[test]
    fn read_preserves_numbered_text() {
        let numbered = "1: first\n2: second";
        assert_eq!(
            adapt("read", &output(&[numbered])).expect("read adapts"),
            json!(numbered)
        );
    }

    #[test]
    fn empty_file_search_and_directory_outputs_become_arrays() {
        for (name, text) in [
            ("glob", "No files found"),
            ("grep", "No files found"),
            ("list", "(empty directory)"),
        ] {
            assert_eq!(
                adapt(name, &output(&[text])).expect("empty adapts"),
                json!([])
            );
        }
    }

    #[test]
    fn path_lists_drop_instruction_trailers_but_keep_entries() {
        let text = "src/\nCargo.toml\n\n---\nInstructions from: /repo/AGENTS.md\nrules";
        assert_eq!(
            adapt("list", &output(&[text])).expect("list adapts"),
            json!(["src/", "Cargo.toml"])
        );
    }

    #[test]
    fn incomplete_list_outputs_are_rejected() {
        for (name, text) in [
            ("glob", "src/a.rs\n\n[Truncated: more files may exist.]"),
            (
                "grep",
                "a.rs:\n  1: text\n\n[Search truncated: more matches may exist.]",
            ),
            (
                "grep",
                "a.rs:\n  1: text\n  [line truncated; excerpt starts at byte column 1; match at byte column 1]",
            ),
            ("list", "... [output truncated]"),
        ] {
            let error = adapt(name, &output(&[text])).expect_err("partial data must not pass");
            assert!(error.contains("truncated"), "{error}");
        }
    }

    #[test]
    fn grep_becomes_path_line_text_objects() {
        let text = "src/a.rs:\n  2: let value: &str = \"x:y\";\n\nsrc/b.rs:\n  10:   indented";
        assert_eq!(
            adapt("grep", &output(&[text])).expect("grep adapts"),
            json!([
                {"path": "src/a.rs", "line": 2, "text": "let value: &str = \"x:y\";"},
                {"path": "src/b.rs", "line": 10, "text": "  indented"}
            ])
        );
    }

    #[test]
    fn mcp_returns_only_a_terminal_fenced_json_object() {
        let text = "result follows\n```json\n{\n  \"answer\": 42\n}\n```";
        assert_eq!(
            adapt("server__tool", &output(&[text])).expect("MCP adapts"),
            json!({"answer": 42})
        );
        let not_terminal = format!("{text}\nextra");
        assert_eq!(
            adapt("server__tool", &output(&[&not_terminal])).expect("text adapts"),
            json!(not_terminal)
        );
    }

    #[test]
    fn bash_reports_combined_output_and_truthful_exit_codes() {
        assert_eq!(
            adapt(
                "bash",
                &output(&["<untrusted-content>\nout\nerr\n</untrusted-content>"])
            )
            .expect("bash adapts"),
            json!({"exit_code": 0, "output": "out\nerr"})
        );
        assert_eq!(
            adapt(
                "bash",
                &output(&["<untrusted-content>\noops\n</untrusted-content>\nExit code: 3"])
            )
            .expect("failed bash adapts"),
            json!({"exit_code": 3, "output": "oops"})
        );
        assert_eq!(
            adapt("bash", &output(&["Background task: bg_1"])).expect("background adapts"),
            json!({"exit_code": null, "output": "Background task: bg_1"})
        );
    }

    #[test]
    fn other_tools_join_all_text_blocks() {
        assert_eq!(
            adapt("edit", &output(&["first", "second"])).expect("edit adapts"),
            json!("first\nsecond")
        );
    }

    #[test]
    fn output_shaping_caps_the_combined_utf8_text() {
        assert_eq!(shape_output("out", Some("42"), 20), "out\n42");
        assert_eq!(shape_output("out\n", Some("42"), 20), "out\n42");
        assert_eq!(shape_output("", Some("éclair"), 3), "éc");
        assert_eq!(shape_output("é", Some("🙂"), 5), "é\n");
        assert_eq!(shape_output("", None, 10), "(no output");
        assert_eq!(
            shape_output("", None, 100),
            "(no output: print or end with an expression)"
        );
    }
}
