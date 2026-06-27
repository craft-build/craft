use std::collections::HashMap;
use std::path::{Path, PathBuf};

use minijinja::Environment;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

/// Supported recipe parameter types.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ParamType {
    String,
    Number,
    Boolean,
    Date,
    File,
    Select,
}

fn default_param_type() -> ParamType {
    ParamType::String
}

#[derive(Debug, Clone, Deserialize)]
pub struct Parameter {
    pub name: String,
    #[serde(rename = "type", default = "default_param_type")]
    pub kind: ParamType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
    pub description: Option<String>,
    #[serde(default)]
    pub options: Vec<Value>,
}

/// A declarative, parameterized session blueprint loaded from YAML or JSON.
#[derive(Debug, Clone, Deserialize)]
pub struct Recipe {
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Vec<Parameter>,
    pub instructions: String,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
}

#[derive(Debug, Error)]
pub enum RecipeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("template: {0}")]
    Template(String),
    #[error("missing required parameter: {0}")]
    MissingRequired(String),
    #[error("invalid value for parameter {param}: {message}")]
    InvalidValue { param: String, message: String },
}

pub fn load(path: &Path) -> Result<Recipe, RecipeError> {
    let content = std::fs::read_to_string(path)?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "json" => Ok(serde_json::from_str(&content)?),
        _ => Ok(serde_yaml::from_str(&content)?),
    }
}

impl Recipe {
    /// Parameters that are required, have no default, and are absent from overrides.
    pub fn missing_required<'a>(
        &'a self,
        overrides: &HashMap<String, String>,
    ) -> Vec<&'a Parameter> {
        self.parameters
            .iter()
            .filter(|p| p.required && p.default.is_none() && !overrides.contains_key(&p.name))
            .collect()
    }

    /// Resolve all parameters from overrides, defaults, or error on missing required.
    pub fn resolve_parameters(
        &self,
        overrides: &HashMap<String, String>,
    ) -> Result<HashMap<String, Value>, RecipeError> {
        let mut params = HashMap::new();
        for p in &self.parameters {
            let value = if let Some(v) = overrides.get(&p.name) {
                coerce(&p.kind, v).map_err(|m| RecipeError::InvalidValue {
                    param: p.name.clone(),
                    message: m,
                })?
            } else if let Some(d) = &p.default {
                d.clone()
            } else if p.required {
                return Err(RecipeError::MissingRequired(p.name.clone()));
            } else {
                continue;
            };
            if p.kind == ParamType::Select
                && !p.options.is_empty()
                && !p.options.iter().any(|o| option_matches(o, &value))
            {
                return Err(RecipeError::InvalidValue {
                    param: p.name.clone(),
                    message: "value not in options".to_string(),
                });
            }
            params.insert(p.name.clone(), value);
        }
        Ok(params)
    }

    /// Render the instructions template with the resolved parameters. Supports
    /// `{% include "relative/path" %}` of other recipe files (their instructions
    /// are rendered) or plain partials, resolved relative to the recipe's directory.
    pub fn render(
        &self,
        params: &HashMap<String, Value>,
        recipe_path: &Path,
    ) -> Result<String, RecipeError> {
        let recipe_dir: PathBuf = recipe_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut env = Environment::new();
        env.set_loader(move |name| {
            let p = recipe_dir.join(name);
            if !p.is_file() {
                return Ok(None);
            }
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "yaml" | "yml" | "json")
                && let Ok(sub) = load(&p)
            {
                return Ok(Some(sub.instructions));
            }
            match std::fs::read_to_string(&p) {
                Ok(c) => Ok(Some(c)),
                Err(_) => Ok(None),
            }
        });
        let template = env
            .template_from_str(&self.instructions)
            .map_err(|e| RecipeError::Template(e.to_string()))?;
        let context: minijinja::Value = params
            .iter()
            .map(|(k, v)| (minijinja::Value::from(k.as_str()), to_mj_value(v)))
            .collect();
        template
            .render(context)
            .map_err(|e| RecipeError::Template(e.to_string()))
    }
}

fn to_mj_value(v: &Value) -> minijinja::Value {
    match v {
        Value::Null => minijinja::Value::UNDEFINED,
        Value::Bool(b) => minijinja::Value::from(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                minijinja::Value::from(i)
            } else if let Some(u) = n.as_u64() {
                minijinja::Value::from(u)
            } else {
                minijinja::Value::from(n.as_f64().unwrap_or_default())
            }
        }
        Value::String(s) => minijinja::Value::from(s.as_str()),
        Value::Array(a) => a.iter().map(to_mj_value).collect::<minijinja::Value>(),
        Value::Object(o) => o
            .iter()
            .map(|(k, val)| (minijinja::Value::from(k.as_str()), to_mj_value(val)))
            .collect::<minijinja::Value>(),
    }
}

fn coerce(kind: &ParamType, s: &str) -> Result<Value, String> {
    match kind {
        ParamType::Number => {
            let f: f64 = s.parse().map_err(|_| format!("'{s}' is not a number"))?;
            Ok(if f.fract() == 0.0 && f.abs() < (i64::MAX as f64) {
                Value::from(f as i64)
            } else {
                Value::from(f)
            })
        }
        ParamType::Boolean => match s.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Ok(Value::Bool(true)),
            "false" | "no" | "0" => Ok(Value::Bool(false)),
            _ => Err(format!("'{s}' is not a boolean")),
        },
        _ => Ok(Value::String(s.to_string())),
    }
}

fn option_matches(option: &Value, value: &Value) -> bool {
    if option == value {
        return true;
    }
    let to_str = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    to_str(option) == to_str(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn load_yaml_recipe() {
        let tmp = TempDir::new().unwrap();
        let p = write(
            tmp.path(),
            "audit.yaml",
            "name: audit\ninstructions: \"hello {{ focus }}\"\nparameters:\n  - name: focus\n    type: string\n    default: security\n",
        );
        let recipe = load(&p).unwrap();
        assert_eq!(recipe.name.as_deref(), Some("audit"));
        assert_eq!(recipe.parameters.len(), 1);
        assert_eq!(recipe.parameters[0].kind, ParamType::String);
    }

    #[test]
    fn load_json_recipe() {
        let tmp = TempDir::new().unwrap();
        let p = write(
            tmp.path(),
            "r.json",
            r#"{"name":"r","instructions":"x","parameters":[]}"#,
        );
        let recipe = load(&p).unwrap();
        assert_eq!(recipe.name.as_deref(), Some("r"));
    }

    #[test]
    fn resolve_uses_default_when_no_override() {
        let recipe = Recipe {
            parameters: vec![Parameter {
                name: "focus".into(),
                kind: ParamType::String,
                required: false,
                default: Some(Value::String("security".into())),
                description: None,
                options: vec![],
            }],
            ..minimal()
        };
        let params = recipe.resolve_parameters(&HashMap::new()).unwrap();
        assert_eq!(
            params.get("focus").unwrap(),
            &Value::String("security".into())
        );
    }

    #[test]
    fn resolve_override_takes_precedence() {
        let recipe = Recipe {
            parameters: vec![Parameter {
                name: "focus".into(),
                kind: ParamType::String,
                required: false,
                default: Some(Value::String("security".into())),
                description: None,
                options: vec![],
            }],
            ..minimal()
        };
        let mut overrides = HashMap::new();
        overrides.insert("focus".into(), "performance".into());
        let params = recipe.resolve_parameters(&overrides).unwrap();
        assert_eq!(
            params.get("focus").unwrap(),
            &Value::String("performance".into())
        );
    }

    #[test]
    fn resolve_coerces_number_and_boolean() {
        let recipe = Recipe {
            parameters: vec![
                Parameter {
                    name: "depth".into(),
                    kind: ParamType::Number,
                    required: false,
                    default: None,
                    description: None,
                    options: vec![],
                },
                Parameter {
                    name: "verbose".into(),
                    kind: ParamType::Boolean,
                    required: false,
                    default: None,
                    description: None,
                    options: vec![],
                },
            ],
            ..minimal()
        };
        let mut overrides = HashMap::new();
        overrides.insert("depth".into(), "3".into());
        overrides.insert("verbose".into(), "yes".into());
        let params = recipe.resolve_parameters(&overrides).unwrap();
        assert_eq!(params.get("depth").unwrap(), &Value::from(3_i64));
        assert_eq!(params.get("verbose").unwrap(), &Value::Bool(true));
    }

    #[test]
    fn resolve_errors_on_missing_required() {
        let recipe = Recipe {
            parameters: vec![Parameter {
                name: "must".into(),
                kind: ParamType::String,
                required: true,
                default: None,
                description: None,
                options: vec![],
            }],
            ..minimal()
        };
        let err = recipe.resolve_parameters(&HashMap::new()).unwrap_err();
        assert!(matches!(err, RecipeError::MissingRequired(_)));
    }

    #[test]
    fn resolve_validates_select_options() {
        let recipe = Recipe {
            parameters: vec![Parameter {
                name: "level".into(),
                kind: ParamType::Select,
                required: false,
                default: Some(Value::String("shallow".into())),
                description: None,
                options: vec![
                    Value::String("shallow".into()),
                    Value::String("deep".into()),
                ],
            }],
            ..minimal()
        };
        let mut overrides = HashMap::new();
        overrides.insert("level".into(), "bogus".into());
        let err = recipe.resolve_parameters(&overrides).unwrap_err();
        assert!(matches!(err, RecipeError::InvalidValue { .. }));
    }

    #[test]
    fn missing_required_lists_unset_required() {
        let recipe = Recipe {
            parameters: vec![
                Parameter {
                    name: "a".into(),
                    kind: ParamType::String,
                    required: true,
                    default: None,
                    description: None,
                    options: vec![],
                },
                Parameter {
                    name: "b".into(),
                    kind: ParamType::String,
                    required: true,
                    default: Some(Value::String("d".into())),
                    description: None,
                    options: vec![],
                },
            ],
            ..minimal()
        };
        let missing = recipe.missing_required(&HashMap::new());
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].name, "a");
    }

    #[test]
    fn render_substitutes_parameters() {
        let tmp = TempDir::new().unwrap();
        let p = write(
            tmp.path(),
            "r.yaml",
            "instructions: \"Audit {{ focus }} at depth {{ depth }}\"\nparameters:\n  - name: focus\n    type: string\n    default: security\n  - name: depth\n    type: number\n    default: 1\n",
        );
        let recipe = load(&p).unwrap();
        let params = recipe.resolve_parameters(&HashMap::new()).unwrap();
        let out = recipe.render(&params, &p).unwrap();
        assert_eq!(out, "Audit security at depth 1");
    }

    #[test]
    fn render_includes_subrecipe() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "sub.yaml",
            "instructions: \"Sub-step for {{ focus }}\"\nparameters: []\n",
        );
        let p = write(
            tmp.path(),
            "main.yaml",
            "instructions: \"Main. {% include 'sub.yaml' %}\"\nparameters:\n  - name: focus\n    type: string\n    default: security\n",
        );
        let recipe = load(&p).unwrap();
        let params = recipe.resolve_parameters(&HashMap::new()).unwrap();
        let out = recipe.render(&params, &p).unwrap();
        assert_eq!(out, "Main. Sub-step for security");
    }

    #[test]
    fn render_supports_conditionals() {
        let tmp = TempDir::new().unwrap();
        let p = write(
            tmp.path(),
            "r.yaml",
            "instructions: \"{% if verbose %}verbose{% else %}quiet{% endif %}\"\nparameters:\n  - name: verbose\n    type: boolean\n    default: true\n",
        );
        let recipe = load(&p).unwrap();
        let params = recipe.resolve_parameters(&HashMap::new()).unwrap();
        let out = recipe.render(&params, &p).unwrap();
        assert_eq!(out, "verbose");
    }

    fn minimal() -> Recipe {
        Recipe {
            name: None,
            description: None,
            parameters: vec![],
            instructions: String::new(),
            model: None,
            max_turns: None,
        }
    }
}
