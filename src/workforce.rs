//! Departments and the agents that work in them (s18).
//!
//! A department is a small, durable group: a name, a brief, the rules its
//! members share, and one or more agents. Each agent gets an id that becomes
//! the owner recorded on a task when it claims one. The files live under
//! `<workspace>/.departments/`, next to the task board in `.tasks/` and the
//! conversations in `.sessions/`.
//!
//! This module owns the store, not the loop. The desktop bridge builds one
//! agent at a time from a profile and gives it a department-scoped task tool
//! set, so an agent can create, claim, and complete work without claiming a
//! node that belongs to another department.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Directory (relative to the workspace) holding one JSON file per department.
pub const DEPARTMENTS_DIR: &str = ".departments";

/// Longest department or agent name accepted by the store.
pub const MAX_NAME_CHARS: usize = 64;
/// Longest role accepted by the store.
pub const MAX_ROLE_CHARS: usize = 160;
/// Longest brief or collaboration rules accepted by the store.
pub const MAX_TEXT_CHARS: usize = 4_000;
/// Longest message kept in a department's group chat.
pub const MAX_MESSAGE_CHARS: usize = 8_000;
/// How many messages a department keeps; the oldest fall off first.
pub const MAX_MESSAGES: usize = 200;

/// One agent in a department.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentProfile {
    /// `agent_` plus eight lowercase hex characters.
    pub id: String,
    /// Display name, unique within its department.
    pub name: String,
    /// What this agent is responsible for.
    #[serde(default)]
    pub role: String,
    /// Extra instructions folded into this agent's system prompt.
    #[serde(default)]
    pub instructions: String,
    /// When the agent last finished a shift, as an RFC 3339 stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    /// `completed`, `empty`, `failed`, or `stopped`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    /// The agent's own short closing report, kept for the department page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_summary: Option<String>,
    /// Tasks this agent created during its last shift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_created: Option<usize>,
    /// Tasks this agent completed during its last shift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_completed: Option<usize>,
}

/// One line in a department's group chat.
///
/// `sender_kind` is `user`, `agent`, or `system`. An agent message carries the
/// agent's id, so the page can link the words back to the member that said
/// them; a system note does not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DepartmentMessage {
    /// Stable id for the message row.
    pub id: String,
    /// `user`, `agent`, or `system`.
    pub sender_kind: String,
    /// The agent id, when an agent sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
    /// Display name shown beside the message.
    pub sender_name: String,
    /// The message body, in the sender's own words.
    pub content: String,
    /// When it was appended, as an RFC 3339 stamp.
    pub created_at: String,
}

/// A department and its members.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Department {
    /// `dept_` plus eight lowercase hex characters.
    pub id: String,
    /// Display name.
    pub name: String,
    /// What the department is for.
    #[serde(default)]
    pub description: String,
    /// The parent department, when this is a sub-department.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Rules every member carries into its shift.
    #[serde(default)]
    pub instructions: String,
    /// When the department was created, as an RFC 3339 stamp.
    pub created_at: String,
    /// Members, in the order they were added.
    #[serde(default)]
    pub agents: Vec<AgentProfile>,
    /// The group chat, oldest first.
    #[serde(default)]
    pub messages: Vec<DepartmentMessage>,
}

impl Department {
    /// The member named `agent_id`, when it is still in this department.
    pub fn agent(&self, agent_id: &str) -> Option<&AgentProfile> {
        self.agents.iter().find(|agent| agent.id == agent_id)
    }
}

/// Whether `id` is a department id this store would have handed out.
pub fn is_valid_department_id(id: &str) -> bool {
    is_prefixed_hex_id(id, "dept_")
}

/// Whether `id` is an agent id this store would have handed out.
pub fn is_valid_agent_id(id: &str) -> bool {
    is_prefixed_hex_id(id, "agent_")
}

fn is_prefixed_hex_id(id: &str, prefix: &str) -> bool {
    let Some(hex) = id.strip_prefix(prefix) else {
        return false;
    };
    hex.len() == 8
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Allocate a fresh id (`<prefix>` + eight hex characters).
fn allocate_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    let value = hasher.finish() as u32;
    format!("{prefix}{value:08x}")
}

fn workforce_error(message: impl Into<String>) -> Error {
    Error::Workforce(message.into())
}

fn validate_name(value: &str, field: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(workforce_error(format!("{field} cannot be empty")));
    }
    if value.chars().count() > MAX_NAME_CHARS {
        return Err(workforce_error(format!(
            "{field} is longer than {MAX_NAME_CHARS} characters"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(workforce_error(format!(
            "{field} cannot contain control characters"
        )));
    }
    Ok(value.to_string())
}

fn validate_text(value: &str, max: usize, field: &str) -> Result<String> {
    let value = value.trim();
    if value.chars().count() > max {
        return Err(workforce_error(format!(
            "{field} is longer than {max} characters"
        )));
    }
    if value.contains('\0') {
        return Err(workforce_error(format!("{field} cannot contain NUL")));
    }
    Ok(value.to_string())
}

/// Reads and writes `<workspace>/.departments/`.
#[derive(Debug, Clone)]
pub struct WorkforceStore {
    root: PathBuf,
}

impl WorkforceStore {
    /// A store rooted at `<workspace>/.departments`.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        WorkforceStore {
            root: workspace.into().join(DEPARTMENTS_DIR),
        }
    }

    /// The directory holding the department files.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, department_id: &str) -> Result<PathBuf> {
        if !is_valid_department_id(department_id) {
            return Err(workforce_error(format!(
                "invalid department ID: {department_id:?}"
            )));
        }
        Ok(self.root.join(format!("{department_id}.json")))
    }

    /// Whether a department file exists.
    pub fn exists(&self, department_id: &str) -> bool {
        self.path(department_id)
            .map(|path| path.is_file())
            .unwrap_or(false)
    }

    /// Load one department.
    pub fn load(&self, department_id: &str) -> Result<Department> {
        let path = self.path(department_id)?;
        let text = fs::read_to_string(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                workforce_error(format!("department not found: {department_id}"))
            } else {
                error.into()
            }
        })?;
        let department: Department = serde_json::from_str(&text)?;
        if department.id != department_id {
            return Err(workforce_error(format!(
                "department file id {} does not match {department_id}",
                department.id
            )));
        }
        Ok(department)
    }

    /// Every department, ordered by name and then id.
    pub fn list(&self) -> Result<Vec<Department>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut departments = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            if !is_valid_department_id(id) {
                continue;
            }
            departments.push(self.load(id)?);
        }
        departments.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(departments)
    }

    /// Find the department holding `agent_id`.
    pub fn find_agent(&self, agent_id: &str) -> Result<Option<(Department, AgentProfile)>> {
        if !is_valid_agent_id(agent_id) {
            return Err(workforce_error(format!("invalid agent ID: {agent_id:?}")));
        }
        for department in self.list()? {
            if let Some(agent) = department.agent(agent_id) {
                return Ok(Some((department.clone(), agent.clone())));
            }
        }
        Ok(None)
    }

    /// Create a top-level department and persist it.
    pub fn create(&self, name: &str, description: &str, instructions: &str) -> Result<Department> {
        self.create_child(name, description, instructions, None)
    }

    /// Create a department under `parent_id`, or at the top level for `None`.
    pub fn create_child(
        &self,
        name: &str,
        description: &str,
        instructions: &str,
        parent_id: Option<&str>,
    ) -> Result<Department> {
        let name = validate_name(name, "department name")?;
        let description = validate_text(description, MAX_TEXT_CHARS, "department brief")?;
        let instructions = validate_text(instructions, MAX_TEXT_CHARS, "collaboration rules")?;
        let parent_id = match parent_id.map(str::trim).filter(|id| !id.is_empty()) {
            None => None,
            Some(id) => {
                if !is_valid_department_id(id) {
                    return Err(workforce_error(format!(
                        "invalid parent department ID: {id:?}"
                    )));
                }
                if !self.exists(id) {
                    return Err(workforce_error(format!(
                        "parent department not found: {id}"
                    )));
                }
                Some(id.to_string())
            }
        };
        fs::create_dir_all(&self.root)?;

        for _ in 0..100 {
            let department = Department {
                id: allocate_id("dept_"),
                name: name.clone(),
                description: description.clone(),
                parent_id: parent_id.clone(),
                instructions: instructions.clone(),
                created_at: Utc::now().to_rfc3339(),
                agents: Vec::new(),
                messages: Vec::new(),
            };
            let path = self.path(&department.id)?;
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    file.write_all(serde_json::to_string_pretty(&department)?.as_bytes())?;
                    return Ok(department);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(workforce_error("could not allocate a unique department ID"))
    }

    /// Persist a department, overwriting its file.
    pub fn save(&self, department: &Department) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        let path = self.path(&department.id)?;
        let mut department = department.clone();
        if department.messages.len() > MAX_MESSAGES {
            let keep_from = department.messages.len() - MAX_MESSAGES;
            department.messages.drain(..keep_from);
        }
        let temporary = self.root.join(format!(".{}.json.tmp", department.id));
        fs::write(&temporary, serde_json::to_string_pretty(&department)?)?;
        if let Err(error) = fs::rename(&temporary, &path) {
            // Windows will not always replace an existing file with `rename`.
            // The file is only ever written by this store, so removing the old
            // one after the new one is complete is safe.
            let _ = fs::remove_file(&path);
            fs::rename(&temporary, &path).map_err(|_| error)?;
        }
        Ok(())
    }

    /// Remove a department and everything the file recorded.
    ///
    /// Tasks are not touched: a task points at a department by id, and losing
    /// the department must not silently delete work that was done under it.
    pub fn remove(&self, department_id: &str) -> Result<Department> {
        let department = self.load(department_id)?;
        fs::remove_file(self.path(department_id)?)?;
        Ok(department)
    }

    /// Remove a department and every sub-department below it.
    ///
    /// Task files are deliberately left alone, exactly as with [`remove`]. The
    /// returned list is the departments that were deleted, parent first.
    pub fn remove_tree(&self, department_id: &str) -> Result<Vec<Department>> {
        if !is_valid_department_id(department_id) {
            return Err(workforce_error(format!(
                "invalid department ID: {department_id:?}"
            )));
        }
        let all = self.list()?;
        if !all.iter().any(|department| department.id == department_id) {
            return Err(workforce_error(format!(
                "department not found: {department_id}"
            )));
        }

        let mut doomed = Vec::new();
        let mut queue = vec![department_id.to_string()];
        while let Some(id) = queue.pop() {
            if doomed.iter().any(|existing: &Department| existing.id == id) {
                continue;
            }
            let Some(department) = all.iter().find(|department| department.id == id) else {
                continue;
            };
            for child in &all {
                if child.parent_id.as_deref() == Some(id.as_str()) {
                    queue.push(child.id.clone());
                }
            }
            doomed.push(department.clone());
        }

        for department in &doomed {
            fs::remove_file(self.path(&department.id)?)?;
        }
        doomed.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(doomed)
    }

    /// Add an agent to a department.
    pub fn add_agent(
        &self,
        department_id: &str,
        name: &str,
        role: &str,
        instructions: &str,
    ) -> Result<(Department, AgentProfile)> {
        let mut department = self.load(department_id)?;
        let name = validate_name(name, "agent name")?;
        if department
            .agents
            .iter()
            .any(|agent| agent.name.eq_ignore_ascii_case(&name))
        {
            return Err(workforce_error(format!(
                "agent name `{name}` is already used in this department"
            )));
        }
        let role = validate_text(role, MAX_ROLE_CHARS, "agent role")?;
        let instructions = validate_text(instructions, MAX_TEXT_CHARS, "agent instructions")?;
        let agent = AgentProfile {
            id: allocate_id("agent_"),
            name,
            role,
            instructions,
            last_run_at: None,
            last_status: None,
            last_summary: None,
            last_created: None,
            last_completed: None,
        };
        department.agents.push(agent.clone());
        self.save(&department)?;
        Ok((department, agent))
    }

    /// Remove one agent from a department.
    pub fn remove_agent(&self, department_id: &str, agent_id: &str) -> Result<Department> {
        let mut department = self.load(department_id)?;
        let before = department.agents.len();
        department.agents.retain(|agent| agent.id != agent_id);
        if department.agents.len() == before {
            return Err(workforce_error(format!(
                "agent not found in this department: {agent_id}"
            )));
        }
        self.save(&department)?;
        Ok(department)
    }

    /// Remember how an agent's last shift ended.
    #[allow(clippy::too_many_arguments)]
    pub fn record_agent_run(
        &self,
        department_id: &str,
        agent_id: &str,
        status: &str,
        summary: &str,
        created: usize,
        completed: usize,
    ) -> Result<()> {
        let mut department = self.load(department_id)?;
        let Some(agent) = department
            .agents
            .iter_mut()
            .find(|agent| agent.id == agent_id)
        else {
            return Err(workforce_error(format!(
                "agent not found in this department: {agent_id}"
            )));
        };
        agent.last_run_at = Some(Utc::now().to_rfc3339());
        agent.last_status = Some(status.to_string());
        agent.last_summary = Some(validate_text(summary, MAX_TEXT_CHARS, "run summary")?);
        agent.last_created = Some(created);
        agent.last_completed = Some(completed);
        self.save(&department)
    }

    /// Append one line to the department's group chat.
    pub fn append_message(
        &self,
        department_id: &str,
        sender_kind: &str,
        sender_id: Option<&str>,
        sender_name: &str,
        content: &str,
    ) -> Result<DepartmentMessage> {
        let mut department = self.load(department_id)?;
        if !matches!(sender_kind, "user" | "agent" | "system") {
            return Err(workforce_error(format!(
                "unknown message sender kind: {sender_kind:?}"
            )));
        }
        let sender_name = validate_name(sender_name, "sender name")?;
        let content = validate_text(content, MAX_MESSAGE_CHARS, "message")?;
        if content.is_empty() {
            return Err(workforce_error("message cannot be empty"));
        }
        let sender_id = match sender_kind {
            "agent" => {
                let Some(id) = sender_id.map(str::trim).filter(|id| !id.is_empty()) else {
                    return Err(workforce_error("an agent message needs a sender id"));
                };
                if !is_valid_agent_id(id) {
                    return Err(workforce_error(format!("invalid agent ID: {id:?}")));
                }
                Some(id.to_string())
            }
            _ => None,
        };
        let message = DepartmentMessage {
            id: allocate_id("msg_"),
            sender_kind: sender_kind.to_string(),
            sender_id,
            sender_name,
            content,
            created_at: Utc::now().to_rfc3339(),
        };
        department.messages.push(message.clone());
        self.save(&department)?;
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_department_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkforceStore::new(dir.path());
        let department = store
            .create("研发部", "make the runtime reliable", "keep changes small")
            .unwrap();

        assert!(is_valid_department_id(&department.id));
        assert_eq!(store.root(), dir.path().join(DEPARTMENTS_DIR).as_path());
        assert_eq!(store.list().unwrap(), vec![department.clone()]);
        assert_eq!(store.load(&department.id).unwrap(), department);
    }

    #[test]
    fn agents_get_ids_and_names_are_unique_per_department() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkforceStore::new(dir.path());
        let department = store.create("研发部", "", "").unwrap();
        let (with_agent, agent) = store
            .add_agent(
                &department.id,
                "后端",
                "owns the API",
                "verify every change",
            )
            .unwrap();

        assert!(is_valid_agent_id(&agent.id));
        assert_eq!(with_agent.agents.len(), 1);
        assert_eq!(with_agent.agents[0].name, "后端");
        assert!(store
            .add_agent(&department.id, "后端", "", "")
            .unwrap_err()
            .to_string()
            .contains("already used"));
        assert_eq!(store.find_agent(&agent.id).unwrap().unwrap().1.id, agent.id);
    }

    #[test]
    fn names_and_ids_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkforceStore::new(dir.path());
        assert!(store.create("   ", "", "").is_err());
        assert!(store
            .create(&"x".repeat(MAX_NAME_CHARS + 1), "", "")
            .is_err());
        assert!(store.load("dept_00000000").is_err());
        assert!(store.load("../escape").is_err());
        assert!(!is_valid_agent_id("agent_ABCDEF12"));
        assert!(!is_valid_department_id("dept_1234567"));
    }

    #[test]
    fn removing_an_agent_keeps_the_department() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkforceStore::new(dir.path());
        let department = store.create("研发部", "", "").unwrap();
        let (_, agent) = store.add_agent(&department.id, "后端", "", "").unwrap();

        let trimmed = store.remove_agent(&department.id, &agent.id).unwrap();
        assert!(trimmed.agents.is_empty());
        assert!(store
            .remove_agent(&department.id, &agent.id)
            .unwrap_err()
            .to_string()
            .contains("not found"));
        assert_eq!(store.load(&department.id).unwrap().agents.len(), 0);
    }

    #[test]
    fn a_finished_shift_is_recorded_on_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkforceStore::new(dir.path());
        let department = store.create("研发部", "", "").unwrap();
        let (_, agent) = store.add_agent(&department.id, "后端", "", "").unwrap();

        store
            .record_agent_run(&department.id, &agent.id, "completed", "did one task", 1, 1)
            .unwrap();
        let saved = store.load(&department.id).unwrap();
        let saved = saved.agent(&agent.id).unwrap();
        assert_eq!(saved.last_status.as_deref(), Some("completed"));
        assert_eq!(saved.last_summary.as_deref(), Some("did one task"));
        assert_eq!(saved.last_created, Some(1));
        assert_eq!(saved.last_completed, Some(1));
        assert!(saved.last_run_at.is_some());
    }

    #[test]
    fn removing_a_department_leaves_task_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkforceStore::new(dir.path());
        let department = store.create("研发部", "", "").unwrap();
        let task_dir = dir.path().join(crate::tools::TASKS_DIR);
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("task_12345678.json"), "{}").unwrap();

        store.remove(&department.id).unwrap();
        assert!(task_dir.join("task_12345678.json").is_file());
        assert!(store.list().unwrap().is_empty());
    }
}
