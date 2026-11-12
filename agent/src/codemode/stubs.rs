use serde_json::Value;

use super::catalog::CatalogEntry;

// Provider descriptions are a prompt surface, not an unbounded schema dump.
const MAX_STUB_BYTES: usize = 12 * 1024;
const OMITTED: &str = "# More tools are available; use list_tools for their names and schemas.\n";
const MCP_FOOTER: &str = "# Use list_tools(detail=\"<wire name>\") for full MCP schemas.\n";

pub(super) fn render(entries: &[CatalogEntry]) -> String {
    let mut output = declarations(entries);
    let full: Vec<String> = entries.iter().map(|entry| stub(entry, false)).collect();
    if joined(&output, &full).len() <= MAX_STUB_BYTES {
        output.push_str(&full.join("\n"));
        output.push('\n');
        return output;
    }

    // Large MCP catalogs retain discoverable one-line fallbacks while full
    // signatures remain available through the already-registered list_tools.
    let compact: Vec<String> = entries.iter().map(|entry| stub(entry, true)).collect();
    if joined(&output, &compact).len() + MCP_FOOTER.len() <= MAX_STUB_BYTES {
        output.push_str(&compact.join("\n"));
        output.push('\n');
        output.push_str(MCP_FOOTER);
        return output;
    }

    let mut omitted = 0;
    for (index, line) in compact.iter().enumerate() {
        let remaining = entries.len() - index;
        if output.len() + line.len() + 1 + OMITTED.len() > MAX_STUB_BYTES {
            omitted = remaining;
            break;
        }
        output.push_str(line);
        output.push('\n');
    }
    if omitted > 0 {
        output.push_str(OMITTED);
    }
    output
}

fn declarations(entries: &[CatalogEntry]) -> String {
    let mut output =
        String::from("from typing import Any, TypedDict\n\nToolError = RuntimeError\n");
    if entries.iter().any(|entry| entry.wire_name() == "grep") {
        output.push_str("\nclass Match(TypedDict):\n    path: str\n    line: int\n    text: str\n");
    }
    if entries.iter().any(|entry| entry.wire_name() == "bash") {
        output.push_str(
            "\nclass BashResult(TypedDict):\n    exit_code: int | None\n    output: str\n",
        );
    }
    output.push('\n');
    output
}

fn joined(prefix: &str, lines: &[String]) -> String {
    if lines.is_empty() {
        return prefix.to_owned();
    }
    format!("{prefix}{}\n", lines.join("\n"))
}

fn stub(entry: &CatalogEntry, compact_mcp: bool) -> String {
    let mcp = is_mcp(entry);
    let parameters = if compact_mcp && mcp {
        "*args: Any, **kwargs: Any".to_owned()
    } else {
        entry
            .parameters
            .iter()
            .map(|parameter| {
                let schema = &entry.entry.definition.input_schema;
                let mut rendered = format!(
                    "{}: {}",
                    parameter.python_name,
                    python_type(&parameter.schema, schema, 0)
                );
                if !parameter.required {
                    rendered.push_str(" = ");
                    rendered.push_str(
                        &parameter
                            .schema
                            .get("default")
                            .map(python_value)
                            .unwrap_or_else(|| "...".to_owned()),
                    );
                }
                rendered
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let description_limit = if mcp { 96 } else { 180 };
    let description = compact_description(&entry.entry.definition.description, description_limit);
    let suffix = if description.is_empty() {
        String::new()
    } else {
        format!("  # {description}")
    };
    format!(
        "async def {}({parameters}) -> {}: ...{suffix}",
        entry.python_name,
        return_type(entry)
    )
}

fn is_mcp(entry: &CatalogEntry) -> bool {
    entry.wire_name().contains("__")
}

fn return_type(entry: &CatalogEntry) -> &'static str {
    match entry.wire_name() {
        "glob" | "list" => "list[str]",
        "grep" => "list[Match]",
        "bash" => "BashResult",
        name if name.contains("__") => "dict[str, Any] | str",
        _ => "str",
    }
}

fn compact_description(description: &str, max_chars: usize) -> String {
    let line = description.lines().next().unwrap_or_default().trim();
    let mut compact: String = line
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(max_chars)
        .collect();
    if line.chars().count() > max_chars {
        compact.push('…');
    }
    compact
}

fn python_type(schema: &Value, root: &Value, depth: usize) -> String {
    if depth >= 8 {
        return "Any".to_owned();
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let resolved = reference
            .strip_prefix("#/$defs/")
            .and_then(|name| root.get("$defs")?.get(name))
            .or_else(|| {
                reference
                    .strip_prefix("#/definitions/")
                    .and_then(|name| root.get("definitions")?.get(name))
            });
        return resolved.map_or_else(
            || "Any".to_owned(),
            |schema| python_type(schema, root, depth + 1),
        );
    }
    for union_key in ["anyOf", "oneOf"] {
        if let Some(variants) = schema.get(union_key).and_then(Value::as_array) {
            let mut types: Vec<String> = variants
                .iter()
                .map(|variant| python_type(variant, root, depth + 1))
                .collect();
            types.dedup();
            if !types.is_empty() {
                return types.join(" | ");
            }
        }
    }
    if let Some(variants) = schema.get("allOf").and_then(Value::as_array)
        && let Some(first) = variants.first()
    {
        return python_type(first, root, depth + 1);
    }

    match schema.get("type") {
        Some(Value::Array(kinds)) => {
            let types: Vec<String> = kinds
                .iter()
                .map(|kind| scalar_type(kind.as_str().unwrap_or_default(), schema, root, depth))
                .collect();
            if types.is_empty() {
                "Any".to_owned()
            } else {
                types.join(" | ")
            }
        }
        Some(Value::String(kind)) => scalar_type(kind, schema, root, depth),
        _ if schema.get("properties").is_some() => "dict[str, Any]".to_owned(),
        _ => "Any".to_owned(),
    }
}

fn scalar_type(kind: &str, schema: &Value, root: &Value, depth: usize) -> String {
    match kind {
        "string" => "str".to_owned(),
        "integer" => "int".to_owned(),
        "number" => "float".to_owned(),
        "boolean" => "bool".to_owned(),
        "null" => "None".to_owned(),
        "array" => format!(
            "list[{}]",
            schema.get("items").map_or_else(
                || "Any".to_owned(),
                |items| python_type(items, root, depth + 1)
            )
        ),
        "object" => "dict[str, Any]".to_owned(),
        _ => "Any".to_owned(),
    }
}

fn python_value(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(_) | Value::String(_) => value.to_string(),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!(
                    "{}: {}",
                    python_value(&Value::String(key.clone())),
                    python_value(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use animist::agent::tool::ToolRegistry;
    use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput};

    use super::*;
    use crate::codemode::catalog::Catalog;

    struct TestTool;

    #[async_trait::async_trait]
    impl ToolImpl for TestTool {
        type Args = Value;
        type Output = ToolOutput;
        type Error = std::io::Error;

        fn definition(&self) -> ToolDefinition {
            ToolDefinition::builder()
                .name("grep")
                .description("Search text")
                .input_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "max_matches": {"type": "integer", "default": 100},
                        "glob": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": null}
                    },
                    "required": ["pattern"]
                }))
                .build()
        }

        async fn call(
            &self,
            _context: &ToolContext<'_>,
            _args: Value,
        ) -> Result<ToolOutput, std::io::Error> {
            unreachable!()
        }
    }

    #[test]
    fn renders_schema_types_defaults_and_adapter_declarations() {
        let mut registry = ToolRegistry::default();
        registry.register(TestTool).unwrap();
        let stubs = Catalog::from_registry(&registry).stubs();
        assert!(stubs.contains("class Match(TypedDict):"));
        assert!(stubs.contains("pattern: str"));
        assert!(stubs.contains("max_matches: int = 100"));
        assert!(stubs.contains("glob: str | None = None"));
        assert!(stubs.contains(") -> list[Match]: ...  # Search text"));
    }
}
