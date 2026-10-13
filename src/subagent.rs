//! Subagents: run a nested agent loop with fresh context.
//!
//! The `task` tool drives a second [`Agent`] with a brand-new message list.
//! It shares the working directory but only its final text returns to the
//! parent conversation. The subagent is built without a `task` tool, so it
//! cannot delegate again.
//!
//! ```text
//!   Parent agent                     Subagent
//!   +------------------+             +------------------+
//!   | messages = [...] |   task      | messages=[prompt]|
//!   | tool: task       | ----------> | own agent loop   |
//!   | tool_result      | <---------- | final text       |
//!   +------------------+             +------------------+
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agent::Agent;
use crate::error::{Error, Result};
use crate::tools::{required_str, Tool, ToolContext};

/// Tool name the parent uses to delegate.
pub const TASK_TOOL_NAME: &str = "task";

/// Default system prompt for a subagent.
pub fn subagent_system_prompt() -> String {
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let environment = if cfg!(windows) {
        "Windows: the bash tool runs through cmd.exe; use cmd.exe syntax, not Unix Bash or PowerShell syntax, and prefer the dedicated file tools."
    } else {
        "Unix-like: the bash tool runs the system shell."
    };
    format!(
        "You are a coding agent at {cwd}. Environment: {environment} Complete the given task, then return a concise final answer."
    )
}

/// The `task` tool: runs a subagent and returns only its final text.
pub struct TaskTool {
    subagent: Arc<Agent>,
}

impl TaskTool {
    /// Wrap an agent as a subagent tool.
    pub fn new(subagent: Agent) -> Self {
        TaskTool {
            subagent: Arc::new(subagent),
        }
    }

    /// Wrap an already-shared agent.
    pub fn shared(subagent: Arc<Agent>) -> Self {
        TaskTool { subagent }
    }

    /// The subagent this tool drives.
    pub fn subagent(&self) -> &Agent {
        &self.subagent
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        TASK_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Run a subagent with a fresh conversation context and return its final text. \
         Useful for focused exploration or a self-contained subtask."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "minLength": 1, "description": "What the subagent should do." }
            },
            "required": ["prompt"]
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        let prompt = required_str(&input, "prompt")?;
        match self.subagent.ask(prompt).await {
            Ok(answer) if answer.trim().is_empty() => Ok("(no summary)".to_string()),
            Ok(answer) => Ok(answer),
            Err(Error::MaxIterations(limit)) => Ok(format!(
                "Subagent stopped after {limit} turns without a final answer."
            )),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::llm::{CompletionRequest, CompletionResponse, LlmClient};
    use crate::message::{ContentBlock, ToolCall};
    use crate::tools::{ToolContext, ToolRegistry};

    /// Replays scripted responses and records the size of each request's
    /// message list (so we can prove the subagent starts fresh).
    struct ScriptedClient {
        responses: Arc<Mutex<VecDeque<CompletionResponse>>>,
        message_counts: Arc<Mutex<Vec<usize>>>,
    }

    impl ScriptedClient {
        fn new(responses: Vec<CompletionResponse>) -> Self {
            ScriptedClient {
                responses: Arc::new(Mutex::new(responses.into())),
                message_counts: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn complete(&self, request: CompletionRequest<'_>) -> Result<CompletionResponse> {
            self.message_counts
                .lock()
                .unwrap()
                .push(request.messages.len());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::Config("no scripted response left".into()))
        }
    }

    /// A tool that echoes its `value` argument back.
    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo a value."
        }
        fn input_schema(&self) -> Value {
            json!({ "type": "object", "properties": { "value": { "type": "string" } } })
        }
        async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
            Ok(input
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string())
        }
    }

    fn echo_registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        registry
    }

    fn response(content: Vec<ContentBlock>, stop_reason: &str) -> CompletionResponse {
        CompletionResponse {
            content,
            stop_reason: Some(stop_reason.to_string()),
            model: None,
            usage: None,
        }
    }

    fn tool_use(id: &str, name: &str, input: Value) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    #[tokio::test]
    async fn task_tool_runs_subagent_with_fresh_context() {
        let scripted = Arc::new(ScriptedClient::new(vec![
            response(
                vec![tool_use("s1", "echo", json!({ "value": "probe" }))],
                "tool_use",
            ),
            response(vec![ContentBlock::text("sub answer")], "end_turn"),
        ]));
        let client: Arc<dyn LlmClient> = scripted.clone();

        let subagent = Agent::builder()
            .shared_client(client)
            .tools(echo_registry())
            .workspace(std::env::temp_dir())
            .max_steps(5)
            .build();

        let mut registry = ToolRegistry::new();
        registry.register(TaskTool::new(subagent));

        let ctx = ToolContext::new(std::env::temp_dir());
        let call = ToolCall {
            id: "t1".into(),
            name: TASK_TOOL_NAME.into(),
            input: json!({ "prompt": "explore the repo" }),
        };
        let output = registry.execute(&ctx, &call).await.unwrap();

        assert_eq!(output, "sub answer");
        // The subagent's first request carried a single, prompt-only message.
        assert_eq!(scripted.message_counts.lock().unwrap()[0], 1);
    }

    #[tokio::test]
    async fn task_tool_reports_when_subagent_hits_its_limit() {
        let scripted = Arc::new(ScriptedClient::new(vec![
            response(
                vec![tool_use("s1", "echo", json!({ "value": "x" }))],
                "tool_use",
            ),
            response(
                vec![tool_use("s2", "echo", json!({ "value": "y" }))],
                "tool_use",
            ),
            response(
                vec![tool_use("s3", "echo", json!({ "value": "z" }))],
                "tool_use",
            ),
        ]));
        let client: Arc<dyn LlmClient> = scripted.clone();

        let subagent = Agent::builder()
            .shared_client(client)
            .tools(echo_registry())
            .workspace(std::env::temp_dir())
            .max_steps(2)
            .build();

        let mut registry = ToolRegistry::new();
        registry.register(TaskTool::new(subagent));

        let ctx = ToolContext::new(std::env::temp_dir());
        let call = ToolCall {
            id: "t1".into(),
            name: TASK_TOOL_NAME.into(),
            input: json!({ "prompt": "loop forever" }),
        };
        let output = registry.execute(&ctx, &call).await.unwrap();

        assert!(output.contains("stopped after 2 turns"), "{output}");
    }

    #[test]
    fn base_tools_cannot_delegate() {
        assert!(ToolRegistry::base().get(TASK_TOOL_NAME).is_none());
        assert!(ToolRegistry::with_defaults().get(TASK_TOOL_NAME).is_none());
    }

    #[test]
    fn subagent_prompt_is_not_empty() {
        assert!(!subagent_system_prompt().is_empty());
    }
}
