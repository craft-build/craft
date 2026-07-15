use serde::{Deserialize, Serialize};

use crate::types::{QuestionOption, QuestionSpec};

const ANSWER_PREFIX: &str = "question:";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub dismissed: bool,
    #[serde(default)]
    pub answers: Vec<Vec<String>>,
}

pub fn parse_questions(input: &serde_json::Value) -> Result<Vec<QuestionSpec>, String> {
    let arr = input
        .get("questions")
        .and_then(|v| v.as_array())
        .ok_or("missing questions array")?;
    if arr.is_empty() {
        return Err("at least one question is required".to_string());
    }
    let mut out = Vec::with_capacity(arr.len());
    for q in arr {
        let question = q
            .get("question")
            .and_then(|v| v.as_str())
            .ok_or("each question needs a `question` field")?
            .to_string();
        let header = q
            .get("header")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let multi_select = q
            .get("multiSelect")
            .or_else(|| q.get("multiple"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let options = q
            .get("options")
            .and_then(|v| v.as_array())
            .map(|opts| {
                opts.iter()
                    .filter_map(|o| {
                        let label = o.get("label")?.as_str()?.to_string();
                        let description = o
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        Some(QuestionOption { label, description })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(QuestionSpec {
            question,
            header,
            options,
            multi_select,
        });
    }
    Ok(out)
}

pub fn encode_answer(answer: &QuestionAnswer) -> String {
    let json = serde_json::to_string(answer).unwrap_or_else(|_| r#"{"dismissed":true}"#.into());
    format!("{ANSWER_PREFIX}{json}")
}

pub fn decode_answer(s: &str) -> Option<QuestionAnswer> {
    let json = s.strip_prefix(ANSWER_PREFIX)?;
    serde_json::from_str(json).ok()
}

pub fn is_question_answer(s: &str) -> bool {
    s.starts_with(ANSWER_PREFIX)
}

pub fn format_answer_markdown(questions: &[QuestionSpec], answer: &QuestionAnswer) -> String {
    if answer.dismissed {
        return "(question dismissed by user)".to_string();
    }
    let blocks: Vec<String> = questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let mut lines = vec![format!("**Q{}.** {}", i + 1, q.question)];
            lines.push(format!("**A{}.**", i + 1));
            let ans = answer.answers.get(i).filter(|a| !a.is_empty());
            match ans {
                Some(labels) => {
                    for v in labels {
                        let indented = v.replace("\r\n", "\n").replace("\n", "\n  ");
                        lines.push(format!("- {indented}"));
                    }
                }
                None => lines.push("- (no answer)".to_string()),
            }
            lines.join("\n")
        })
        .collect();
    blocks.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_single_question() {
        let input = json!({
            "questions": [{
                "question": "Pick one",
                "options": [
                    { "label": "A", "description": "first" },
                    { "label": "B" }
                ],
                "multiSelect": false
            }]
        });
        let qs = parse_questions(&input).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].question, "Pick one");
        assert_eq!(qs[0].options.len(), 2);
        assert!(!qs[0].multi_select);
    }

    #[test]
    fn rejects_empty_questions() {
        let input = json!({ "questions": [] });
        assert!(parse_questions(&input).is_err());
    }

    #[test]
    fn formats_submit_answer() {
        let qs = vec![QuestionSpec {
            question: "Which?".into(),
            header: None,
            options: vec![QuestionOption {
                label: "X".into(),
                description: None,
            }],
            multi_select: false,
        }];
        let answer = QuestionAnswer {
            dismissed: false,
            answers: vec![vec!["X".into()]],
        };
        let md = format_answer_markdown(&qs, &answer);
        assert!(md.contains("**Q1.** Which?"));
        assert!(md.contains("- X"));
    }

    #[test]
    fn formats_dismiss() {
        let answer = QuestionAnswer {
            dismissed: true,
            answers: vec![],
        };
        let md = format_answer_markdown(&[], &answer);
        assert_eq!(md, "(question dismissed by user)");
    }

    #[test]
    fn answer_round_trip() {
        let answer = QuestionAnswer {
            dismissed: false,
            answers: vec![vec!["A".into(), "B".into()], vec![]],
        };
        let encoded = encode_answer(&answer);
        assert!(is_question_answer(&encoded));
        let decoded = decode_answer(&encoded).unwrap();
        assert!(!decoded.dismissed);
        assert_eq!(decoded.answers.len(), 2);
        assert_eq!(decoded.answers[0], vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn permission_answer_is_not_question_answer() {
        assert!(!is_question_answer("allow"));
        assert!(!is_question_answer("deny:reason"));
    }
}
