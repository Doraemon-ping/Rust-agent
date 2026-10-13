//! The task system (s10): a file-persisted task graph.
//!
//! s05's `todo_write` is an in-session checklist. This is the durable version:
//! every task is a JSON file under `<workspace>/.tasks/`, edges recorded in
//! `blockedBy` form a dependency graph, and `owner` records who claimed a task.
//! The state survives the process, so an agent can recover progress later.

use std::collections::HashSet;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{required_str, Tool, ToolContext, ToolRegistry};
use crate::error::{Error, Result};

/// Directory (relative to the workspace) holding one JSON file per task.
pub const TASKS_DIR: &str = ".tasks";

/// Default owner recorded when a task is claimed.
pub const DEFAULT_OWNER: &str = "agent";

/// Tool names.
pub const CREATE_TASK_TOOL_NAME: &str = "create_task";
pub const UPDATE_TASK_TOOL_NAME: &str = "update_task";
pub const LIST_TASKS_TOOL_NAME: &str = "list_tasks";
pub const GET_TASK_TOOL_NAME: &str = "get_task";
pub const CLAIM_TASK_TOOL_NAME: &str = "claim_task";
pub const COMPLETE_TASK_TOOL_NAME: &str = "complete_task";

/// System-prompt guidance for the task tools.
pub const TASKS_PROMPT: &str = "Use task tools to track dependencies and progress. \
Create all task nodes first. After create_task returns runtime-generated IDs, \
use update_task with those exact IDs to add dependencies.";

/// JSON Schema pattern for a task id, shared by the tool schemas.
const TASK_ID_SCHEMA: &str = "^task_[0-9a-f]{8}$";

/// Where a task stands. Two actions move it forward: claim, then complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Created, not started.
    Pending,
    /// Claimed by an owner.
    InProgress,
    /// Done; downstream tasks may now be unblocked.
    Completed,
}

impl TaskStatus {
    /// Compact marker used when listing tasks.
    pub fn marker(self) -> &'static str {
        match self {
            TaskStatus::Pending => "[ ]",
            TaskStatus::InProgress => "[>]",
            TaskStatus::Completed => "[x]",
        }
    }
}

/// One durable task, persisted as `.tasks/{id}.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// `task_` plus 8 hex characters.
    pub id: String,
    /// Short title.
    pub subject: String,
    /// Longer description; recovered across sessions via `get_task`.
    #[serde(default)]
    pub description: String,
    /// Current state.
    pub status: TaskStatus,
    /// Who claimed it, if anyone.
    #[serde(default)]
    pub owner: Option<String>,
    /// IDs this task waits on (the wire name is `blockedBy`).
    #[serde(rename = "blockedBy", default)]
    pub blocked_by: Vec<String>,
    /// Optional teammate worktree (s13) bound to this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
}

/// Whether `id` matches `task_` followed by 8 lowercase hex digits.
pub fn is_valid_task_id(id: &str) -> bool {
    let Some(hex) = id.strip_prefix("task_") else {
        return false;
    };
    hex.len() == 8
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Allocate a fresh, effectively unique id (`task_` + 8 hex chars).
fn allocate_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    let value = hasher.finish() as u32;
    format!("task_{value:08x}")
}

/// A tool error for the task system.
fn task_error(message: impl Into<String>) -> Error {
    Error::Tool {
        tool: "task".to_string(),
        message: message.into(),
    }
}

/// Reads and writes the `.tasks/` directory for one workspace.
#[derive(Debug, Clone)]
pub struct TaskStore {
    root: PathBuf,
}

impl TaskStore {
    /// A store rooted at `<workspace>/.tasks`.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        TaskStore {
            root: workspace.into().join(TASKS_DIR),
        }
    }

    /// The directory holding the task files.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, task_id: &str) -> Result<PathBuf> {
        if !is_valid_task_id(task_id) {
            return Err(task_error(format!("invalid task ID: {task_id:?}")));
        }
        Ok(self.root.join(format!("{task_id}.json")))
    }

    /// Whether a task file exists.
    pub fn exists(&self, task_id: &str) -> bool {
        self.path(task_id)
            .map(|path| path.is_file())
            .unwrap_or(false)
    }

    /// Create a task and persist it, retrying on the (unlikely) id clash.
    pub fn create(&self, subject: &str, description: &str) -> Result<Task> {
        let subject = subject.trim();
        if subject.is_empty() {
            return Err(task_error("task subject cannot be empty"));
        }
        fs::create_dir_all(&self.root)?;

        for _ in 0..100 {
            let task = Task {
                id: allocate_id(),
                subject: subject.to_string(),
                description: description.to_string(),
                status: TaskStatus::Pending,
                owner: None,
                blocked_by: Vec::new(),
                worktree: None,
            };
            let path = self.path(&task.id)?;
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    file.write_all(serde_json::to_string_pretty(&task)?.as_bytes())?;
                    return Ok(task);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(task_error("could not allocate a unique task ID"))
    }

    /// Persist a task, overwriting its file.
    pub fn save(&self, task: &Task) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        let path = self.path(&task.id)?;
        fs::write(&path, serde_json::to_string_pretty(task)?)?;
        Ok(())
    }

    /// Load one task, validating the id and status.
    pub fn load(&self, task_id: &str) -> Result<Task> {
        let path = self.path(task_id)?;
        let text = fs::read_to_string(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                task_error(format!("task not found: {task_id}"))
            } else {
                error.into()
            }
        })?;
        let task: Task = serde_json::from_str(&text)?;
        if task.id != task_id {
            return Err(task_error(format!(
                "task file id {} does not match {task_id}",
                task.id
            )));
        }
        Ok(task)
    }

    /// Every task, ordered by id for a stable listing.
    pub fn list(&self) -> Result<Vec<Task>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut ids: Vec<String> = fs::read_dir(&self.root)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .filter_map(|name| name.strip_suffix(".json").map(str::to_string))
            .filter(|id| is_valid_task_id(id))
            .collect();
        ids.sort();
        ids.iter().map(|id| self.load(id)).collect()
    }

    /// Prerequisites of `task` that are not yet completed (or have gone missing).
    pub fn incomplete_dependencies(&self, task: &Task) -> Vec<String> {
        task.blocked_by
            .iter()
            .filter(|dependency| {
                self.load(dependency)
                    .map(|loaded| loaded.status != TaskStatus::Completed)
                    .unwrap_or(true)
            })
            .cloned()
            .collect()
    }

    /// Whether `task_id` can start: every prerequisite is completed.
    pub fn can_start(&self, task_id: &str) -> Result<bool> {
        let task = self.load(task_id)?;
        Ok(self.incomplete_dependencies(&task).is_empty())
    }

    /// Whether `task_id` transitively depends on `target_id`.
    fn depends_on(&self, task_id: &str, target_id: &str) -> Result<bool> {
        let mut pending = vec![task_id.to_string()];
        let mut visited = HashSet::new();
        while let Some(current) = pending.pop() {
            if current == target_id {
                return Ok(true);
            }
            if !visited.insert(current.clone()) {
                continue;
            }
            pending.extend(self.load(&current)?.blocked_by);
        }
        Ok(false)
    }

    /// Add dependency edges, rejecting unknown/self/cyclic ones. Re-adding an
    /// existing edge is a no-op rather than an error.
    pub fn update_dependencies(&self, task_id: &str, add_blocked_by: &[String]) -> Result<Task> {
        let mut task = self.load(task_id)?;
        if task.status != TaskStatus::Pending || task.owner.is_some() {
            return Err(task_error(format!(
                "task {task_id} dependencies can only be updated while pending and unowned"
            )));
        }

        // Deduplicate while preserving order.
        let mut seen = HashSet::new();
        let dependencies: Vec<String> = add_blocked_by
            .iter()
            .filter(|&dependency| seen.insert(dependency.clone()))
            .cloned()
            .collect();

        for dependency in &dependencies {
            if dependency == task_id {
                return Err(task_error("task cannot depend on itself"));
            }
            if !self.exists(dependency) {
                return Err(task_error(format!("dependency not found: {dependency}")));
            }
            if !task.blocked_by.contains(dependency) && self.depends_on(dependency, task_id)? {
                return Err(task_error(format!(
                    "dependency cycle detected: {task_id} -> {dependency}"
                )));
            }
        }

        for dependency in dependencies {
            if !task.blocked_by.contains(&dependency) {
                task.blocked_by.push(dependency);
            }
        }
        self.save(&task)?;
        Ok(task)
    }

    /// Bind a task (s13) to a teammate worktree name.
    pub fn bind_worktree(&self, task_id: &str, name: &str) -> Result<Task> {
        let mut task = self.load(task_id)?;
        task.worktree = Some(name.to_string());
        self.save(&task)?;
        Ok(task)
    }

    /// Give a claimed task back to the board: `in_progress` -> `pending`.
    pub fn release(&self, task_id: &str) -> Result<Task> {
        let mut task = self.load(task_id)?;
        task.owner = None;
        task.status = TaskStatus::Pending;
        self.save(&task)?;
        Ok(task)
    }

    /// Claim a pending, unblocked task: `pending` -> `in_progress`.
    pub fn claim(&self, task_id: &str, owner: &str) -> Result<String> {
        let mut task = self.load(task_id)?;
        if task.status != TaskStatus::Pending {
            return Ok(format!(
                "Task {task_id} is {}, cannot claim",
                status_name(task.status)
            ));
        }
        let blocked = self.incomplete_dependencies(&task);
        if !blocked.is_empty() {
            return Ok(format!("Blocked by: {}", blocked.join(", ")));
        }
        task.owner = Some(owner.to_string());
        task.status = TaskStatus::InProgress;
        self.save(&task)?;
        Ok(format!("Claimed {} ({})", task.id, task.subject))
    }

    /// Complete a claimed task: `in_progress` -> `completed`, then report any
    /// downstream tasks that were just unblocked.
    pub fn complete(&self, task_id: &str, owner: &str) -> Result<String> {
        let mut task = self.load(task_id)?;
        if task.status != TaskStatus::InProgress {
            return Ok(format!(
                "Task {task_id} is {}, cannot complete",
                status_name(task.status)
            ));
        }
        let current_owner = task.owner.clone().unwrap_or_default();
        if current_owner != owner {
            return Ok(format!(
                "Task {task_id} is owned by {current_owner}, not {owner}"
            ));
        }

        let ready_before: HashSet<String> = self
            .list()?
            .into_iter()
            .filter(|candidate| {
                candidate.status == TaskStatus::Pending
                    && !candidate.blocked_by.is_empty()
                    && self.incomplete_dependencies(candidate).is_empty()
            })
            .map(|candidate| candidate.id)
            .collect();

        task.status = TaskStatus::Completed;
        self.save(&task)?;

        let unblocked: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|candidate| {
                candidate.status == TaskStatus::Pending
                    && !candidate.blocked_by.is_empty()
                    && !ready_before.contains(&candidate.id)
                    && self.incomplete_dependencies(candidate).is_empty()
            })
            .map(|candidate| candidate.subject)
            .collect();

        let mut message = format!("Completed {} ({})", task.id, task.subject);
        if !unblocked.is_empty() {
            message.push_str(&format!("\nUnblocked: {}", unblocked.join(", ")));
        }
        Ok(message)
    }
}

fn status_name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Completed => "completed",
    }
}

/// `create_task`: allocate a node and return its runtime-generated id.
pub struct CreateTaskTool;

#[async_trait]
impl Tool for CreateTaskTool {
    fn name(&self) -> &str {
        CREATE_TASK_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Create a task and return its runtime-generated ID. Create every node \
         first, then wire dependencies with update_task."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subject": { "type": "string", "minLength": 1 },
                "description": { "type": "string" }
            },
            "required": ["subject"],
            "additionalProperties": false
        })
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<String> {
        let subject = required_str(&input, "subject")?;
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let task = TaskStore::new(ctx.workspace()).create(subject, description)?;
        Ok(format!("Created {}: {}", task.id, task.subject))
    }
}

/// `update_task`: add `blockedBy` edges using ids from `create_task`.
pub struct UpdateTaskTool;

#[async_trait]
impl Tool for UpdateTaskTool {
    fn name(&self) -> &str {
        UPDATE_TASK_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Add dependencies to a task using the IDs returned by create_task. The \
         task must still be pending and unowned; cycles are rejected."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "pattern": TASK_ID_SCHEMA },
                "addBlockedBy": {
                    "type": "array",
                    "items": { "type": "string", "pattern": TASK_ID_SCHEMA },
                    "minItems": 1
                }
            },
            "required": ["task_id", "addBlockedBy"],
            "additionalProperties": false
        })
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<String> {
        let task_id = required_str(&input, "task_id")?;
        let raw = input.get("addBlockedBy").cloned().unwrap_or(Value::Null);
        let dependencies: Vec<String> = serde_json::from_value(raw).map_err(|error| {
            task_error(format!("addBlockedBy must be a list of task IDs: {error}"))
        })?;
        let task = TaskStore::new(ctx.workspace()).update_dependencies(task_id, &dependencies)?;
        let dependencies = if task.blocked_by.is_empty() {
            "(none)".to_string()
        } else {
            task.blocked_by.join(", ")
        };
        Ok(format!("Updated {} blockedBy: {dependencies}", task.id))
    }
}

/// `list_tasks`: one line per task, with status, owner, and dependencies.
pub struct ListTasksTool;

#[async_trait]
impl Tool for ListTasksTool {
    fn name(&self) -> &str {
        LIST_TASKS_TOOL_NAME
    }

    fn description(&self) -> &str {
        "List tasks with status, owner, and dependencies."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn call(&self, ctx: &ToolContext, _input: Value) -> Result<String> {
        let tasks = TaskStore::new(ctx.workspace()).list()?;
        if tasks.is_empty() {
            return Ok("No tasks. Use create_task to add some.".to_string());
        }
        let lines = tasks
            .iter()
            .map(|task| {
                let owner = task
                    .owner
                    .as_ref()
                    .map(|owner| format!(" [{owner}]"))
                    .unwrap_or_default();
                let dependencies = if task.blocked_by.is_empty() {
                    String::new()
                } else {
                    format!(" (blockedBy: {})", task.blocked_by.join(", "))
                };
                let worktree = task
                    .worktree
                    .as_ref()
                    .map(|name| format!(" (worktree: {name})"))
                    .unwrap_or_default();
                format!(
                    "{} {}: {} [{}]{owner}{dependencies}{worktree}",
                    task.status.marker(),
                    task.id,
                    task.subject,
                    status_name(task.status),
                )
            })
            .collect::<Vec<_>>();
        Ok(lines.join("\n"))
    }
}

/// `get_task`: the full task JSON, description and all.
pub struct GetTaskTool;

#[async_trait]
impl Tool for GetTaskTool {
    fn name(&self) -> &str {
        GET_TASK_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Get a task by ID, including its description and dependency details."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "task_id": { "type": "string" } },
            "required": ["task_id"]
        })
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<String> {
        let task_id = required_str(&input, "task_id")?;
        let task = TaskStore::new(ctx.workspace()).load(task_id)?;
        Ok(serde_json::to_string_pretty(&task)?)
    }
}

/// `claim_task`: take ownership of a pending, unblocked task.
pub struct ClaimTaskTool {
    owner: String,
}

impl Default for ClaimTaskTool {
    fn default() -> Self {
        ClaimTaskTool {
            owner: DEFAULT_OWNER.to_string(),
        }
    }
}

impl ClaimTaskTool {
    /// A variant that claims tasks for a named teammate (s13).
    pub fn owned_by(owner: impl Into<String>) -> Self {
        ClaimTaskTool {
            owner: owner.into(),
        }
    }
}

#[async_trait]
impl Tool for ClaimTaskTool {
    fn name(&self) -> &str {
        CLAIM_TASK_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Claim a pending task whose dependencies are complete, setting its owner \
         and moving it to in_progress."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "task_id": { "type": "string" } },
            "required": ["task_id"]
        })
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<String> {
        let task_id = required_str(&input, "task_id")?;
        TaskStore::new(ctx.workspace()).claim(task_id, &self.owner)
    }
}

/// `complete_task`: finish a task this agent owns and report unblocked work.
pub struct CompleteTaskTool {
    owner: String,
}

impl Default for CompleteTaskTool {
    fn default() -> Self {
        CompleteTaskTool {
            owner: DEFAULT_OWNER.to_string(),
        }
    }
}

impl CompleteTaskTool {
    /// A variant that completes tasks owned by a named teammate (s13).
    pub fn owned_by(owner: impl Into<String>) -> Self {
        CompleteTaskTool {
            owner: owner.into(),
        }
    }
}

#[async_trait]
impl Tool for CompleteTaskTool {
    fn name(&self) -> &str {
        COMPLETE_TASK_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Complete the task claimed by this agent, and report which downstream \
         tasks were unblocked."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "task_id": { "type": "string" } },
            "required": ["task_id"]
        })
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<String> {
        let task_id = required_str(&input, "task_id")?;
        TaskStore::new(ctx.workspace()).complete(task_id, &self.owner)
    }
}

/// Register all six task tools on a registry.
pub fn register_task_tools(registry: &mut ToolRegistry) {
    registry.register(CreateTaskTool);
    registry.register(UpdateTaskTool);
    registry.register(ListTasksTool);
    registry.register(GetTaskTool);
    registry.register(ClaimTaskTool::default());
    registry.register(CompleteTaskTool::default());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;

    fn store() -> (tempfile::TempDir, TaskStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn ids_have_the_expected_shape() {
        for _ in 0..50 {
            assert!(is_valid_task_id(&allocate_id()));
        }
        assert!(!is_valid_task_id("task_1234567"));
        assert!(is_valid_task_id("task_12345678"));
        assert!(!is_valid_task_id("task_123456789"));
        assert!(!is_valid_task_id("task_ABCDEF12"));
        assert!(!is_valid_task_id("task_1234567g"));
        assert!(!is_valid_task_id("nope"));
    }

    #[test]
    fn with_tasks_exposes_todo_and_the_task_tools() {
        let names: Vec<String> = ToolRegistry::with_tasks()
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        for expected in [
            crate::tools::TODO_TOOL_NAME,
            CREATE_TASK_TOOL_NAME,
            UPDATE_TASK_TOOL_NAME,
            LIST_TASKS_TOOL_NAME,
            GET_TASK_TOOL_NAME,
            CLAIM_TASK_TOOL_NAME,
            COMPLETE_TASK_TOOL_NAME,
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing {expected}: {names:?}"
            );
        }
    }

    #[test]
    fn create_persists_a_pending_unowned_task() {
        let (_dir, store) = store();
        let task = store.create("setup schema", "create the tables").unwrap();
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.owner.is_none());
        assert!(task.blocked_by.is_empty());

        assert!(store.root().join(format!("{}.json", task.id)).is_file());
        assert_eq!(store.load(&task.id).unwrap(), task);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn blank_subject_and_bad_ids_are_rejected() {
        let (_dir, store) = store();
        assert!(store.create("   ", "").is_err());
        assert!(store.load("not-a-task").is_err());
        assert!(store
            .load("task_00000000")
            .unwrap_err()
            .to_string()
            .contains("not found"));
    }

    #[test]
    fn unknown_self_and_cyclic_dependencies_are_rejected() {
        let (_dir, store) = store();
        let a = store.create("a", "").unwrap();
        let b = store.create("b", "").unwrap();
        let c = store.create("c", "").unwrap();

        assert!(store
            .update_dependencies(&a.id, std::slice::from_ref(&a.id))
            .is_err());
        assert!(store
            .update_dependencies(&a.id, &["task_00000000".to_string()])
            .is_err());

        store
            .update_dependencies(&b.id, std::slice::from_ref(&a.id))
            .unwrap();
        store
            .update_dependencies(&c.id, std::slice::from_ref(&b.id))
            .unwrap();
        // c -> b -> a, so a may not depend on c.
        let error = store
            .update_dependencies(&a.id, std::slice::from_ref(&c.id))
            .unwrap_err();
        assert!(error.to_string().contains("cycle"), "{error}");

        // Re-adding an existing edge is a no-op.
        store
            .update_dependencies(&b.id, std::slice::from_ref(&a.id))
            .unwrap();
        assert_eq!(store.load(&b.id).unwrap().blocked_by, vec![a.id.clone()]);
    }

    #[test]
    fn dependencies_freeze_once_the_task_is_claimed() {
        let (_dir, store) = store();
        let a = store.create("a", "").unwrap();
        let b = store.create("b", "").unwrap();
        store.claim(&a.id, DEFAULT_OWNER).unwrap();
        let error = store
            .update_dependencies(&a.id, std::slice::from_ref(&b.id))
            .unwrap_err();
        assert!(error.to_string().contains("pending and unowned"), "{error}");
    }

    #[test]
    fn claim_waits_for_dependencies_and_complete_unblocks() {
        let (_dir, store) = store();
        let schema = store.create("schema", "").unwrap();
        let endpoints = store.create("endpoints", "").unwrap();
        let tests = store.create("tests", "").unwrap();
        let docs = store.create("docs", "").unwrap();

        store
            .update_dependencies(&endpoints.id, std::slice::from_ref(&schema.id))
            .unwrap();
        store
            .update_dependencies(&tests.id, std::slice::from_ref(&endpoints.id))
            .unwrap();
        store
            .update_dependencies(&docs.id, std::slice::from_ref(&schema.id))
            .unwrap();

        assert!(store
            .claim(&endpoints.id, DEFAULT_OWNER)
            .unwrap()
            .starts_with("Blocked by:"));
        assert!(store
            .claim(&schema.id, DEFAULT_OWNER)
            .unwrap()
            .starts_with("Claimed"));
        assert!(store
            .claim(&schema.id, DEFAULT_OWNER)
            .unwrap()
            .contains("cannot claim"));
        assert!(store
            .complete(&schema.id, "someone-else")
            .unwrap()
            .contains("owned by"));

        let message = store.complete(&schema.id, DEFAULT_OWNER).unwrap();
        assert!(message.contains("Completed"), "{message}");
        assert!(message.contains("Unblocked:"), "{message}");
        assert!(
            message.contains("endpoints") && message.contains("docs"),
            "{message}"
        );

        // tests stays blocked until endpoints completes.
        assert!(store
            .claim(&tests.id, DEFAULT_OWNER)
            .unwrap()
            .starts_with("Blocked by:"));
        assert!(store
            .claim(&docs.id, DEFAULT_OWNER)
            .unwrap()
            .starts_with("Claimed"));
    }

    fn call(name: &str, input: Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: name.into(),
            input,
        }
    }

    #[tokio::test]
    async fn registry_dispatches_the_task_tools() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let mut registry = ToolRegistry::new();
        register_task_tools(&mut registry);

        let empty = registry
            .execute(&ctx, &call(LIST_TASKS_TOOL_NAME, json!({})))
            .await
            .unwrap();
        assert!(empty.starts_with("No tasks"), "{empty}");

        let created = registry
            .execute(
                &ctx,
                &call(CREATE_TASK_TOOL_NAME, json!({ "subject": "schema" })),
            )
            .await
            .unwrap();
        let id = created
            .strip_prefix("Created ")
            .and_then(|rest| rest.split(':').next())
            .unwrap()
            .to_string();
        assert!(is_valid_task_id(&id), "{created}");

        let listed = registry
            .execute(&ctx, &call(LIST_TASKS_TOOL_NAME, json!({})))
            .await
            .unwrap();
        assert!(
            listed.contains("[ ]") && listed.contains("schema"),
            "{listed}"
        );
    }

    #[tokio::test]
    async fn get_task_returns_full_json_with_wire_field_names() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let mut registry = ToolRegistry::new();
        register_task_tools(&mut registry);

        let created = registry
            .execute(
                &ctx,
                &call(CREATE_TASK_TOOL_NAME, json!({ "subject": "a" })),
            )
            .await
            .unwrap();
        let id = created
            .strip_prefix("Created ")
            .and_then(|rest| rest.split(':').next())
            .unwrap()
            .to_string();

        let detail = registry
            .execute(&ctx, &call(GET_TASK_TOOL_NAME, json!({ "task_id": id })))
            .await
            .unwrap();
        assert!(detail.contains("\"blockedBy\""), "{detail}");
        assert!(detail.contains("\"status\": \"pending\""), "{detail}");
    }

    #[tokio::test]
    async fn update_task_rejects_a_missing_dependency_list() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let mut registry = ToolRegistry::new();
        register_task_tools(&mut registry);

        let created = registry
            .execute(
                &ctx,
                &call(CREATE_TASK_TOOL_NAME, json!({ "subject": "a" })),
            )
            .await
            .unwrap();
        let id = created
            .strip_prefix("Created ")
            .and_then(|rest| rest.split(':').next())
            .unwrap()
            .to_string();

        let error = registry
            .execute(&ctx, &call(UPDATE_TASK_TOOL_NAME, json!({ "task_id": id })))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("addBlockedBy"), "{error}");
    }
}
