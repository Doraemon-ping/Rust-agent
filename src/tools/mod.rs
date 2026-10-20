//! Tool abstraction and the built-in coding tools.
//!
//! The loop is deliberately ignorant of what a tool does: it asks the
//! registry for [`ToolDefinition`]s, and dispatches [`ToolCall`]s back by
//! name. Adding a capability means implementing [`Tool`] and registering it.

pub mod ask;
pub mod bash;
pub mod fs;
pub mod skill;
pub mod task;
pub mod todo;

pub use ask::{
    AskUserTool, Asker, ASK_USER_TOOL_NAME, MAX_ANSWER_CHARS, MAX_OPTIONS, MAX_QUESTION_CHARS,
};
pub use bash::BashTool;
pub use fs::{EditFileTool, GlobTool, ReadFileTool, WriteFileTool};
pub use skill::{LoadSkillTool, LOAD_SKILL_TOOL_NAME};
pub use task::{
    is_valid_task_id, register_task_tools, ClaimTaskTool, CompleteTaskTool, CreateTaskTool,
    GetTaskTool, ListTasksTool, Task, TaskStatus, TaskStore, UpdateTaskTool, CLAIM_TASK_TOOL_NAME,
    COMPLETE_TASK_TOOL_NAME, CREATE_TASK_TOOL_NAME, DEFAULT_OWNER, GET_TASK_TOOL_NAME,
    LIST_TASKS_TOOL_NAME, TASKS_DIR, TASKS_PROMPT, UPDATE_TASK_TOOL_NAME,
};
pub use todo::{Todo, TodoList, TodoStatus, TodoTool, TODO_TOOL_NAME};

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::llm::ToolDefinition;
use crate::message::ToolCall;

/// Everything a tool needs beyond its own arguments.
#[derive(Debug, Clone)]
pub struct ToolContext {
    workspace: PathBuf,
}

impl ToolContext {
    /// Create a context rooted at `workspace`.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        ToolContext {
            workspace: normalize(&workspace.into()),
        }
    }

    /// The sandbox root shared by every file operation.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Resolve a model-supplied path, refusing anything that escapes the
    /// workspace. Relative paths are taken from the workspace root.
    pub fn resolve(&self, path: &str) -> Result<PathBuf> {
        let resolved = normalize(&self.workspace.join(path));
        if resolved.starts_with(&self.workspace) {
            Ok(resolved)
        } else {
            Err(Error::PathEscape(path.to_string()))
        }
    }
}

/// A capability the model can invoke.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique name the model calls this tool by.
    fn name(&self) -> &str;
    /// Human-readable description shown to the model.
    fn description(&self) -> &str;
    /// JSON Schema describing the arguments.
    fn input_schema(&self) -> Value;
    /// Run the tool.
    ///
    /// Returning `Err` is surfaced to the model as a failed tool result; it
    /// never aborts the loop.
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<String>;
}

/// Name -> tool lookup, in stable (alphabetical) order.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The base coding tools: shell and files. No planning, no delegation.
    ///
    /// This is the tool set a subagent gets.
    pub fn base() -> Self {
        let mut registry = Self::new();
        registry.register(BashTool::default());
        registry.register(ReadFileTool);
        registry.register(WriteFileTool);
        registry.register(EditFileTool);
        registry.register(GlobTool);
        registry
    }

    /// The default tool set: base tools plus `todo_write`.
    pub fn with_defaults() -> Self {
        let mut registry = Self::base();
        registry.register(TodoTool::default());
        registry
    }

    /// The task-aware tool set: defaults plus the s10 task system.
    pub fn with_tasks() -> Self {
        let mut registry = Self::with_defaults();
        register_task_tools(&mut registry);
        registry
    }

    /// Register a tool, replacing any existing tool with the same name.
    pub fn register(&mut self, tool: impl Tool + 'static) -> &mut Self {
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
        self
    }

    /// Definitions for every registered tool, as sent to the model.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Execute a tool call by name.
    pub async fn execute(&self, ctx: &ToolContext, call: &ToolCall) -> Result<String> {
        let tool = self
            .get(&call.name)
            .ok_or_else(|| Error::UnknownTool(call.name.clone()))?;
        tool.call(ctx, call.input.clone()).await
    }
}

/// Lexically normalise a path without touching the filesystem.
///
/// This keeps `..` from smuggling a path outside the workspace without
/// requiring the target to exist (a new file, for example).
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Read a required string argument.
pub(crate) fn required_str<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Tool {
            tool: key.to_string(),
            message: format!("missing string argument `{key}`"),
        })
}

/// Truncate on a character boundary, marking the cut.
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}\n... (output truncated)")
    }
}
