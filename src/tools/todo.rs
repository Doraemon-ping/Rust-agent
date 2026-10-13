//! The `todo_write` tool: a task list the model maintains across a session.
//!
//! Unlike the other tools this one is stateful: it remembers the list between
//! calls so the model can plan, then mark steps off as it goes. The state
//! lives behind a mutex so the tool stays `Send + Sync` and shareable.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{Tool, ToolContext};
use crate::error::{Error, Result};

/// Tool name; the loop also uses it to detect planning updates.
pub const TODO_TOOL_NAME: &str = "todo_write";

/// Largest list the model may submit in one call.
pub const MAX_ITEMS: usize = 20;

/// Status of a single task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started.
    #[default]
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Finished.
    Completed,
}

impl TodoStatus {
    /// Checkbox marker used when rendering.
    pub fn marker(self) -> &'static str {
        match self {
            TodoStatus::Pending => "[ ]",
            TodoStatus::InProgress => "[>]",
            TodoStatus::Completed => "[x]",
        }
    }
}

/// A single task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    /// What needs to be done.
    pub content: String,
    /// Where it stands. Defaults to `pending` when omitted.
    #[serde(default)]
    pub status: TodoStatus,
}

/// A shared, mutable task list.
#[derive(Debug, Default)]
pub struct TodoList {
    items: Mutex<Vec<Todo>>,
}

impl TodoList {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Validate and replace the list, returning the rendered view.
    pub fn update(&self, todos: &[Todo]) -> Result<String> {
        if todos.len() > MAX_ITEMS {
            return Err(tool_error(format!("at most {MAX_ITEMS} todos allowed")));
        }
        if let Some(index) = todos.iter().position(|todo| todo.content.trim().is_empty()) {
            return Err(tool_error(format!("todos[{index}] requires content")));
        }
        let in_progress = todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::InProgress)
            .count();
        if in_progress > 1 {
            return Err(tool_error(
                "only one todo can be in_progress at a time".to_string(),
            ));
        }

        let mut items = self.items.lock().expect("todo list poisoned");
        items.clear();
        items.extend_from_slice(todos);
        Ok(render(&items[..]))
    }

    /// A snapshot of the current list.
    pub fn items(&self) -> Vec<Todo> {
        self.items.lock().expect("todo list poisoned").clone()
    }

    /// Render the current list.
    pub fn render(&self) -> String {
        render(&self.items.lock().expect("todo list poisoned")[..])
    }
}

fn render(items: &[Todo]) -> String {
    if items.is_empty() {
        return "No todos.".to_string();
    }
    let mut lines: Vec<String> = items
        .iter()
        .map(|todo| format!("{} {}", todo.status.marker(), todo.content))
        .collect();
    let done = items
        .iter()
        .filter(|todo| todo.status == TodoStatus::Completed)
        .count();
    lines.push(format!("\n({done}/{} completed)", items.len()));
    lines.join("\n")
}

fn tool_error(message: String) -> Error {
    Error::Tool {
        tool: TODO_TOOL_NAME.to_string(),
        message,
    }
}

/// The `todo_write` tool, holding a shared [`TodoList`].
pub struct TodoTool {
    list: Arc<TodoList>,
}

impl TodoTool {
    /// A tool with a fresh, empty list.
    pub fn new() -> Self {
        TodoTool {
            list: Arc::new(TodoList::new()),
        }
    }

    /// Shared handle to the underlying list, for inspection by callers.
    pub fn list(&self) -> Arc<TodoList> {
        Arc::clone(&self.list)
    }
}

impl Default for TodoTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        TODO_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Create and manage a task list for your current coding session."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "maxItems": MAX_ITEMS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string", "minLength": 1 },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        // Accept the array directly, or a JSON string holding it.
        let raw = input.get("todos").cloned().unwrap_or(Value::Null);
        let value = match raw {
            Value::String(text) => serde_json::from_str(&text)
                .map_err(|error| tool_error(format!("todos must be a JSON array: {error}")))?,
            other => other,
        };
        let todos: Vec<Todo> = serde_json::from_value(value)
            .map_err(|error| tool_error(format!("invalid todos: {error}")))?;
        self.list.update(&todos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(content: &str, status: TodoStatus) -> Todo {
        Todo {
            content: content.to_string(),
            status,
        }
    }

    #[test]
    fn render_uses_markers_and_footer() {
        let list = TodoList::new();
        let rendered = list
            .update(&[
                todo("plan", TodoStatus::Completed),
                todo("build", TodoStatus::InProgress),
                todo("test", TodoStatus::Pending),
            ])
            .unwrap();
        assert!(rendered.contains("[x] plan"), "{rendered}");
        assert!(rendered.contains("[>] build"), "{rendered}");
        assert!(rendered.contains("[ ] test"), "{rendered}");
        assert!(rendered.contains("(1/3 completed)"), "{rendered}");
    }

    #[test]
    fn only_one_in_progress_allowed() {
        let list = TodoList::new();
        let error = list
            .update(&[
                todo("a", TodoStatus::InProgress),
                todo("b", TodoStatus::InProgress),
            ])
            .unwrap_err();
        assert!(error.to_string().contains("in_progress"), "{error}");
    }

    #[test]
    fn too_many_todos_rejected() {
        let list = TodoList::new();
        let todos: Vec<Todo> = (0..=MAX_ITEMS)
            .map(|n| todo(&format!("task {n}"), TodoStatus::Pending))
            .collect();
        assert!(list.update(&todos).is_err());
    }

    #[test]
    fn blank_content_rejected() {
        let list = TodoList::new();
        assert!(list.update(&[todo("   ", TodoStatus::Pending)]).is_err());
    }
}
