//! `question` tool (A.5): structured ask-the-user tool. The model poses
//! one or more questions with predefined options; the tool parks on a
//! host-provided [`AskQuestions`] seam until the user answers, is
//! dismissed, or the ask times out. Ported from the reference
//! `craft-agent/src/tools/question.rs` + `plugins/question/`; the
//! interactive form lives in the TUI (`tui/question_form.rs`), and the
//! default asker is the reference's headless variant: answer dismissed.

use std::sync::Arc;

use rig_core::tool::{IntoToolOutput, PortableTool, ToolOutput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Result, invalid};

/// How long a question may wait for the user before the answer degrades
/// to dismissed. Shared with the permission prompt (reference: 30 min).
pub const ASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

const ANSWER_PREFIX: &str = "question:";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionSpec {
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
}

/// The user's answer to a `question` call: either dismissed outright, or
/// one list of picked labels per question (custom answers are free text
/// that matches no predefined label).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub dismissed: bool,
    #[serde(default)]
    pub answers: Vec<Vec<String>>,
}

/// Host seam: present `questions` to the user and resolve their answer.
/// The TUI implementation opens the question form; the default
/// ([`DismissAsk`]) is the headless fallback.
pub trait AskQuestions: Send + Sync {
    fn ask(&self, questions: Vec<QuestionSpec>) -> crate::run::BoxFuture<QuestionAnswer>;
}

/// Headless default: nobody is there to ask, so the question reads as
/// dismissed — the same degradation the reference's hostless sessions
/// produce.
pub struct DismissAsk;

impl AskQuestions for DismissAsk {
    fn ask(&self, _questions: Vec<QuestionSpec>) -> crate::run::BoxFuture<QuestionAnswer> {
        Box::pin(async {
            QuestionAnswer {
                dismissed: true,
                answers: vec![],
            }
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QuestionArgs {
    /// List of questions to ask the user
    pub questions: Vec<QuestionInput>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QuestionInput {
    /// The question text
    pub question: String,
    /// Short tab header for the question
    pub header: Option<String>,
    /// List of predefined options
    #[serde(default)]
    pub options: Vec<QuestionOptionInput>,
    /// Whether multiple options can be selected
    #[serde(default)]
    pub multi_select: bool,
    /// Accepted alias of `multi_select` (reference schema keeps both).
    #[serde(default)]
    pub multiple: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QuestionOptionInput {
    /// Option label
    pub label: String,
    /// Option description
    pub description: Option<String>,
}

#[derive(Debug)]
pub struct QuestionOutput {
    pub text: String,
}

impl IntoToolOutput for QuestionOutput {
    fn into_tool_output(
        self,
    ) -> std::result::Result<ToolOutput, rig_core::tool::ToolExecutionError> {
        Ok(ToolOutput::text(self.text))
    }
}

/// Ask the user questions during execution: gather preferences, clarify
/// ambiguity, or offer choices about direction. `custom` entry is always
/// available, so do not include catch-all options; put the recommended
/// option first with a "(Recommended)" suffix; answers come back as lists
/// of labels (`multiSelect: true` for multi-select).
#[derive(Clone)]
pub struct Question(pub Arc<dyn AskQuestions>);

pub fn parse_questions(input: &QuestionArgs) -> Result<Vec<QuestionSpec>> {
    if input.questions.is_empty() {
        return Err(invalid("at least one question is required"));
    }
    Ok(input
        .questions
        .iter()
        .map(|q| QuestionSpec {
            question: q.question.clone(),
            header: q.header.clone(),
            options: q
                .options
                .iter()
                .map(|o| QuestionOption {
                    label: o.label.clone(),
                    description: o.description.clone(),
                })
                .collect(),
            multi_select: q.multi_select || q.multiple,
        })
        .collect())
}

pub fn encode_answer(answer: &QuestionAnswer) -> String {
    let json = serde_json::to_string(answer).unwrap_or_else(|_| r#"{"dismissed":true}"#.into());
    format!("{ANSWER_PREFIX}{json}")
}

pub fn decode_answer(s: &str) -> Option<QuestionAnswer> {
    serde_json::from_str(s.strip_prefix(ANSWER_PREFIX)?).ok()
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
            match answer.answers.get(i).filter(|a| !a.is_empty()) {
                Some(labels) => {
                    for v in labels {
                        let indented = v.replace("\r\n", "\n").replace('\n', "\n  ");
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

impl PortableTool for Question {
    const NAME: &'static str = "question";
    type Args = QuestionArgs;
    type Output = QuestionOutput;
    type Error = rig_core::tool::ToolExecutionError;

    fn description(&self) -> String {
        "Use this tool when you need to ask the user questions during execution. This allows you to:\n\
         - Gather user preferences or requirements\n\
         - Clarify ambiguous instructions\n\
         - Get decisions on implementation choices as you work\n\
         - Offer choices to the user about what direction to take\n\n\
         Rules:\n\
         - `custom` enabled by default adds \"Type your own answer\" - don't include catch-all options.\n\
         - Answers returned as arrays of labels. Set `multiSelect: true` for multi-select.\n\
         - Put recommended option first with \"(Recommended)\" suffix."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(QuestionArgs))
            .expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output> {
        let questions = parse_questions(&args)?;
        let answer = self.0.ask(questions.clone()).await;
        Ok(QuestionOutput {
            text: format_answer_markdown(&questions, &answer),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scripted(std::sync::Mutex<Option<QuestionAnswer>>);

    impl AskQuestions for Scripted {
        fn ask(&self, _questions: Vec<QuestionSpec>) -> crate::run::BoxFuture<QuestionAnswer> {
            let answer = self.0.lock().unwrap().take().unwrap_or(QuestionAnswer {
                dismissed: true,
                answers: vec![],
            });
            Box::pin(async move { answer })
        }
    }

    fn questions() -> QuestionArgs {
        QuestionArgs {
            questions: vec![QuestionInput {
                question: "Pick one".into(),
                header: Some("Pick".into()),
                options: vec![
                    QuestionOptionInput {
                        label: "A".into(),
                        description: Some("first".into()),
                    },
                    QuestionOptionInput {
                        label: "B".into(),
                        description: None,
                    },
                ],
                multi_select: false,
                multiple: false,
            }],
        }
    }

    #[test]
    fn parses_single_question() {
        let qs = parse_questions(&questions()).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].question, "Pick one");
        assert_eq!(qs[0].options.len(), 2);
        assert!(!qs[0].multi_select);
    }

    #[test]
    fn multiple_alias_sets_multi_select() {
        let mut args = questions();
        args.questions[0].multiple = true;
        assert!(parse_questions(&args).unwrap()[0].multi_select);
    }

    #[test]
    fn rejects_empty_questions() {
        let args = QuestionArgs { questions: vec![] };
        assert!(parse_questions(&args).is_err());
    }

    #[test]
    fn formats_submit_answer() {
        let qs = parse_questions(&questions()).unwrap();
        let answer = QuestionAnswer {
            dismissed: false,
            answers: vec![vec!["A".into()]],
        };
        let md = format_answer_markdown(&qs, &answer);
        assert!(md.contains("**Q1.** Pick one"));
        assert!(md.contains("- A"));
    }

    #[test]
    fn formats_dismiss() {
        let answer = QuestionAnswer {
            dismissed: true,
            answers: vec![],
        };
        assert_eq!(
            format_answer_markdown(&[], &answer),
            "(question dismissed by user)"
        );
    }

    #[test]
    fn formats_missing_answer_slot() {
        let qs = parse_questions(&questions()).unwrap();
        let answer = QuestionAnswer {
            dismissed: false,
            answers: vec![],
        };
        assert!(format_answer_markdown(&qs, &answer).contains("- (no answer)"));
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
        assert_eq!(decoded.answers, answer.answers);
    }

    #[test]
    fn permission_answer_is_not_question_answer() {
        assert!(!is_question_answer("allow"));
        assert!(!is_question_answer("deny:reason"));
    }

    #[tokio::test]
    async fn tool_returns_markdown_of_the_answer() {
        let tool = Question(Arc::new(Scripted(std::sync::Mutex::new(Some(
            QuestionAnswer {
                dismissed: false,
                answers: vec![vec!["A".into()]],
            },
        )))));
        let out = tool.call(questions()).await.unwrap();
        assert!(out.text.contains("**Q1.** Pick one"));
        assert!(out.text.contains("- A"));
    }

    #[tokio::test]
    async fn tool_defaults_to_dismissed_without_a_host() {
        let tool = Question(Arc::new(DismissAsk));
        let out = tool.call(questions()).await.unwrap();
        assert_eq!(out.text, "(question dismissed by user)");
    }

    #[tokio::test]
    async fn tool_rejects_empty_questions() {
        let tool = Question(Arc::new(DismissAsk));
        assert!(tool.call(QuestionArgs { questions: vec![] }).await.is_err());
    }
}
