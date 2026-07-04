//! JSON Schemas for Flow stage documents, expressed as `serde_json::Value`
//! constants. Each stage's `output_schema` is one of these. Mirrors the
//! proposal's §2.1 document shapes.

use serde_json::{Value, json};

fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
    })
}

/// Goal/TPM document: the restated goal, scope, and acceptance criteria.
pub fn goal_doc() -> Value {
    object(
        json!({
            "goal": {"type": "string"},
            "scope": {"type": "string"},
            "out_of_scope": {"type": "array", "items": {"type": "string"}},
            "acceptance_criteria": {
                "type": "array",
                "items": {"type": "string"},
                "minItems": 1
            }
        }),
        &["goal", "scope", "acceptance_criteria"],
    )
}

/// Plan document: chunks of work with optional dependency edges forming a DAG.
/// The array order is the plan's suggested topological order; `depends_on`
/// declares the actual edges so the orchestrator can schedule correctly and
/// the TUI can render the real graph.
pub fn plan_doc() -> Value {
    object(
        json!({
            "summary": {"type": "string"},
            "chunks": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "id": {"type": "string"},
                        "title": {"type": "string"},
                        "files": {"type": "array", "items": {"type": "string"}},
                        "description": {"type": "string"},
                        "depends_on": {"type": "array", "items": {"type": "string"}}
                    },
                    "required": ["id", "title", "description"]
                },
                "minItems": 1
            }
        }),
        &["summary", "chunks"],
    )
}

/// Requirement document for a single chunk.
pub fn requirement_doc() -> Value {
    object(
        json!({
            "chunk_id": {"type": "string"},
            "spec": {"type": "string"},
            "constraints": {"type": "array", "items": {"type": "string"}},
            "verification_steps": {"type": "array", "items": {"type": "string"}}
        }),
        &["chunk_id", "spec"],
    )
}

/// Review report for a chunk: pass/fail plus findings. Mirrors `qa_report` so
/// the review re-Execute gate is driven by a structured `status` field rather
/// than lexical `[P0]`/`[P1]` substring scanning (which silently let blockers
/// through whenever the review subagent phrased severity any other way).
pub fn review_report() -> Value {
    object(
        json!({
            "chunk_id": {"type": "string"},
            "status": {"type": "string", "enum": ["pass", "fail"]},
            "findings": {"type": "array", "items": {"type": "string"}}
        }),
        &["chunk_id", "status"],
    )
}

/// QA report for a chunk: pass/fail plus findings.
pub fn qa_report() -> Value {
    object(
        json!({
            "chunk_id": {"type": "string"},
            "status": {"type": "string", "enum": ["pass", "fail"]},
            "findings": {"type": "array", "items": {"type": "string"}}
        }),
        &["chunk_id", "status"],
    )
}

/// Integration checkpoint after merging chunks.
pub fn integration_checkpoint() -> Value {
    object(
        json!({
            "status": {"type": "string", "enum": ["integrated", "conflicts"]},
            "conflicts": {"type": "array", "items": {"type": "string"}},
            "notes": {"type": "string"}
        }),
        &["status"],
    )
}

/// Final verification report against acceptance criteria.
pub fn verification_report() -> Value {
    object(
        json!({
            "goal_met": {"type": "boolean"},
            "met_criteria": {"type": "array", "items": {"type": "string"}},
            "unmet_criteria": {"type": "array", "items": {"type": "string"}},
            "verdict": {"type": "string", "enum": ["ship", "block"]},
            "summary": {"type": "string"}
        }),
        &["goal_met", "verdict"],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_doc_is_valid_object_schema() {
        let s = goal_doc();
        assert_eq!(s["type"], "object");
        assert!(s["properties"]["goal"]["type"] == "string");
        assert!(s["required"].as_array().unwrap().len() >= 3);
    }

    #[test]
    fn plan_doc_requires_chunks() {
        let s = plan_doc();
        let required: Vec<&str> = s["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"chunks"));
    }

    #[test]
    fn verification_report_has_verdict_enum() {
        let s = verification_report();
        let verdict = &s["properties"]["verdict"]["enum"];
        assert!(verdict.as_array().unwrap().len() == 2);
    }

    #[test]
    fn qa_report_status_enum_is_pass_fail() {
        let s = qa_report();
        let status = &s["properties"]["status"]["enum"];
        assert_eq!(status.as_array().unwrap().len(), 2);
    }

    #[test]
    fn review_report_status_enum_is_pass_fail() {
        let s = review_report();
        let status = &s["properties"]["status"]["enum"];
        assert_eq!(status.as_array().unwrap().len(), 2);
        let required: Vec<&str> = s["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"chunk_id"));
        assert!(required.contains(&"status"));
    }
}
