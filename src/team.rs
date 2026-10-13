//! Agent teams (s13): one Lead, many persistent teammates.
//!
//! A subagent (s06) is a single call. A *teammate* is a long-lived runtime with
//! its own conversation, its own working directory and a `WORK -> IDLE -> WORK`
//! loop over the shared task board. Nothing is shared through the model
//! context: Lead and teammates talk over file mailboxes, and the harness folds
//! the events into the Lead's conversation only when it wakes up.
//!
//! ```text
//!   Lead ──spawn_teammate──> TeammateRuntime (thread)
//!     ^                          │  work / idle
//!     │  .mailboxes/lead.jsonl    │  claims ready tasks, completes them
//!     └──── result / idle / plan / shutdown ────┘
//! ```
//!
//! Two protocols travel on the bus rather than through tool results: a typed
//! `shutdown_request` / `shutdown_response` handshake, and a plan gate that
//! blocks a teammate's mutating tools until the Lead approves its plan.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::{Agent, AgentObserver, NoopObserver};
use crate::error::{Error, Result};
use crate::hooks::Hook;
use crate::llm::LlmClient;
use crate::message::{Message, ToolCall};
use crate::tools::{
    required_str, ClaimTaskTool, CompleteTaskTool, ListTasksTool, Task, TaskStatus, TaskStore,
    Tool, ToolContext, ToolRegistry,
};

/// Directory (relative to the workspace) holding `<agent>.jsonl` inboxes.
pub const MAILBOXES_DIR: &str = ".mailboxes";
/// Directory holding teammate worktrees.
pub const WORKTREES_DIR: &str = ".worktrees";
/// The coordinator's mailbox name.
pub const LEAD: &str = "lead";

/// System-prompt guidance for the team tools.
pub const TEAM_PROMPT: &str = "When parallel work would help, first propose a small team with \
clear responsibilities and wait for the user's confirmation; do not call spawn_teammate before \
the user confirms. Only the Lead changes task dependencies. After spawning a teammate, end the \
turn instead of polling: the runtime delivers team events and wakes the Lead.";

/// Lead-side tool names.
pub const SPAWN_TEAMMATE_TOOL_NAME: &str = "spawn_teammate";
pub const LIST_TEAMMATES_TOOL_NAME: &str = "list_teammates";
pub const SEND_MESSAGE_TOOL_NAME: &str = "send_message";
pub const REQUEST_SHUTDOWN_TOOL_NAME: &str = "request_shutdown";
pub const REQUEST_PLAN_TOOL_NAME: &str = "request_plan";
pub const REVIEW_PLAN_TOOL_NAME: &str = "review_plan";
pub const CREATE_WORKTREE_TOOL_NAME: &str = "create_worktree";
/// Teammate-side tool name.
pub const SUBMIT_PLAN_TOOL_NAME: &str = "submit_plan";

/// Message kinds carried on the bus.
pub const MSG_MESSAGE: &str = "message";
pub const MSG_RESULT: &str = "result";
pub const MSG_IDLE: &str = "idle_notification";
pub const MSG_ERROR: &str = "error";
pub const MSG_SHUTDOWN_REQUEST: &str = "shutdown_request";
pub const MSG_SHUTDOWN_RESPONSE: &str = "shutdown_response";
pub const MSG_PLAN_REQUEST: &str = "plan_request";
pub const MSG_PLAN_APPROVAL_REQUEST: &str = "plan_approval_request";
pub const MSG_PLAN_APPROVAL_RESPONSE: &str = "plan_approval_response";

/// How long an idle teammate waits for traffic before scanning the board.
pub const IDLE_SCAN_INTERVAL: Duration = Duration::from_secs(2);

/// Whether `name` is a usable mailbox / teammate name.
pub fn is_valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Whether `name` is reserved for the runtime itself.
pub fn is_reserved_agent_name(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), LEAD | "agent")
}

/// Whether `name` is usable as a worktree directory / branch suffix.
pub fn is_valid_worktree_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.contains("..")
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// One bus message. Absent metadata is simply `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamMessage {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub content: String,
    #[serde(rename = "type", default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub ts: f64,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub approve: Option<bool>,
    #[serde(default)]
    pub task_id: Option<String>,
}

fn default_kind() -> String {
    MSG_MESSAGE.to_string()
}

impl TeamMessage {
    /// A plain message.
    pub fn new(from: &str, to: &str, content: impl Into<String>, kind: &str) -> Self {
        TeamMessage {
            from: from.to_string(),
            to: to.to_string(),
            content: content.into(),
            kind: kind.to_string(),
            ts: now_seconds(),
            request_id: None,
            approve: None,
            task_id: None,
        }
    }

    /// Attach the request id that ties a response to its request.
    pub fn with_request(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    /// Attach an approve/reject verdict.
    pub fn with_approval(mut self, approve: bool) -> Self {
        self.approve = Some(approve);
        self
    }
}

/// Seconds since the epoch, as the reference records them.
pub fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or_default()
}

/// Shared bus state: the mailbox directory and the wake-up condition.
struct BusShared {
    dir: PathBuf,
    lock: Mutex<()>,
    changed: Condvar,
}

/// File-backed inboxes: one `<agent>.jsonl` per participant.
///
/// Reads are *destructive* -- an inbox is drained when it is read -- so a
/// message is delivered exactly once to the model that reads it.
#[derive(Clone)]
pub struct MessageBus {
    shared: Arc<BusShared>,
}

impl MessageBus {
    /// A bus rooted at `<workspace>/.mailboxes`.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        MessageBus {
            shared: Arc::new(BusShared {
                dir: workspace.into().join(MAILBOXES_DIR),
                lock: Mutex::new(()),
                changed: Condvar::new(),
            }),
        }
    }

    /// The mailbox directory.
    pub fn dir(&self) -> &Path {
        &self.shared.dir
    }

    fn path(&self, agent: &str) -> Result<PathBuf> {
        if !is_valid_agent_name(agent) {
            return Err(Error::Team(format!("invalid mailbox recipient: {agent:?}")));
        }
        Ok(self.shared.dir.join(format!("{agent}.jsonl")))
    }

    /// Append a message to the recipient's inbox and wake any waiters.
    pub fn send(&self, message: &TeamMessage) -> Result<()> {
        let path = self.path(&message.to)?;
        let line = serde_json::to_string(message)?;
        let _guard = self.shared.lock.lock().expect("bus poisoned");
        fs::create_dir_all(&self.shared.dir)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes())?;
        self.shared.changed.notify_all();
        Ok(())
    }

    /// Drain a mailbox, returning its messages in arrival order.
    pub fn read_inbox(&self, agent: &str) -> Result<Vec<TeamMessage>> {
        let path = self.path(agent)?;
        let _guard = self.shared.lock.lock().expect("bus poisoned");
        drain(&path)
    }

    /// Whether a mailbox has anything waiting.
    pub fn peek(&self, agent: &str) -> Result<bool> {
        let path = self.path(agent)?;
        let _guard = self.shared.lock.lock().expect("bus poisoned");
        has_content(&path)
    }

    /// Block until the mailbox has messages, or `timeout` expires.
    pub fn wait_for_messages(&self, agent: &str, timeout: Duration) -> Result<Vec<TeamMessage>> {
        let path = self.path(agent)?;
        let deadline = std::time::Instant::now() + timeout;
        let mut guard = self.shared.lock.lock().expect("bus poisoned");
        loop {
            if has_content(&path)? {
                return drain(&path);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(Vec::new());
            }
            let (next, _) = self
                .shared
                .changed
                .wait_timeout(guard, deadline - now)
                .expect("bus poisoned");
            guard = next;
        }
    }
}

/// Whether `path` exists and holds at least one non-whitespace byte.
fn has_content(path: &Path) -> Result<bool> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(!text.trim().is_empty()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Read and remove a mailbox file.
fn drain(path: &Path) -> Result<Vec<TeamMessage>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut messages = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        messages.push(serde_json::from_str::<TeamMessage>(line)?);
    }
    if let Err(error) = fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error.into());
        }
    }
    Ok(messages)
}

/// How much of the workspace a teammate is allowed to touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanGate {
    /// No plan is needed; mutating tools run.
    NotRequired,
    /// A plan was demanded but not submitted yet.
    Required,
    /// A plan is waiting for the Lead's decision.
    Pending,
    /// The Lead approved the current plan.
    Approved,
    /// The Lead rejected the plan; the teammate must revise it.
    Rejected,
}

impl PlanGate {
    /// Wire / display name.
    pub fn name(self) -> &'static str {
        match self {
            PlanGate::NotRequired => "not_required",
            PlanGate::Required => "required",
            PlanGate::Pending => "pending",
            PlanGate::Approved => "approved",
            PlanGate::Rejected => "rejected",
        }
    }

    /// Whether `bash` / `write_file` / `edit_file` may run.
    pub fn allows_changes(self) -> bool {
        matches!(self, PlanGate::NotRequired | PlanGate::Approved)
    }

    /// Rebuild a gate from its wire name (unknown values stay `Required`).
    pub fn from_name(name: &str) -> PlanGate {
        match name {
            "not_required" => PlanGate::NotRequired,
            "pending" => PlanGate::Pending,
            "approved" => PlanGate::Approved,
            "rejected" => PlanGate::Rejected,
            _ => PlanGate::Required,
        }
    }
}

/// The two typed protocols that travel over the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// Lead asks a teammate to stop.
    Shutdown,
    /// Teammate asks the Lead to approve a plan.
    PlanApproval,
}

impl RequestKind {
    /// Request name on the wire.
    pub fn name(self) -> &'static str {
        match self {
            RequestKind::Shutdown => "shutdown",
            RequestKind::PlanApproval => "plan_approval",
        }
    }

    /// The message kind that answers this request.
    pub fn response_kind(self) -> &'static str {
        match self {
            RequestKind::Shutdown => MSG_SHUTDOWN_RESPONSE,
            RequestKind::PlanApproval => MSG_PLAN_APPROVAL_RESPONSE,
        }
    }

    /// Parse the request name carried by a pending request.
    pub fn from_name(name: &str) -> Option<RequestKind> {
        match name {
            "shutdown" => Some(RequestKind::Shutdown),
            "plan_approval" => Some(RequestKind::PlanApproval),
            _ => None,
        }
    }
}

/// Lifecycle of one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStatus {
    /// Sent, not yet answered.
    Pending,
    /// Answered with an approval.
    Approved,
    /// Answered with a rejection.
    Rejected,
}

impl RequestStatus {
    /// Wire / display name.
    pub fn name(self) -> &'static str {
        match self {
            RequestStatus::Pending => "pending",
            RequestStatus::Approved => "approved",
            RequestStatus::Rejected => "rejected",
        }
    }
}

/// One outstanding request, matched to its response by id and identity.
#[derive(Debug, Clone)]
pub struct ProtocolState {
    /// `req_NNNNNN`.
    pub request_id: String,
    /// Which protocol this request belongs to.
    pub kind: RequestKind,
    /// Who created the request.
    pub sender: String,
    /// Who must answer it.
    pub target: String,
    /// Current status.
    pub status: RequestStatus,
    /// The plan text (for plan approvals).
    pub payload: String,
    /// The assignee's work version when the request was created.
    pub work_version: u64,
    /// The task the assignee held when the request was created.
    pub task_id: Option<String>,
}

/// What a teammate is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeammateStatus {
    /// Running a turn.
    Working,
    /// Parked until the Lead answers its plan.
    WaitingApproval,
    /// Waiting for work or messages.
    Idle,
    /// Shutting down.
    Stopping,
}

impl TeammateStatus {
    /// Wire / display name.
    pub fn name(self) -> &'static str {
        match self {
            TeammateStatus::Working => "working",
            TeammateStatus::WaitingApproval => "waiting_approval",
            TeammateStatus::Idle => "idle",
            TeammateStatus::Stopping => "stopping",
        }
    }
}

/// Tools a teammate may not call until its plan is approved.
pub fn is_mutating_tool(name: &str) -> bool {
    matches!(name, "bash" | "write_file" | "edit_file")
}

#[derive(Default)]
struct TeamState {
    protocol: BTreeMap<String, ProtocolState>,
    gates: BTreeMap<String, PlanGate>,
    /// teammate -> the plan request currently awaiting review
    plan_requests: BTreeMap<String, String>,
    teammates: BTreeMap<String, TeammateStatus>,
    /// teammate -> work version, bumped whenever its assignment changes
    versions: BTreeMap<String, u64>,
    /// teammate -> the task it holds
    assignments: BTreeMap<String, String>,
}

struct TeamInner {
    workspace: PathBuf,
    bus: MessageBus,
    state: Mutex<TeamState>,
    /// Serialises task-board mutations across teammate threads.
    board: Mutex<()>,
    client: Arc<dyn LlmClient>,
    observer: Mutex<Arc<dyn AgentObserver>>,
    max_tokens: AtomicU64,
    max_steps: AtomicU64,
    counter: AtomicU64,
}

/// The Lead's handle on the team. Cheap to clone; shared by every thread.
#[derive(Clone)]
pub struct Team {
    inner: Arc<TeamInner>,
}

impl Team {
    /// A team rooted at `workspace`, spawning teammates on `client`.
    pub fn new(workspace: impl Into<PathBuf>, client: Arc<dyn LlmClient>) -> Self {
        let workspace = workspace.into();
        Team {
            inner: Arc::new(TeamInner {
                bus: MessageBus::new(&workspace),
                workspace,
                state: Mutex::new(TeamState::default()),
                board: Mutex::new(()),
                client,
                observer: Mutex::new(Arc::new(NoopObserver)),
                max_tokens: AtomicU64::new(8192),
                max_steps: AtomicU64::new(64),
                counter: AtomicU64::new(0),
            }),
        }
    }

    /// Render teammate events through an observer (the CLI does this).
    pub fn with_observer(self, observer: impl AgentObserver + 'static) -> Self {
        *self.inner.observer.lock().expect("team poisoned") = Arc::new(observer);
        self
    }

    /// Per-call response budget for teammate turns.
    pub fn with_max_tokens(self, max_tokens: u32) -> Self {
        self.inner
            .max_tokens
            .store(u64::from(max_tokens), Ordering::Relaxed);
        self
    }

    /// Safety bound on teammate loop iterations per turn.
    pub fn with_max_steps(self, max_steps: usize) -> Self {
        self.inner
            .max_steps
            .store(max_steps as u64, Ordering::Relaxed);
        self
    }

    /// The workspace root every teammate shares.
    pub fn workspace(&self) -> &Path {
        &self.inner.workspace
    }

    /// The message bus.
    pub fn bus(&self) -> &MessageBus {
        &self.inner.bus
    }

    /// The shared task board.
    pub fn tasks(&self) -> TaskStore {
        TaskStore::new(&self.inner.workspace)
    }
}

/// Protocol bookkeeping and mailbox delivery.
impl Team {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, TeamState> {
        self.inner.state.lock().expect("team poisoned")
    }

    fn lock_board(&self) -> std::sync::MutexGuard<'_, ()> {
        self.inner.board.lock().expect("team poisoned")
    }

    fn observer(&self) -> Arc<dyn AgentObserver> {
        Arc::clone(&self.inner.observer.lock().expect("team poisoned"))
    }

    fn unique_request_id(&self, state: &TeamState) -> String {
        loop {
            let value = self.inner.counter.fetch_add(1, Ordering::Relaxed) % 1_000_000;
            let request_id = format!("req_{value:06}");
            if !state.protocol.contains_key(&request_id) {
                return request_id;
            }
        }
    }

    /// A fresh request id that is not currently pending.
    pub fn new_request_id(&self) -> String {
        let state = self.lock_state();
        self.unique_request_id(&state)
    }

    /// Match one protocol response to its pending request.
    pub fn match_response(
        &self,
        kind: &str,
        request_id: &str,
        approve: bool,
        from: &str,
        to: &str,
    ) -> bool {
        let mut state = self.lock_state();
        let Some(request) = state.protocol.get_mut(request_id) else {
            return false;
        };
        if kind != request.kind.response_kind()
            || from != request.target
            || to != request.sender
            || request.status != RequestStatus::Pending
        {
            return false;
        }
        request.status = if approve {
            RequestStatus::Approved
        } else {
            RequestStatus::Rejected
        };
        true
    }

    /// Drain the Lead's inbox, applying any protocol responses it carries.
    pub fn consume_lead_inbox(&self) -> Result<Vec<TeamMessage>> {
        let messages = self.inner.bus.read_inbox(LEAD)?;
        for message in &messages {
            if message.kind.ends_with("_response") {
                if let Some(request_id) = message.request_id.clone() {
                    self.match_response(
                        &message.kind,
                        &request_id,
                        message.approve.unwrap_or(false),
                        &message.from,
                        &message.to,
                    );
                }
            }
        }
        Ok(messages)
    }

    /// Render teammate events for the Lead's next turn.
    pub fn format_team_events(messages: &[TeamMessage]) -> String {
        let lines = messages
            .iter()
            .map(|message| {
                let suffix = message
                    .request_id
                    .as_ref()
                    .map(|id| format!(" request_id={id}"))
                    .unwrap_or_default();
                format!(
                    "[{}{suffix}] {}: {}",
                    message.kind, message.from, message.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("[Team events]\n{lines}")
    }

    /// Active teammates with their status, in name order.
    pub fn teammates(&self) -> Vec<(String, TeammateStatus)> {
        self.lock_state()
            .teammates
            .iter()
            .map(|(name, status)| (name.clone(), *status))
            .collect()
    }

    /// One teammate's status.
    pub fn status(&self, name: &str) -> Option<TeammateStatus> {
        self.lock_state().teammates.get(name).copied()
    }

    /// A teammate's plan gate.
    pub fn gate(&self, name: &str) -> PlanGate {
        self.lock_state()
            .gates
            .get(name)
            .copied()
            .unwrap_or(PlanGate::NotRequired)
    }

    /// The status of one outstanding request.
    pub fn request_status(&self, request_id: &str) -> Option<RequestStatus> {
        self.lock_state()
            .protocol
            .get(request_id)
            .map(|request| request.status)
    }

    /// The task a teammate currently holds.
    pub fn assignment(&self, name: &str) -> Option<String> {
        self.lock_state().assignments.get(name).cloned()
    }

    /// `(work version, task)` for a teammate.
    pub fn current_work_identity(&self, name: &str) -> (u64, Option<String>) {
        let state = self.lock_state();
        (
            state.versions.get(name).copied().unwrap_or(0),
            state.assignments.get(name).cloned(),
        )
    }

    fn set_status(&self, name: &str, status: TeammateStatus) {
        self.lock_state().teammates.insert(name.to_string(), status);
    }

    fn bump_version(&self, name: &str) {
        let mut state = self.lock_state();
        let version = state.versions.entry(name.to_string()).or_insert(0);
        *version += 1;
    }

    /// Drop a teammate's assignment, bumping its work version.
    fn clear_assignment(&self, name: &str) {
        self.lock_state().assignments.remove(name);
        self.bump_version(name);
    }

    fn forget(&self, name: &str) {
        let mut state = self.lock_state();
        state.teammates.remove(name);
        state.gates.remove(name);
        state.plan_requests.remove(name);
        state.assignments.remove(name);
    }

    /// A teammate asking the Lead to approve its plan.
    pub fn submit_plan(&self, name: &str, plan: &str) -> Result<String> {
        let mut state = self.lock_state();
        if state.gates.get(name) == Some(&PlanGate::Pending) {
            return Ok("A plan is already waiting for review.".to_string());
        }
        let request_id = self.unique_request_id(&state);
        let work_version = state.versions.get(name).copied().unwrap_or(0);
        let task_id = state.assignments.get(name).cloned();
        state.protocol.insert(
            request_id.clone(),
            ProtocolState {
                request_id: request_id.clone(),
                kind: RequestKind::PlanApproval,
                sender: name.to_string(),
                target: LEAD.to_string(),
                status: RequestStatus::Pending,
                payload: plan.to_string(),
                work_version,
                task_id,
            },
        );
        state.gates.insert(name.to_string(), PlanGate::Pending);
        state
            .plan_requests
            .insert(name.to_string(), request_id.clone());
        state
            .teammates
            .insert(name.to_string(), TeammateStatus::WaitingApproval);
        drop(state);

        self.inner.bus.send(
            &TeamMessage::new(name, LEAD, plan, MSG_PLAN_APPROVAL_REQUEST)
                .with_request(request_id.clone()),
        )?;
        Ok(format!(
            "Plan submitted ({request_id}). Wait for Lead's decision."
        ))
    }

    /// Apply the Lead's decision on a teammate's current plan.
    pub fn apply_plan_response(&self, name: &str, message: &TeamMessage) -> (bool, String) {
        let (version, task_id) = self.current_work_identity(name);
        let request_id = message.request_id.clone().unwrap_or_default();
        let mut state = self.lock_state();
        let expected = state.plan_requests.get(name).cloned();
        let request = state.protocol.get(&request_id).cloned();
        let status = request.as_ref().map(|request| request.status);
        let valid = message.from == LEAD
            && message.to == name
            && expected.as_deref() == Some(request_id.as_str())
            && request.as_ref().is_some_and(|request| {
                request.kind == RequestKind::PlanApproval
                    && request.sender == name
                    && request.target == LEAD
                    && request.work_version == version
                    && request.task_id == task_id
            })
            && matches!(
                status,
                Some(RequestStatus::Approved | RequestStatus::Rejected)
            )
            && message.approve.unwrap_or(false) == (status == Some(RequestStatus::Approved));
        if !valid {
            return (
                false,
                "[Ignored plan response: request mismatch]".to_string(),
            );
        }

        let approved = status == Some(RequestStatus::Approved);
        state.gates.insert(
            name.to_string(),
            if approved {
                PlanGate::Approved
            } else {
                PlanGate::Rejected
            },
        );
        state
            .teammates
            .insert(name.to_string(), TeammateStatus::Working);
        state.plan_requests.remove(name);
        let outcome = if approved { "approved" } else { "rejected" };
        (true, format!("[Plan {outcome}] {}", message.content))
    }

    /// Accept a shutdown request only from the Lead, only once.
    pub fn apply_shutdown_request(
        &self,
        name: &str,
        message: &TeamMessage,
    ) -> (bool, Option<String>) {
        let request_id = message.request_id.clone().unwrap_or_default();
        let mut state = self.lock_state();
        let valid = message.from == LEAD
            && message.to == name
            && state.protocol.get(&request_id).is_some_and(|request| {
                request.kind == RequestKind::Shutdown
                    && request.sender == LEAD
                    && request.target == name
                    && request.status == RequestStatus::Pending
            })
            && state.teammates.get(name) != Some(&TeammateStatus::Stopping);
        if !valid {
            return (false, None);
        }
        state
            .teammates
            .insert(name.to_string(), TeammateStatus::Stopping);
        (true, Some(request_id))
    }

    /// Apply a teammate's inbox: returns `(stop, notices for its history)`.
    pub fn handle_inbox(&self, name: &str, messages: &[TeamMessage]) -> (bool, Vec<String>) {
        let mut notices = Vec::new();
        for message in messages {
            match message.kind.as_str() {
                MSG_SHUTDOWN_REQUEST => {
                    let (accepted, request_id) = self.apply_shutdown_request(name, message);
                    let Some(request_id) = request_id else {
                        notices.push("[Ignored shutdown request: request mismatch]".to_string());
                        continue;
                    };
                    if accepted {
                        let _ = self.inner.bus.send(
                            &TeamMessage::new(
                                name,
                                LEAD,
                                "Shutdown acknowledged.",
                                MSG_SHUTDOWN_RESPONSE,
                            )
                            .with_request(request_id)
                            .with_approval(true),
                        );
                        return (true, notices);
                    }
                }
                MSG_PLAN_APPROVAL_RESPONSE => {
                    let (_, notice) = self.apply_plan_response(name, message);
                    notices.push(notice);
                }
                MSG_PLAN_REQUEST => notices.push(format!("[Plan required] {}", message.content)),
                _ => notices.push(format!(
                    "[Message from {}] {}",
                    message.from, message.content
                )),
            }
        }
        (false, notices)
    }
}

/// Lead operations: spawning, messaging, protocols and the shared board.
impl Team {
    /// Spawn a persistent teammate, claiming its initial task first.
    pub fn spawn_teammate(
        &self,
        name: &str,
        role: &str,
        prompt: &str,
        task_id: Option<&str>,
        require_plan: bool,
    ) -> Result<String> {
        if !is_valid_agent_name(name) {
            return Err(Error::Team(
                "invalid teammate name: use 1-64 letters, digits, underscores, or dashes".into(),
            ));
        }
        if is_reserved_agent_name(name) {
            return Err(Error::Team(format!(
                "teammate name `{name}` is reserved by the runtime"
            )));
        }
        {
            let mut state = self.lock_state();
            if state
                .teammates
                .keys()
                .any(|existing| existing.eq_ignore_ascii_case(name))
            {
                return Err(Error::Team(format!("teammate `{name}` already exists")));
            }
            state
                .teammates
                .insert(name.to_string(), TeammateStatus::Working);
            state.gates.insert(
                name.to_string(),
                if require_plan {
                    PlanGate::Required
                } else {
                    PlanGate::NotRequired
                },
            );
            state.versions.insert(name.to_string(), 0);
        }

        if let Some(task_id) = task_id {
            if task_id.is_empty() {
                self.forget(name);
                return Err(Error::Team("task_id cannot be empty".into()));
            }
            let claimed = {
                let _board = self.lock_board();
                self.tasks().claim(task_id, name)
            };
            let claimed = match claimed {
                Ok(message) => message,
                Err(error) => {
                    self.forget(name);
                    return Err(Error::Team(format!(
                        "cannot spawn teammate `{name}`: {error}"
                    )));
                }
            };
            if !claimed.starts_with("Claimed ") {
                self.forget(name);
                return Err(Error::Team(format!(
                    "cannot spawn teammate `{name}`: {claimed}"
                )));
            }
            let mut state = self.lock_state();
            state
                .assignments
                .insert(name.to_string(), task_id.to_string());
        }
        self.bump_version(name);

        let assignment = self.assignment(name);
        self.start_teammate_thread(name, role, prompt, assignment);
        let assigned = task_id
            .map(|id| format!(" for {id}"))
            .unwrap_or_else(|| " without an initial Task".to_string());
        Ok(format!(
            "Teammate '{name}' spawned as {role}{assigned}. End this turn; the runtime \
             will deliver its events."
        ))
    }

    /// One line per active teammate.
    pub fn list_teammates(&self) -> String {
        let teammates = self.teammates();
        if teammates.is_empty() {
            return "No active teammates.".to_string();
        }
        teammates
            .iter()
            .map(|(name, status)| format!("{name}: {}", status.name()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Send a plain message between participants.
    pub fn send_message(&self, from: &str, to: &str, content: &str) -> Result<String> {
        let allowed = if from == LEAD {
            self.status(to).is_some()
        } else {
            to == LEAD || self.status(to).is_some()
        };
        if !allowed {
            return Err(Error::Team(format!("teammate `{to}` is not active")));
        }
        self.inner
            .bus
            .send(&TeamMessage::new(from, to, content, MSG_MESSAGE))?;
        Ok(format!("Sent to {to}"))
    }

    /// Ask a teammate to finish its step and stop.
    pub fn request_shutdown(&self, teammate: &str) -> Result<String> {
        if self.status(teammate).is_none() {
            return Err(Error::Team(format!("teammate `{teammate}` is not active")));
        }
        let request_id = self.new_request_id();
        let mut state = self.lock_state();
        state.protocol.insert(
            request_id.clone(),
            ProtocolState {
                request_id: request_id.clone(),
                kind: RequestKind::Shutdown,
                sender: LEAD.to_string(),
                target: teammate.to_string(),
                status: RequestStatus::Pending,
                payload: String::new(),
                work_version: 0,
                task_id: None,
            },
        );
        drop(state);
        self.inner.bus.send(
            &TeamMessage::new(
                LEAD,
                teammate,
                "Finish the current step and shut down.",
                MSG_SHUTDOWN_REQUEST,
            )
            .with_request(request_id.clone()),
        )?;
        Ok(format!("Shutdown requested from {teammate} ({request_id})"))
    }

    /// Demand a plan before the teammate may change the workspace.
    pub fn request_plan(&self, teammate: &str, task: &str) -> Result<String> {
        if self.status(teammate).is_none() {
            return Err(Error::Team(format!("teammate `{teammate}` is not active")));
        }
        self.lock_state()
            .gates
            .insert(teammate.to_string(), PlanGate::Required);
        self.inner
            .bus
            .send(&TeamMessage::new(LEAD, teammate, task, MSG_PLAN_REQUEST))?;
        Ok(format!("Plan requested from {teammate}"))
    }

    /// Approve or reject the teammate's current plan.
    pub fn review_plan(&self, request_id: &str, approve: bool, feedback: &str) -> Result<String> {
        let sender = {
            let mut state = self.lock_state();
            let Some(request) = state.protocol.get_mut(request_id) else {
                return Err(Error::Team(format!("request {request_id} not found")));
            };
            if request.kind != RequestKind::PlanApproval {
                return Err(Error::Team(format!("request {request_id} is not a plan")));
            }
            if request.status != RequestStatus::Pending {
                return Err(Error::Team(format!(
                    "request {request_id} already {}",
                    request.status.name()
                )));
            }
            request.sender.clone()
        };

        let (version, task_id) = self.current_work_identity(&sender);
        {
            let mut state = self.lock_state();
            let Some(request) = state.protocol.get(request_id).cloned() else {
                return Err(Error::Team(format!("request {request_id} not found")));
            };
            if request.work_version != version || request.task_id != task_id {
                return Err(Error::Team(format!(
                    "request {request_id} belongs to an earlier assignment"
                )));
            }
            if state.plan_requests.get(&sender).map(String::as_str) != Some(request_id) {
                return Err(Error::Team(format!(
                    "request {request_id} is not the current plan"
                )));
            }
            state.protocol.get_mut(request_id).expect("checked").status = if approve {
                RequestStatus::Approved
            } else {
                RequestStatus::Rejected
            };
        }

        let content = if !feedback.is_empty() {
            feedback.to_string()
        } else if approve {
            "Plan approved.".to_string()
        } else {
            "Revise the plan and submit it again.".to_string()
        };
        self.inner.bus.send(
            &TeamMessage::new(LEAD, &sender, content, MSG_PLAN_APPROVAL_RESPONSE)
                .with_request(request_id)
                .with_approval(approve),
        )?;
        Ok(format!(
            "Plan {} ({request_id})",
            if approve { "approved" } else { "rejected" }
        ))
    }

    /// Create `.worktrees/<name>` with git and bind it to `task_id`.
    pub fn create_worktree(&self, name: &str, task_id: &str) -> Result<String> {
        if !is_valid_worktree_name(name) {
            return Err(Error::Team(format!("invalid worktree name: {name:?}")));
        }
        let store = self.tasks();
        let _board = self.lock_board();
        store.load(task_id)?;
        let dir = self.inner.workspace.join(WORKTREES_DIR).join(name);
        if !dir.exists() {
            fs::create_dir_all(self.inner.workspace.join(WORKTREES_DIR))?;
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.inner.workspace)
                .args(["worktree", "add", "-b", &format!("teammate/{name}")])
                .arg(&dir)
                .output()
                .map_err(|error| Error::Team(format!("could not run git: {error}")))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(Error::Team(format!(
                    "git worktree add failed: {}",
                    stderr.trim()
                )));
            }
        }
        store.bind_worktree(task_id, name)?;
        Ok(format!(
            "Worktree '{name}' at {} bound to {task_id}",
            dir.display()
        ))
    }

    /// The directory a task's tools run in.
    pub fn assignment_cwd(&self, task: &Task) -> PathBuf {
        match task.worktree.as_deref() {
            Some(name) if is_valid_worktree_name(name) => {
                self.inner.workspace.join(WORKTREES_DIR).join(name)
            }
            _ => self.inner.workspace.clone(),
        }
    }

    /// Claim the first ready task that can use `cwd`, if the teammate is free.
    pub fn claim_next_task(&self, name: &str, cwd: &Path) -> Option<Task> {
        let _board = self.lock_board();
        let busy = {
            let state = self.lock_state();
            state.assignments.contains_key(name)
        };
        if busy || self.owns_in_progress(name) {
            return None;
        }
        for task in self.ready_tasks(cwd) {
            if self
                .tasks()
                .claim(&task.id, name)
                .map(|message| message.starts_with("Claimed "))
                .unwrap_or(false)
            {
                self.bump_version(name);
                self.lock_state()
                    .assignments
                    .insert(name.to_string(), task.id.clone());
                return Some(task);
            }
        }
        None
    }

    /// Ready tasks (pending, unowned, unblocked) whose cwd matches.
    fn ready_tasks(&self, cwd: &Path) -> Vec<Task> {
        let store = self.tasks();
        let Ok(tasks) = store.list() else {
            return Vec::new();
        };
        tasks
            .into_iter()
            .filter(|task| {
                task.status == TaskStatus::Pending
                    && task.owner.is_none()
                    && self.assignment_cwd(task) == cwd
                    && store.can_start(&task.id).unwrap_or(false)
            })
            .collect()
    }

    /// Whether `name` already owns a task in progress.
    fn owns_in_progress(&self, name: &str) -> bool {
        self.tasks()
            .list()
            .map(|tasks| {
                tasks.into_iter().any(|task| {
                    task.owner.as_deref() == Some(name) && task.status == TaskStatus::InProgress
                })
            })
            .unwrap_or(false)
    }

    /// Clear the assignment once its task is completed.
    fn release_completed_assignment(&self, name: &str) -> bool {
        let Some(task_id) = self.assignment(name) else {
            return false;
        };
        let completed = self
            .tasks()
            .load(&task_id)
            .map(|task| task.status == TaskStatus::Completed)
            .unwrap_or(false);
        if completed {
            self.clear_assignment(name);
        }
        completed
    }

    /// Give a stopping teammate's task back to the board.
    fn release_teammate_assignment(&self, name: &str) {
        if let Some(task_id) = self.assignment(name) {
            let _board = self.lock_board();
            if let Ok(task) = self.tasks().load(&task_id) {
                if task.status == TaskStatus::InProgress && task.owner.as_deref() == Some(name) {
                    let _ = self.tasks().release(&task_id);
                }
            }
        }
        self.clear_assignment(name);
    }
}

/// What a teammate should do after a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkOutcome {
    Continue,
    Idle,
    Stop,
}

/// A teammate's system prompt: identity, the task contract, the plan gate.
pub fn teammate_system_prompt(name: &str, role: &str) -> String {
    format!(
        "You are '{name}', a {role}. Use tools to complete the assigned Task, then call \
         complete_task and report a concise result. If the first user message contains \
         [Assigned task], that Task is already claimed; do not call claim_task for it again. \
         When asked for a plan, call submit_plan and wait for approval before bash or file \
         changes. File and shell tools use the Task's working directory; that directory is not \
         a sandbox. The runtime delivers your final text to Lead. Use send_message only for \
         intermediate coordination, and address the coordinator as 'lead'."
    )
}

/// `PreToolUse` gate: no workspace changes until the plan is approved.
struct PlanGateHook {
    team: Team,
    name: String,
}

impl Hook for PlanGateHook {
    fn pre_tool_use(&self, call: &ToolCall) -> Option<String> {
        if !is_mutating_tool(&call.name) {
            return None;
        }
        let gate = self.team.gate(&self.name);
        if gate.allows_changes() {
            return None;
        }
        Some(format!(
            "Blocked: plan status is {}. Submit or revise the plan and wait for approval \
             before changing the workspace.",
            gate.name()
        ))
    }
}

/// The last assistant text in a conversation, trimmed.
fn last_assistant_text(history: &[Message]) -> String {
    history
        .iter()
        .rev()
        .find(|message| message.role == crate::message::Role::Assistant)
        .map(|message| message.text().trim().to_string())
        .unwrap_or_default()
}

/// One persistent teammate: its own conversation, tools and working directory.
struct TeammateRuntime {
    team: Team,
    name: String,
    cwd: PathBuf,
    agent: Agent,
    history: Vec<Message>,
}

impl TeammateRuntime {
    fn new(
        team: Team,
        name: String,
        role: String,
        prompt: String,
        task_id: Option<String>,
        cwd: PathBuf,
        observer: Arc<dyn AgentObserver>,
    ) -> Self {
        let mut tools = ToolRegistry::base();
        tools.register(ListTasksTool);
        tools.register(ClaimTaskTool::owned_by(&name));
        tools.register(CompleteTaskTool::owned_by(&name));
        register_teammate_tools(&mut tools, team.clone(), name.clone());

        let agent = Agent::builder()
            .shared_client(team.client())
            .tools(tools)
            .workspace(cwd.clone())
            .hook(PlanGateHook {
                team: team.clone(),
                name: name.clone(),
            })
            .shared_observer(observer)
            .max_tokens(team.max_tokens())
            .max_steps(team.max_steps())
            .system_prompt(teammate_system_prompt(&name, &role))
            .build();

        let mut opening = prompt;
        if let Some(task_id) = &task_id {
            if let Ok(task) = team.tasks().load(task_id) {
                opening.push_str(&format!(
                    "\n\n[Assigned task {}] {}\n{}\nWork directory: {}",
                    task.id,
                    task.subject,
                    task.description,
                    cwd.display()
                ));
            }
        }
        if team.gate(&name) == PlanGate::Required {
            opening.push_str(
                "\n\n[Plan required] Submit a plan and wait for Lead approval before \
                 changing files or using bash.",
            );
        }

        TeammateRuntime {
            team,
            name,
            cwd,
            agent,
            history: vec![Message::user_text(opening)],
        }
    }

    fn push_user_lines(&mut self, lines: Vec<String>) {
        if !lines.is_empty() {
            self.history.push(Message::user_text(lines.join("\n")));
        }
    }

    fn report_error(&self, message: &str) {
        let _ = self
            .team
            .bus()
            .send(&TeamMessage::new(&self.name, LEAD, message, MSG_ERROR));
    }

    /// Run one model turn; decide whether to continue, idle or stop.
    async fn work(&mut self) -> WorkOutcome {
        let inbox = match self.team.bus().read_inbox(&self.name) {
            Ok(messages) => messages,
            Err(error) => {
                self.report_error(&error.to_string());
                return WorkOutcome::Stop;
            }
        };
        let (stop, notices) = self.team.handle_inbox(&self.name, &inbox);
        self.push_user_lines(notices);
        if stop {
            return WorkOutcome::Stop;
        }

        self.team.set_status(&self.name, TeammateStatus::Working);
        if let Err(error) = self.agent.run_turn(&mut self.history).await {
            self.report_error(&error.to_string());
            return WorkOutcome::Stop;
        }

        let summary = last_assistant_text(&self.history);
        let gate = self.team.gate(&self.name);
        if gate != PlanGate::Pending && !summary.is_empty() {
            let _ = self
                .team
                .bus()
                .send(&TeamMessage::new(&self.name, LEAD, summary, MSG_RESULT));
        }
        if gate == PlanGate::Pending {
            self.team
                .set_status(&self.name, TeammateStatus::WaitingApproval);
            return WorkOutcome::Idle;
        }

        self.team.release_completed_assignment(&self.name);
        self.team.set_status(&self.name, TeammateStatus::Idle);
        let _ = self.team.bus().send(&TeamMessage::new(
            &self.name,
            LEAD,
            "Waiting for more work.",
            MSG_IDLE,
        ));
        WorkOutcome::Idle
    }

    /// Wait for a message or atomically claim the next ready task.
    fn wait_for_work(&mut self) -> bool {
        loop {
            let inbox = self
                .team
                .bus()
                .wait_for_messages(&self.name, IDLE_SCAN_INTERVAL)
                .unwrap_or_default();
            if !inbox.is_empty() {
                let (stop, notices) = self.team.handle_inbox(&self.name, &inbox);
                if stop {
                    return false;
                }
                self.push_user_lines(notices);
                return true;
            }

            let Some(task) = self.team.claim_next_task(&self.name, &self.cwd) else {
                continue;
            };
            self.push_user_lines(vec![format!(
                "[Auto-claimed task {}] {}\n{}\nWork directory: {}",
                task.id,
                task.subject,
                task.description,
                self.cwd.display()
            )]);
            return true;
        }
    }

    async fn run(&mut self) {
        let mut outcome = WorkOutcome::Continue;
        while outcome != WorkOutcome::Stop {
            if outcome == WorkOutcome::Idle && !self.wait_for_work() {
                break;
            }
            outcome = self.work().await;
        }
        self.team.release_teammate_assignment(&self.name);
        self.team.forget(&self.name);
    }
}

/// Thread entry point for one teammate.
fn run_teammate(
    team: Team,
    name: String,
    role: String,
    prompt: String,
    task_id: Option<String>,
    cwd: PathBuf,
    observer: Arc<dyn AgentObserver>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = team
                .bus()
                .send(&TeamMessage::new(&name, LEAD, error.to_string(), MSG_ERROR));
            team.forget(&name);
            return;
        }
    };
    runtime.block_on(async move {
        let mut teammate = TeammateRuntime::new(team, name, role, prompt, task_id, cwd, observer);
        teammate.run().await;
    });
}

impl Team {
    fn start_teammate_thread(&self, name: &str, role: &str, prompt: &str, task_id: Option<String>) {
        let cwd = task_id
            .as_ref()
            .and_then(|id| self.tasks().load(id).ok())
            .map(|task| self.assignment_cwd(&task))
            .unwrap_or_else(|| self.inner.workspace.clone());
        let team = self.clone();
        let name = name.to_string();
        let role = role.to_string();
        let prompt = prompt.to_string();
        let observer = self.observer();
        std::thread::spawn(move || run_teammate(team, name, role, prompt, task_id, cwd, observer));
    }

    /// The client teammates share with the Lead.
    pub fn client(&self) -> Arc<dyn LlmClient> {
        Arc::clone(&self.inner.client)
    }

    /// Per-call response budget for teammate turns.
    pub fn max_tokens(&self) -> u32 {
        self.inner.max_tokens.load(Ordering::Relaxed) as u32
    }

    /// Safety bound on teammate loop iterations per turn.
    pub fn max_steps(&self) -> usize {
        self.inner.max_steps.load(Ordering::Relaxed) as usize
    }
}

/// `send_message`: Lead to teammate, or teammate to Lead / another teammate.
pub struct SendMessageTool {
    team: Team,
    from: String,
}

impl SendMessageTool {
    /// A sender-scoped message tool.
    pub fn new(team: Team, from: impl Into<String>) -> Self {
        SendMessageTool {
            team,
            from: from.into(),
        }
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        SEND_MESSAGE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Send a message to 'lead' or to an active teammate."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["to", "content"],
            "additionalProperties": false
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        let to = required_str(&input, "to")?;
        let content = required_str(&input, "content")?;
        self.team.send_message(&self.from, to, content)
    }
}

/// `submit_plan`: a teammate asks the Lead to approve its plan.
pub struct SubmitPlanTool {
    team: Team,
    name: String,
}

impl SubmitPlanTool {
    /// Bind the tool to the teammate that owns it.
    pub fn new(team: Team, name: impl Into<String>) -> Self {
        SubmitPlanTool {
            team,
            name: name.into(),
        }
    }
}

#[async_trait]
impl Tool for SubmitPlanTool {
    fn name(&self) -> &str {
        SUBMIT_PLAN_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Submit a work plan for Lead approval; mutating tools stay blocked until it is approved."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "plan": { "type": "string", "minLength": 1 } },
            "required": ["plan"],
            "additionalProperties": false
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        let plan = required_str(&input, "plan")?;
        self.team.submit_plan(&self.name, plan)
    }
}

/// `spawn_teammate`: Lead starts one persistent teammate.
pub struct SpawnTeammateTool {
    team: Team,
}

impl SpawnTeammateTool {
    /// Bind the tool to a team.
    pub fn new(team: Team) -> Self {
        SpawnTeammateTool { team }
    }
}

#[async_trait]
impl Tool for SpawnTeammateTool {
    fn name(&self) -> &str {
        SPAWN_TEAMMATE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Spawn a persistent teammate that works the shared task board until shut down."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "pattern": "^[A-Za-z0-9_-]{1,64}$" },
                "role": { "type": "string" },
                "prompt": { "type": "string" },
                "task_id": { "type": "string", "pattern": "^task_[0-9a-f]{8}$" },
                "require_plan": { "type": "boolean" }
            },
            "required": ["name", "role", "prompt"],
            "additionalProperties": false
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        let name = required_str(&input, "name")?;
        let role = required_str(&input, "role")?;
        let prompt = required_str(&input, "prompt")?;
        let task_id = input.get("task_id").and_then(Value::as_str);
        let require_plan = input
            .get("require_plan")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.team
            .spawn_teammate(name, role, prompt, task_id, require_plan)
    }
}

/// `list_teammates`: status of every active teammate.
pub struct ListTeammatesTool {
    team: Team,
}

impl ListTeammatesTool {
    /// Bind the tool to a team.
    pub fn new(team: Team) -> Self {
        ListTeammatesTool { team }
    }
}

#[async_trait]
impl Tool for ListTeammatesTool {
    fn name(&self) -> &str {
        LIST_TEAMMATES_TOOL_NAME
    }

    fn description(&self) -> &str {
        "List active teammates and their status."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn call(&self, _ctx: &ToolContext, _input: Value) -> Result<String> {
        Ok(self.team.list_teammates())
    }
}

/// `request_shutdown`: begin the typed shutdown handshake.
pub struct RequestShutdownTool {
    team: Team,
}

impl RequestShutdownTool {
    /// Bind the tool to a team.
    pub fn new(team: Team) -> Self {
        RequestShutdownTool { team }
    }
}

#[async_trait]
impl Tool for RequestShutdownTool {
    fn name(&self) -> &str {
        REQUEST_SHUTDOWN_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Ask a teammate to finish its step and shut down."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "teammate": { "type": "string" } },
            "required": ["teammate"],
            "additionalProperties": false
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        self.team
            .request_shutdown(required_str(&input, "teammate")?)
    }
}

/// `request_plan`: gate a teammate behind a plan review.
pub struct RequestPlanTool {
    team: Team,
}

impl RequestPlanTool {
    /// Bind the tool to a team.
    pub fn new(team: Team) -> Self {
        RequestPlanTool { team }
    }
}

#[async_trait]
impl Tool for RequestPlanTool {
    fn name(&self) -> &str {
        REQUEST_PLAN_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Require a teammate to submit a plan before it may change the workspace."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "teammate": { "type": "string" },
                "task": { "type": "string" }
            },
            "required": ["teammate", "task"],
            "additionalProperties": false
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        let teammate = required_str(&input, "teammate")?;
        let task = required_str(&input, "task")?;
        self.team.request_plan(teammate, task)
    }
}

/// `review_plan`: approve or reject a pending plan.
pub struct ReviewPlanTool {
    team: Team,
}

impl ReviewPlanTool {
    /// Bind the tool to a team.
    pub fn new(team: Team) -> Self {
        ReviewPlanTool { team }
    }
}

#[async_trait]
impl Tool for ReviewPlanTool {
    fn name(&self) -> &str {
        REVIEW_PLAN_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Approve or reject a teammate's plan by request_id."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "request_id": { "type": "string" },
                "approve": { "type": "boolean" },
                "feedback": { "type": "string" }
            },
            "required": ["request_id", "approve"],
            "additionalProperties": false
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        let request_id = required_str(&input, "request_id")?;
        let approve = input
            .get("approve")
            .and_then(Value::as_bool)
            .ok_or_else(|| Error::Team("missing boolean argument `approve`".into()))?;
        let feedback = input
            .get("feedback")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.team.review_plan(request_id, approve, feedback)
    }
}

/// `create_worktree`: give a task its own working directory.
pub struct CreateWorktreeTool {
    team: Team,
}

impl CreateWorktreeTool {
    /// Bind the tool to a team.
    pub fn new(team: Team) -> Self {
        CreateWorktreeTool { team }
    }
}

#[async_trait]
impl Tool for CreateWorktreeTool {
    fn name(&self) -> &str {
        CREATE_WORKTREE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Create a git worktree under .worktrees/ and bind it to a task."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "pattern": "^(?!.*\\.\\.)[A-Za-z0-9][A-Za-z0-9._-]{0,63}$"
                },
                "task_id": { "type": "string", "pattern": "^task_[0-9a-f]{8}$" }
            },
            "required": ["name", "task_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        let name = required_str(&input, "name")?;
        let task_id = required_str(&input, "task_id")?;
        self.team.create_worktree(name, task_id)
    }
}

/// Register the Lead's team tools.
pub fn register_team_tools(registry: &mut ToolRegistry, team: Team) {
    registry.register(SpawnTeammateTool::new(team.clone()));
    registry.register(ListTeammatesTool::new(team.clone()));
    registry.register(RequestShutdownTool::new(team.clone()));
    registry.register(RequestPlanTool::new(team.clone()));
    registry.register(ReviewPlanTool::new(team.clone()));
    registry.register(CreateWorktreeTool::new(team.clone()));
    registry.register(SendMessageTool::new(team, LEAD));
}

/// Register the teammate-side team tools.
pub fn register_teammate_tools(registry: &mut ToolRegistry, team: Team, name: String) {
    registry.register(SendMessageTool::new(team.clone(), name.clone()));
    registry.register(SubmitPlanTool::new(team, name));
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::llm::{CompletionRequest, CompletionResponse};
    use crate::message::ContentBlock;

    /// Replays a scripted list of responses; extra calls are an error.
    struct ScriptedClient {
        responses: Mutex<VecDeque<CompletionResponse>>,
    }

    impl ScriptedClient {
        fn scripted(responses: Vec<CompletionResponse>) -> Arc<dyn LlmClient> {
            Arc::new(ScriptedClient {
                responses: Mutex::new(responses.into()),
            })
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn complete(&self, _request: CompletionRequest<'_>) -> Result<CompletionResponse> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::Config("no scripted response left".into()))
        }
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

    fn text(value: &str) -> CompletionResponse {
        response(vec![ContentBlock::text(value)], "end_turn")
    }

    fn fixture() -> (tempfile::TempDir, Team) {
        let dir = tempfile::tempdir().unwrap();
        let team = Team::new(dir.path(), ScriptedClient::scripted(vec![]));
        (dir, team)
    }

    fn call(name: &str, input: Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: name.into(),
            input,
        }
    }

    /// Wait until `lead` has received a message of `kind`.
    fn wait_for_lead(team: &Team, kind: &str, messages: &mut Vec<TeamMessage>) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            let batch = team
                .bus()
                .wait_for_messages(LEAD, Duration::from_millis(200))
                .unwrap();
            messages.extend(batch);
            if messages.iter().any(|message| message.kind == kind) {
                return true;
            }
        }
        false
    }

    /// Wait until the lead hears a message of `kind` with `needle` in it.
    fn wait_for_lead_text(team: &Team, needle: &str, messages: &mut Vec<TeamMessage>) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            let batch = team
                .bus()
                .wait_for_messages(LEAD, Duration::from_millis(200))
                .unwrap();
            messages.extend(batch);
            if messages
                .iter()
                .any(|message| message.content.contains(needle))
            {
                return true;
            }
        }
        false
    }

    #[test]
    fn the_bus_drains_an_inbox_and_rejects_bad_names() {
        let (_dir, team) = fixture();
        let bus = team.bus();
        assert!(!bus.peek(LEAD).unwrap());

        bus.send(&TeamMessage::new("alice", LEAD, "hello", MSG_MESSAGE))
            .unwrap();
        bus.send(&TeamMessage::new("alice", LEAD, "done", MSG_RESULT))
            .unwrap();
        assert!(bus.peek(LEAD).unwrap());

        let messages = bus.read_inbox(LEAD).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "hello");
        assert_eq!(messages[1].kind, MSG_RESULT);
        assert_eq!(messages[1].from, "alice");
        // The drain is destructive.
        assert!(bus.read_inbox(LEAD).unwrap().is_empty());
        assert!(bus
            .send(&TeamMessage::new("alice", "bad name", "x", MSG_MESSAGE))
            .is_err());
    }

    #[test]
    fn a_waiter_wakes_on_a_message_and_times_out_when_idle() {
        let (_dir, team) = fixture();
        let bus = team.bus().clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            bus.send(&TeamMessage::new("alice", LEAD, "late", MSG_MESSAGE))
                .unwrap();
        });

        let messages = team
            .bus()
            .wait_for_messages(LEAD, Duration::from_secs(5))
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "late");

        let nothing = team
            .bus()
            .wait_for_messages("nobody", Duration::from_millis(50))
            .unwrap();
        assert!(nothing.is_empty());
    }

    #[test]
    fn names_and_worktree_labels_are_validated() {
        assert!(is_valid_agent_name("alice"));
        assert!(is_valid_agent_name("a_b-c9"));
        assert!(!is_valid_agent_name(""));
        assert!(!is_valid_agent_name("bad name"));
        assert!(!is_valid_agent_name(&"a".repeat(65)));
        assert!(is_reserved_agent_name("lead"));
        assert!(is_reserved_agent_name("Agent"));
        assert!(!is_reserved_agent_name("alice"));

        assert!(is_valid_worktree_name("auth-refactor"));
        assert!(!is_valid_worktree_name(".."));
        assert!(!is_valid_worktree_name("a/b"));
        assert!(!is_valid_worktree_name("-dash"));
    }

    #[test]
    fn the_plan_gate_blocks_changes_until_the_lead_approves() {
        let (_dir, team) = fixture();
        team.lock_state()
            .teammates
            .insert("alice".to_string(), TeammateStatus::Working);
        team.request_plan("alice", "plan the refactor").unwrap();
        assert_eq!(team.gate("alice"), PlanGate::Required);

        let hook = PlanGateHook {
            team: team.clone(),
            name: "alice".to_string(),
        };
        let bash = call("bash", json!({ "command": "ls" }));
        assert!(hook.pre_tool_use(&bash).is_some());
        assert!(hook.pre_tool_use(&call("read_file", json!({}))).is_none());

        let requests = team.bus().read_inbox("alice").unwrap();
        assert_eq!(requests[0].kind, MSG_PLAN_REQUEST);

        let notice = team.submit_plan("alice", "1. read  2. edit").unwrap();
        assert!(notice.contains("req_"), "{notice}");
        assert_eq!(team.gate("alice"), PlanGate::Pending);
        assert!(hook.pre_tool_use(&bash).is_some());

        let submitted = team.bus().read_inbox(LEAD).unwrap();
        assert_eq!(submitted[0].kind, MSG_PLAN_APPROVAL_REQUEST);
        let request_id = submitted[0].request_id.clone().unwrap();

        // An unknown request id is not reviewable.
        assert!(team.review_plan("req_999999", true, "").is_err());
        assert!(team.review_plan(&request_id, true, "").is_ok());
        assert_eq!(
            team.request_status(&request_id),
            Some(RequestStatus::Approved)
        );

        let replies = team.bus().read_inbox("alice").unwrap();
        let (stop, notices) = team.handle_inbox("alice", &replies);
        assert!(!stop);
        assert!(notices[0].contains("[Plan approved]"), "{notices:?}");
        assert_eq!(team.gate("alice"), PlanGate::Approved);
        assert!(hook.pre_tool_use(&bash).is_none());
    }

    #[test]
    fn a_rejected_or_stale_plan_keeps_the_gate_closed() {
        let (_dir, team) = fixture();
        team.submit_plan("alice", "risky plan").unwrap();
        let request_id = team.bus().read_inbox(LEAD).unwrap()[0]
            .request_id
            .clone()
            .unwrap();
        assert!(team.review_plan(&request_id, false, "too broad").is_ok());

        let replies = team.bus().read_inbox("alice").unwrap();
        let (_, notices) = team.handle_inbox("alice", &replies);
        assert!(notices[0].contains("[Plan rejected]"), "{notices:?}");
        assert_eq!(team.gate("alice"), PlanGate::Rejected);
        assert!(!PlanGate::Rejected.allows_changes());

        // A plan from an earlier assignment cannot be approved.
        team.submit_plan("alice", "second plan").unwrap();
        let second = team.bus().read_inbox(LEAD).unwrap()[0]
            .request_id
            .clone()
            .unwrap();
        team.bump_version("alice");
        assert!(team.review_plan(&second, true, "").is_err());
    }

    #[test]
    fn the_shutdown_handshake_matches_exactly_once() {
        let (_dir, team) = fixture();
        team.lock_state()
            .teammates
            .insert("bob".to_string(), TeammateStatus::Idle);
        assert!(team.request_shutdown("lead").is_err());

        let notice = team.request_shutdown("bob").unwrap();
        let request_id = notice
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .to_string();
        assert!(request_id.starts_with("req_"), "{request_id}");

        let delivered = team.bus().read_inbox("bob").unwrap();
        assert_eq!(delivered[0].kind, MSG_SHUTDOWN_REQUEST);
        let (stop, notices) = team.handle_inbox("bob", &delivered);
        assert!(stop && notices.is_empty());
        assert_eq!(team.status("bob"), Some(TeammateStatus::Stopping));
        // The same request never applies twice.
        assert_eq!(
            team.apply_shutdown_request("bob", &delivered[0]),
            (false, None)
        );

        let responses = team.consume_lead_inbox().unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].kind, MSG_SHUTDOWN_RESPONSE);
        assert_eq!(
            team.request_status(&request_id),
            Some(RequestStatus::Approved)
        );
        // Replaying the acknowledgement changes nothing.
        assert!(!team.match_response(MSG_SHUTDOWN_RESPONSE, &request_id, true, "bob", LEAD));
        assert!(team.bus().read_inbox(LEAD).unwrap().is_empty());
    }

    #[test]
    fn team_events_are_formatted_for_the_lead() {
        let messages = vec![
            TeamMessage::new("bob", LEAD, "auth done", MSG_RESULT),
            TeamMessage::new("bob", LEAD, "plan?", MSG_PLAN_APPROVAL_REQUEST)
                .with_request("req_000007"),
        ];
        let rendered = Team::format_team_events(&messages);
        assert!(rendered.starts_with("[Team events]"), "{rendered}");
        assert!(rendered.contains("[result] bob: auth done"), "{rendered}");
        assert!(rendered.contains("request_id=req_000007"), "{rendered}");
    }

    #[test]
    fn a_teammate_completes_its_task_and_idles_until_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let task = TaskStore::new(dir.path())
            .create("refactor auth", "extract the middleware")
            .unwrap();
        let client = ScriptedClient::scripted(vec![
            response(
                vec![tool_use(
                    "call_1",
                    "complete_task",
                    json!({ "task_id": task.id }),
                )],
                "tool_use",
            ),
            text("auth refactored"),
        ]);
        let team = Team::new(dir.path(), client);
        let spawned = team
            .spawn_teammate(
                "bob",
                "auth specialist",
                "Refactor auth.",
                Some(&task.id),
                false,
            )
            .unwrap();
        assert!(spawned.contains("bob"), "{spawned}");
        // The spawn claimed the task for the teammate before starting it.
        assert_eq!(team.assignment("bob").as_deref(), Some(task.id.as_str()));

        let mut messages = Vec::new();
        assert!(
            wait_for_lead(&team, MSG_IDLE, &mut messages),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.kind == MSG_RESULT
                    && message.content.contains("auth refactored")),
            "{messages:?}"
        );

        let stored = TaskStore::new(dir.path()).load(&task.id).unwrap();
        assert_eq!(stored.status, TaskStatus::Completed);
        assert_eq!(stored.owner.as_deref(), Some("bob"));
        // Completing the task released the assignment; the teammate idles.
        assert_eq!(team.status("bob"), Some(TeammateStatus::Idle));
        assert_eq!(team.assignment("bob"), None);

        let notice = team.request_shutdown("bob").unwrap();
        assert!(notice.contains("req_"), "{notice}");
        let mut ending = Vec::new();
        assert!(
            wait_for_lead(&team, MSG_SHUTDOWN_RESPONSE, &mut ending),
            "{ending:?}"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while team.status("bob").is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(team.status("bob").is_none(), "teammate did not stop");
    }

    #[test]
    fn an_idle_teammate_claims_the_next_ready_task() {
        let dir = tempfile::tempdir().unwrap();
        let task = TaskStore::new(dir.path())
            .create("add tests", "cover the middleware")
            .unwrap();
        let client = ScriptedClient::scripted(vec![
            text("ready for work"),
            response(
                vec![tool_use(
                    "call_1",
                    "complete_task",
                    json!({ "task_id": task.id }),
                )],
                "tool_use",
            ),
            text("tests added"),
        ]);
        let team = Team::new(dir.path(), client);
        team.spawn_teammate("carol", "test engineer", "Wait for work.", None, false)
            .unwrap();

        let mut messages = Vec::new();
        assert!(
            wait_for_lead_text(&team, "tests added", &mut messages),
            "{messages:?}"
        );
        let stored = TaskStore::new(dir.path()).load(&task.id).unwrap();
        assert_eq!(stored.status, TaskStatus::Completed);
        assert_eq!(stored.owner.as_deref(), Some("carol"));
        team.request_shutdown("carol").unwrap();
    }

    #[test]
    fn spawning_rejects_bad_names_and_unclaimable_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let task = TaskStore::new(dir.path()).create("t", "").unwrap();
        let team = Team::new(dir.path(), ScriptedClient::scripted(vec![]));

        assert!(team.spawn_teammate("lead", "x", "y", None, false).is_err());
        assert!(team
            .spawn_teammate("bad name", "x", "y", None, false)
            .is_err());
        assert!(team
            .spawn_teammate("dave", "x", "y", Some("task_00000000"), false)
            .is_err());
        // A failed spawn leaves nothing behind.
        assert_eq!(team.list_teammates(), "No active teammates.");

        assert!(team.send_message(LEAD, "ghost", "hi").is_err());
        let other = Team::new(dir.path(), ScriptedClient::scripted(vec![]));
        assert_eq!(other.list_teammates(), "No active teammates.");
        // The task is still claimable after the failed spawns.
        assert!(task.id.starts_with("task_"));
    }

    #[test]
    fn the_lead_tools_are_registered_with_schemas() {
        let (_dir, team) = fixture();
        let mut tools = ToolRegistry::new();
        register_team_tools(&mut tools, team);
        let names: Vec<String> = tools
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        for expected in [
            SPAWN_TEAMMATE_TOOL_NAME,
            LIST_TEAMMATES_TOOL_NAME,
            SEND_MESSAGE_TOOL_NAME,
            REQUEST_SHUTDOWN_TOOL_NAME,
            REQUEST_PLAN_TOOL_NAME,
            REVIEW_PLAN_TOOL_NAME,
            CREATE_WORKTREE_TOOL_NAME,
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing {expected}: {names:?}"
            );
        }
        assert!(!names.iter().any(|name| name == SUBMIT_PLAN_TOOL_NAME));
    }

    #[tokio::test]
    async fn the_team_tools_report_bad_input_to_the_model() {
        let (_dir, team) = fixture();
        let ctx = ToolContext::new(std::env::temp_dir());
        let mut tools = ToolRegistry::new();
        register_team_tools(&mut tools, team.clone());

        assert_eq!(
            tools
                .execute(&ctx, &call(LIST_TEAMMATES_TOOL_NAME, json!({})))
                .await
                .unwrap(),
            "No active teammates."
        );
        assert!(tools
            .execute(
                &ctx,
                &call(
                    SPAWN_TEAMMATE_TOOL_NAME,
                    json!({ "name": "lead", "role": "x", "prompt": "y" })
                )
            )
            .await
            .is_err());
        assert!(tools
            .execute(
                &ctx,
                &call(
                    SEND_MESSAGE_TOOL_NAME,
                    json!({ "to": "ghost", "content": "hi" })
                )
            )
            .await
            .is_err());
        assert!(tools
            .execute(
                &ctx,
                &call(
                    REVIEW_PLAN_TOOL_NAME,
                    json!({ "request_id": "req_000001", "approve": true })
                )
            )
            .await
            .is_err());
        assert!(tools
            .execute(
                &ctx,
                &call(
                    CREATE_WORKTREE_TOOL_NAME,
                    json!({ "name": "../escape", "task_id": "task_00000000" })
                )
            )
            .await
            .is_err());
    }

    #[test]
    fn a_worktree_binds_to_a_task_and_moves_its_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "init",
        ]);

        let store = TaskStore::new(dir.path());
        let task = store.create("auth", "refactor the middleware").unwrap();
        let team = Team::new(dir.path(), ScriptedClient::scripted(vec![]));
        let message = team.create_worktree("auth-refactor", &task.id).unwrap();
        assert!(message.contains("auth-refactor"), "{message}");

        let bound = store.load(&task.id).unwrap();
        assert_eq!(bound.worktree.as_deref(), Some("auth-refactor"));
        let cwd = team.assignment_cwd(&bound);
        assert_eq!(cwd, dir.path().join(WORKTREES_DIR).join("auth-refactor"));
        assert!(cwd.is_dir(), "{} should exist", cwd.display());
        // An unbound task keeps using the repository root.
        let plain = store.create("plain", "").unwrap();
        assert_eq!(team.assignment_cwd(&plain), dir.path());

        // A bad name never reaches git.
        assert!(team.create_worktree("../escape", &plain.id).is_err());
    }
}
