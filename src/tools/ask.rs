//! The `ask_user` tool: the model asks a question, and a front end answers it.
//!
//! The loop is a library and has no idea who -- or whether anybody -- is on the
//! other end of it. Asking is therefore a seam, not a capability: [`Asker`] is
//! implemented by whoever can reach a human (the CLI reads a line, the desktop
//! bridge asks the window and waits), and this tool is the model's way in.
//!
//! A question the model asks is a question somebody may never answer. That is
//! reported as a failed tool result rather than a pause that never ends, so the
//! loop can say what happened and carry on.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{required_str, Tool, ToolContext};
use crate::error::{Error, Result};

/// Tool name.
pub const ASK_USER_TOOL_NAME: &str = "ask_user";

/// Longest question passed on, in characters. A question is read by a person,
/// so the bound is what a prompt can show rather than what a model can write.
pub const MAX_QUESTION_CHARS: usize = 2000;

/// Longest answer kept, in characters. The answer goes into the conversation,
/// so it is bounded the way a tool result is.
pub const MAX_ANSWER_CHARS: usize = 2000;

/// Most suggested answers one question may carry.
pub const MAX_OPTIONS: usize = 6;

/// The half of the loop that can reach a person.
///
/// `None` means the question reached nobody: the front end is gone, or the turn
/// was stopped while it waited. Returning an empty answer is the same thing as
/// far as the model is concerned -- there is nothing to act on.
pub trait Asker: Send + Sync {
    /// Put `question` to the user, offering `options` as suggested answers, and
    /// wait for what they say.
    fn ask(&self, question: &str, options: &[String]) -> Option<String>;
}

/// The `ask_user` tool, holding whatever can reach a person.
pub struct AskUserTool {
    asker: Arc<dyn Asker>,
}

impl AskUserTool {
    /// A tool that asks through `asker`.
    pub fn new(asker: Arc<dyn Asker>) -> Self {
        AskUserTool { asker }
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        ASK_USER_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Ask the user a question and wait for their answer. Use it when the \
         decision is theirs to make or a detail cannot be read out of the \
         workspace; do not use it to confirm something you were already told."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to put to the user, in one or two sentences."
                },
                "options": {
                    "type": "array",
                    "maxItems": MAX_OPTIONS,
                    "items": { "type": "string", "minLength": 1 },
                    "description": "Suggested answers. The user may still answer in their own words."
                }
            },
            "required": ["question"]
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        let question = required_str(&input, "question")?.trim();
        if question.is_empty() {
            return Err(tool_error("a question cannot be empty".to_string()));
        }
        if question.chars().count() > MAX_QUESTION_CHARS {
            return Err(tool_error(format!(
                "a question cannot be longer than {MAX_QUESTION_CHARS} characters"
            )));
        }
        let options = options_of(&input)?;

        match self.asker.ask(question, &options) {
            Some(answer) if !answer.trim().is_empty() => {
                Ok(super::truncate_chars(answer.trim(), MAX_ANSWER_CHARS))
            }
            // Nobody answered: the person closed the window, the turn was
            // stopped, or the question was left alone too long. The model is
            // told so rather than left holding an empty answer.
            _ => Err(tool_error(
                "the user did not answer (there was nobody to answer, or the turn was stopped \
                 while the question waited)"
                    .to_string(),
            )),
        }
    }
}

/// Read the suggested answers, refusing more than a prompt can show.
fn options_of(input: &Value) -> Result<Vec<String>> {
    let Some(value) = input.get("options") else {
        return Ok(Vec::new());
    };
    let Some(list) = value.as_array() else {
        return Err(tool_error(
            "`options` must be an array of strings".to_string(),
        ));
    };
    if list.len() > MAX_OPTIONS {
        return Err(tool_error(format!(
            "at most {MAX_OPTIONS} suggested answers are accepted"
        )));
    }
    let mut options = Vec::with_capacity(list.len());
    for entry in list {
        let Some(text) = entry.as_str() else {
            return Err(tool_error(
                "`options` must be an array of strings".to_string(),
            ));
        };
        let text = text.trim();
        if text.is_empty() {
            return Err(tool_error("a suggested answer cannot be empty".to_string()));
        }
        options.push(text.to_string());
    }
    Ok(options)
}

fn tool_error(message: String) -> Error {
    Error::Tool {
        tool: ASK_USER_TOOL_NAME.to_string(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// An asker that answers with whatever it was given, and remembers what it
    /// was asked.
    struct Scripted {
        answer: Option<String>,
        asked: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl Scripted {
        fn answering(answer: &str) -> Arc<Scripted> {
            Arc::new(Scripted {
                answer: Some(answer.to_string()),
                asked: Mutex::new(Vec::new()),
            })
        }

        fn silent() -> Arc<Scripted> {
            Arc::new(Scripted {
                answer: None,
                asked: Mutex::new(Vec::new()),
            })
        }
    }

    impl Asker for Scripted {
        fn ask(&self, question: &str, options: &[String]) -> Option<String> {
            self.asked
                .lock()
                .unwrap()
                .push((question.to_string(), options.to_vec()));
            self.answer.clone()
        }
    }

    fn context() -> ToolContext {
        ToolContext::new(std::env::temp_dir())
    }

    #[tokio::test]
    async fn the_answer_is_the_tool_result() {
        let asker = Scripted::answering("the second one");
        let tool = AskUserTool::new(Arc::clone(&asker) as Arc<dyn Asker>);
        let output = tool
            .call(
                &context(),
                json!({ "question": "which?", "options": ["first", "second"] }),
            )
            .await
            .unwrap();
        assert_eq!(output, "the second one");

        let asked = asker.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].0, "which?");
        assert_eq!(asked[0].1, vec!["first".to_string(), "second".to_string()]);
    }

    #[tokio::test]
    async fn a_question_nobody_answered_is_a_failed_result() {
        let tool = AskUserTool::new(Scripted::silent() as Arc<dyn Asker>);
        let error = tool
            .call(&context(), json!({ "question": "anybody there?" }))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("did not answer"), "{error}");
    }

    #[tokio::test]
    async fn a_blank_answer_counts_as_no_answer() {
        let tool = AskUserTool::new(Scripted::answering("   ") as Arc<dyn Asker>);
        assert!(tool
            .call(&context(), json!({ "question": "which?" }))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn the_question_is_required_and_bounded() {
        let tool = AskUserTool::new(Scripted::answering("yes") as Arc<dyn Asker>);
        assert!(tool.call(&context(), json!({})).await.is_err());
        assert!(tool
            .call(&context(), json!({ "question": "   " }))
            .await
            .is_err());
        let long = "x".repeat(MAX_QUESTION_CHARS + 1);
        assert!(tool
            .call(&context(), json!({ "question": long }))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn suggested_answers_are_checked() {
        let tool = AskUserTool::new(Scripted::answering("yes") as Arc<dyn Asker>);
        // Not an array, an empty entry, and one too many.
        assert!(tool
            .call(&context(), json!({ "question": "q", "options": "yes" }))
            .await
            .is_err());
        assert!(tool
            .call(
                &context(),
                json!({ "question": "q", "options": ["yes", " "] })
            )
            .await
            .is_err());
        let many: Vec<String> = (0..=MAX_OPTIONS).map(|n| format!("option {n}")).collect();
        assert!(tool
            .call(&context(), json!({ "question": "q", "options": many }))
            .await
            .is_err());
    }

    #[test]
    fn the_schema_names_the_tool_and_what_it_needs() {
        let tool = AskUserTool::new(Scripted::answering("yes") as Arc<dyn Asker>);
        assert_eq!(tool.name(), ASK_USER_TOOL_NAME);
        let schema = tool.input_schema();
        assert_eq!(schema["required"], json!(["question"]));
        assert_eq!(
            schema["properties"]["options"]["maxItems"],
            json!(MAX_OPTIONS)
        );
    }
}
