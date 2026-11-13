use std::collections::{BTreeMap, BTreeSet};

use animist::agent::tool::{ToolEntry, ToolRegistry, ToolSelection};
use serde_json::{Map, Value};

use crate::config::CodemodeMode;

const CALLERS_KEY: &str = "craft.callers";
const EXCLUSIVE_MODEL_TOOLS: &[&str] = &["code", "question", "todo_write", "task"];

/// One positional argument, with both its Python and wire spellings.
#[derive(Clone, Debug)]
pub struct Parameter {
    pub python_name: String,
    pub wire_name: String,
    pub required: bool,
    pub schema: Value,
}

/// A code-callable tool and its frozen provider declaration.
#[derive(Clone)]
pub struct CatalogEntry {
    pub python_name: String,
    pub entry: ToolEntry,
    pub parameters: Vec<Parameter>,
}

impl CatalogEntry {
    pub fn wire_name(&self) -> &str {
        &self.entry.definition.name
    }
}

/// The immutable tool namespace captured immediately before `code` is registered.
#[derive(Clone)]
pub struct Catalog {
    entries: Vec<CatalogEntry>,
    wire_names: BTreeMap<String, String>,
    selection: ToolSelection,
    uncallable_names: Vec<String>,
}

impl Catalog {
    pub fn from_registry(registry: &ToolRegistry) -> Self {
        let snapshot = registry.entries();
        let mut used_names = BTreeSet::new();
        let mut entries = Vec::new();
        let mut uncallable_names = Vec::new();

        for entry in snapshot {
            if !code_callable(&entry) {
                if entry.definition.name != "list_tools" {
                    uncallable_names.push(entry.definition.name.clone());
                }
                continue;
            }
            let wire_name = entry.definition.name.clone();
            let base_name = match wire_name.as_str() {
                "list" => "list_dir".to_owned(),
                "list_tools_code" => "list_tools".to_owned(),
                _ => sanitize_identifier(&wire_name),
            };
            let python_name = unique_identifier(base_name, &mut used_names);
            let parameters = parameters_for(&entry.definition.input_schema);
            entries.push(CatalogEntry {
                python_name,
                entry,
                parameters,
            });
        }

        let mut wire_names = BTreeMap::new();
        for entry in &entries {
            wire_names.insert(entry.python_name.clone(), entry.wire_name().to_owned());
        }
        Self {
            entries,
            wire_names,
            selection: registry.selection(code_callable),
            uncallable_names,
        }
    }

    pub fn selection(&self) -> &ToolSelection {
        &self.selection
    }

    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    pub fn contains(&self, python_name: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.python_name == python_name)
    }

    /// The wire name behind a Python name, for keys that must match
    /// dispatch-side tallies; falls back to the given name when the call
    /// never bound (the unknown-tool case).
    pub fn wire_name<'a>(&'a self, python_name: &'a str) -> &'a str {
        self.wire_names
            .get(python_name)
            .map_or(python_name, String::as_str)
    }

    /// Converts a Python call into its registered wire name and JSON object.
    pub fn bind(
        &self,
        python_name: &str,
        args: Vec<Value>,
        kwargs: BTreeMap<String, Value>,
    ) -> Result<(String, Value), String> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.python_name == python_name)
            .ok_or_else(|| format!("unknown tool: {python_name}"))?;

        if args.len() > entry.parameters.len() {
            return Err(format!(
                "too many positional arguments (maximum {})",
                entry.parameters.len(),
            ));
        }
        if let Some(unknown) = kwargs.keys().find(|name| {
            !entry
                .parameters
                .iter()
                .any(|parameter| &parameter.python_name == *name)
        }) {
            return Err(format!("unknown argument '{unknown}'"));
        }

        let mut input = Map::new();
        for (parameter, value) in entry.parameters.iter().zip(args) {
            if kwargs.contains_key(&parameter.python_name) {
                return Err(format!("duplicate argument '{}'", parameter.python_name,));
            }
            input.insert(parameter.wire_name.clone(), value);
        }
        for parameter in &entry.parameters {
            if let Some(value) = kwargs.get(&parameter.python_name) {
                input.insert(parameter.wire_name.clone(), value.clone());
            }
        }
        for parameter in entry
            .parameters
            .iter()
            .filter(|parameter| parameter.required)
        {
            if !input.contains_key(&parameter.wire_name) {
                return Err(format!(
                    "missing required argument '{}'",
                    parameter.python_name,
                ));
            }
        }

        Ok((entry.wire_name().to_owned(), Value::Object(input)))
    }

    pub fn external_names(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.python_name.clone())
            .collect()
    }

    pub fn uncallable_names(&self) -> Vec<String> {
        self.uncallable_names.clone()
    }

    pub fn stubs(&self) -> String {
        super::stubs::render(&self.entries)
    }
}

/// Application metadata for a tool that is visible only to the model.
pub fn model_only_metadata() -> BTreeMap<String, Value> {
    caller_metadata("model")
}

/// Application metadata for a tool that is visible only to scripts.
pub fn code_only_metadata() -> BTreeMap<String, Value> {
    caller_metadata("code")
}

fn caller_metadata(caller: &str) -> BTreeMap<String, Value> {
    BTreeMap::from([(CALLERS_KEY.to_owned(), Value::Array(vec![caller.into()]))])
}

pub fn model_selection(registry: &ToolRegistry, mode: CodemodeMode) -> ToolSelection {
    registry.selection(|entry| model_visible(entry, mode))
}

pub fn model_visible(entry: &ToolEntry, mode: CodemodeMode) -> bool {
    if !has_caller(entry, "model") {
        return false;
    }
    match mode {
        CodemodeMode::Off | CodemodeMode::Alongside => true,
        CodemodeMode::Exclusive => EXCLUSIVE_MODEL_TOOLS.contains(&entry.definition.name.as_str()),
    }
}

fn code_callable(entry: &ToolEntry) -> bool {
    has_caller(entry, "code")
}

/// Missing metadata means both callers. Malformed application metadata fails
/// closed rather than accidentally widening either tool surface.
fn has_caller(entry: &ToolEntry, caller: &str) -> bool {
    match entry.metadata.get(CALLERS_KEY) {
        None => true,
        Some(Value::Array(callers)) => callers.iter().any(|value| value.as_str() == Some(caller)),
        Some(_) => false,
    }
}

fn parameters_for(schema: &Value) -> Vec<Parameter> {
    let required_order: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let required: BTreeSet<&str> = required_order.iter().copied().collect();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };

    // JSON object iteration is the schema's available property order. Partition
    // it without sorting so required arguments precede optional ones stably.
    let ordered = required_order
        .iter()
        .filter_map(|name| properties.get(*name).map(|schema| (*name, schema)))
        .chain(
            properties
                .iter()
                .filter(|(name, _)| !required.contains(name.as_str()))
                .map(|(name, schema)| (name.as_str(), schema)),
        );

    let mut used_names = BTreeSet::new();
    ordered
        .map(|(wire_name, schema)| Parameter {
            python_name: unique_identifier(sanitize_identifier(wire_name), &mut used_names),
            wire_name: wire_name.to_owned(),
            required: required.contains(wire_name),
            schema: schema.clone(),
        })
        .collect()
}

fn sanitize_identifier(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        sanitized.push('_');
    }
    if sanitized.starts_with(|character: char| character.is_ascii_digit()) {
        sanitized.insert(0, '_');
    }
    if is_python_keyword(&sanitized) {
        sanitized.push('_');
    }
    sanitized
}

fn unique_identifier(base: String, used: &mut BTreeSet<String>) -> String {
    if used.insert(base.clone()) {
        return base;
    }
    for suffix in 2.. {
        let candidate = format!("{base}_{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!()
}

fn is_python_keyword(name: &str) -> bool {
    matches!(
        name,
        "False"
            | "None"
            | "True"
            | "and"
            | "as"
            | "assert"
            | "async"
            | "await"
            | "break"
            | "case"
            | "class"
            | "continue"
            | "def"
            | "del"
            | "elif"
            | "else"
            | "except"
            | "finally"
            | "for"
            | "from"
            | "global"
            | "if"
            | "import"
            | "in"
            | "is"
            | "lambda"
            | "match"
            | "nonlocal"
            | "not"
            | "or"
            | "pass"
            | "raise"
            | "return"
            | "try"
            | "type"
            | "while"
            | "with"
            | "yield"
    )
}

#[cfg(test)]
mod tests {
    use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput};
    use serde_json::json;

    use super::*;

    struct TestTool {
        name: &'static str,
        schema: Value,
        metadata: BTreeMap<String, Value>,
    }

    impl TestTool {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                schema: json!({"type": "object", "properties": {}}),
                metadata: BTreeMap::new(),
            }
        }

        fn metadata(mut self, metadata: BTreeMap<String, Value>) -> Self {
            self.metadata = metadata;
            self
        }
    }

    #[async_trait::async_trait]
    impl ToolImpl for TestTool {
        type Args = Value;
        type Output = ToolOutput;
        type Error = std::io::Error;

        fn definition(&self) -> ToolDefinition {
            ToolDefinition::builder()
                .name(self.name)
                .description("test tool")
                .input_schema(self.schema.clone())
                .build()
        }

        fn metadata(&self) -> BTreeMap<String, Value> {
            self.metadata.clone()
        }

        async fn call(
            &self,
            _context: &ToolContext<'_>,
            _args: Value,
        ) -> Result<ToolOutput, std::io::Error> {
            unreachable!()
        }
    }

    fn registry(tools: Vec<TestTool>) -> ToolRegistry {
        let mut registry = ToolRegistry::default();
        for tool in tools {
            registry.register(tool).unwrap();
        }
        registry
    }

    #[test]
    fn aliases_and_sanitized_collisions_round_trip() {
        let registry = registry(vec![
            TestTool::new("list"),
            TestTool::new("class"),
            TestTool::new("9-lives"),
            TestTool::new("srv__get-user"),
            TestTool::new("srv__get_user"),
        ]);
        let catalog = Catalog::from_registry(&registry);

        for (python_name, wire_name) in [
            ("list_dir", "list"),
            ("class_", "class"),
            ("_9_lives", "9-lives"),
            ("srv__get_user", "srv__get-user"),
            ("srv__get_user_2", "srv__get_user"),
        ] {
            assert!(catalog.contains(python_name), "missing {python_name}");
            assert_eq!(
                catalog.bind(python_name, Vec::new(), BTreeMap::new()),
                Ok((wire_name.to_owned(), json!({})))
            );
        }
        assert!(!catalog.contains("list"));
        assert!(catalog.selection().contains("list"));
    }

    #[test]
    fn binding_is_required_first_and_rejects_bad_arguments() {
        let mut tool = TestTool::new("wire-name");
        tool.schema = json!({
            "type": "object",
            "properties": {
                "optional": {"type": "boolean"},
                "second": {"type": "integer"},
                "first": {"type": "string"}
            },
            "required": ["first", "second"]
        });
        let catalog = Catalog::from_registry(&registry(vec![tool]));

        let (name, input) = catalog
            .bind(
                "wire_name",
                vec![json!("value"), json!(2)],
                BTreeMap::from([("optional".to_owned(), json!(true))]),
            )
            .unwrap();
        assert_eq!(name, "wire-name");
        assert_eq!(
            input,
            json!({"first": "value", "second": 2, "optional": true})
        );

        assert!(
            catalog
                .bind(
                    "wire_name",
                    vec![json!("value")],
                    BTreeMap::from([("first".to_owned(), json!("again"))]),
                )
                .unwrap_err()
                .contains("duplicate argument 'first'")
        );
        assert!(
            catalog
                .bind("wire_name", Vec::new(), BTreeMap::new())
                .unwrap_err()
                .contains("missing required argument 'first'")
        );
        assert!(
            catalog
                .bind(
                    "wire_name",
                    Vec::new(),
                    BTreeMap::from([("bogus".to_owned(), json!(1))]),
                )
                .unwrap_err()
                .contains("unknown argument 'bogus'")
        );
        assert!(
            catalog
                .bind(
                    "wire_name",
                    vec![json!("value"), json!(2), json!(true), json!(4)],
                    BTreeMap::new(),
                )
                .unwrap_err()
                .contains("too many positional arguments")
        );
    }

    #[test]
    fn metadata_controls_model_and_code_visibility() {
        let registry = registry(vec![
            TestTool::new("ordinary"),
            TestTool::new("read_text").metadata(code_only_metadata()),
            TestTool::new("batch").metadata(model_only_metadata()),
            TestTool::new("code").metadata(model_only_metadata()),
            TestTool::new("other").metadata(model_only_metadata()),
        ]);
        let entries = registry.entries();
        let entry = |name| {
            entries
                .iter()
                .find(|entry| entry.definition.name == name)
                .unwrap()
        };

        for mode in [CodemodeMode::Off, CodemodeMode::Alongside] {
            assert!(model_visible(entry("ordinary"), mode));
            assert!(model_visible(entry("batch"), mode));
            assert!(!model_visible(entry("read_text"), mode));
        }
        assert!(model_visible(entry("code"), CodemodeMode::Exclusive));
        assert!(!model_visible(entry("other"), CodemodeMode::Exclusive));
        assert!(!model_visible(entry("read_text"), CodemodeMode::Exclusive));

        let catalog = Catalog::from_registry(&registry);
        assert!(catalog.contains("ordinary"));
        assert!(catalog.contains("read_text"));
        assert!(!catalog.contains("batch"));
        assert_eq!(catalog.uncallable_names(), ["batch", "code", "other"]);
        assert!(model_selection(&registry, CodemodeMode::Exclusive).contains("code"));
        assert!(!model_selection(&registry, CodemodeMode::Exclusive).contains("ordinary"));
    }
}
