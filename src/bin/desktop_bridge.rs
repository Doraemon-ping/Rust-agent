//! Long-running desktop bridge for the Tauri app.
//!
//! Speaks newline-delimited JSON over stdin/stdout: requests come in, responses
//! and asynchronous events go out. `stdout` carries protocol messages and
//! nothing else -- every diagnostic goes to `stderr` as one JSON object per
//! line, so a host can parse either stream without guessing.
//!
//! It answers three methods about itself -- `runtime.status`, `runtime.ping`,
//! and `runtime.shutdown` -- building the snapshot from the runtime's own stores
//! (`SkillLoader`, `TaskStore`, `CronScheduler`, `MemoryStore`), and the methods
//! that do work: `chat.send` runs a prompt through the same `Agent` loop the CLI
//! drives and reports the turn as `chat.*` events while it happens, `chat.cancel`
//! stops that turn, `chat.history` hands the reported turns back to a page that
//! reloaded, and `sessions.list` / `sessions.new` / `sessions.open` decide which
//! conversation is being continued. `sessions.rename` and `sessions.delete` are
//! what the page's conversation menu does to them.
//!
//! A turn can also stop and ask the page something. The permission policy's
//! third gate and the `ask_user` tool both go out as `chat.permission` and
//! `chat.question`, and wait for `chat.approve` and `chat.answer` -- so a call
//! the policy flags is decided by the person at the window instead of being
//! refused on their behalf, and a question nobody answers is a failed tool
//! result rather than a turn that hangs. A turn the runtime started on its own,
//! with no page behind it, is refused as it always was.
//!
//! Cron jobs are the runtime's own (`CronScheduler`), so `cron.list`,
//! `cron.schedule`, and `cron.cancel` are a window onto that table rather than a
//! second one. A ticker polls the clock every second and, when a job is due and
//! the runtime is idle, starts a turn of its own: without it a job scheduled for
//! 09:00 would not fire until somebody happened to send a prompt. The loop
//! injects the `[Scheduled]` message itself, so the ticker never invents one.
//!
//! Conversations are written through the runtime's own `SessionStore`, so they
//! outlive the process: a restart -- and choosing another project restarts the
//! runtime -- resumes the conversation the user was reading instead of starting
//! an empty one. Where they are kept is the host's choice: `--sessions-dir` names
//! the directory, which is how the desktop app keeps them in its own store rather
//! than inside the project it happens to be working in. Without the flag they
//! stay under `<workspace>/.sessions`, where the runtime has always put them.
//!
//! The agent is built on the first prompt, not at startup, so a workspace with
//! no model credentials still starts and still answers for itself.
//!
//! `skills.list` and `skills.read` are a window onto the `SkillLoader` the loop
//! itself uses -- the same catalogue the system prompt carries, and the same
//! `SKILL.md` `load_skill` hands the model. `plugins.list` and `plugins.connect`
//! are the MCP registry: the servers this build ships, and the tools of the ones
//! already connected. The page connects a server through the same registry the
//! agent's tool pool reads, so a plugin the page installs is one the model can
//! immediately call.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::watch;

use rust_agent::agent::{AgentEvent, AgentObserver};
use rust_agent::subagent::{subagent_system_prompt, TaskTool};
use rust_agent::{
    default_system_prompt, is_session_id, is_valid_agent_id, is_valid_cron_id,
    is_valid_department_id, is_valid_task_id, last_run, load_plugin_specs, next_match,
    parse_frontmatter, validate_cron, Agent, AgentProfile, AnthropicClient, Approver, AskUserTool,
    Asker, ClaimTaskTool, CompactConfig, Compactor, CompleteTaskTool, Config, CreateTaskTool,
    CronJob, CronScheduler, Department, DepartmentMessage, Error, GetTaskTool, ListTasksTool,
    LlmClient, McpRegistry, MemoryConfig, MemoryStore, Message, RecoveryConfig, Role, Session,
    SessionStore, SessionSummary, Skill, SkillLoader, Task, TaskStatus, TaskStore, ToolCall,
    ToolRegistry, UpdateTaskTool, WorkforceStore, CRON_PROMPT, MAILBOXES_DIR, MAX_MESSAGE_CHARS,
    MCP_PROMPT, SESSIONS_DIR, SKILL_MANIFEST, TASKS_PROMPT, WORKFLOWS_DIR,
};

/// Wire revision of the protocol. Bumped whenever a message shape changes.
const PROTOCOL_VERSION: u32 = 1;

/// Hard cap on one inbound line, so a runaway host cannot exhaust memory.
const MAX_LINE_BYTES: usize = 1 << 20;

/// Every method this build answers, in the order we advertise them.
const METHODS: [&str; 34] = [
    "runtime.status",
    "runtime.ping",
    "runtime.shutdown",
    "chat.send",
    "chat.cancel",
    "chat.history",
    "chat.context",
    "chat.answer",
    "chat.approve",
    "sessions.list",
    "sessions.new",
    "sessions.open",
    "sessions.rename",
    "sessions.delete",
    "cron.list",
    "cron.schedule",
    "cron.cancel",
    "tasks.list",
    "tasks.create",
    "tasks.claim",
    "tasks.release",
    "tasks.complete",
    "departments.list",
    "departments.create",
    "departments.remove",
    "agents.add",
    "agents.remove",
    "departments.run",
    "departments.stop",
    "skills.list",
    "skills.read",
    "skills.import",
    "plugins.list",
    "plugins.connect",
];

/// The methods that take parameters. Everything else refuses them, so a host
/// that thinks it is talking to a newer bridge finds out at once.
fn takes_params(method: &str) -> bool {
    matches!(
        method,
        "chat.send"
            | "chat.answer"
            | "chat.approve"
            | "sessions.open"
            | "sessions.rename"
            | "sessions.delete"
            | "cron.schedule"
            | "cron.cancel"
            | "tasks.create"
            | "tasks.claim"
            | "tasks.release"
            | "tasks.complete"
            | "departments.create"
            | "departments.remove"
            | "agents.add"
            | "agents.remove"
            | "departments.run"
            | "departments.stop"
            | "skills.read"
            | "skills.import"
            | "plugins.connect"
    )
}

/// Exit codes, so the host can tell a bad invocation from a broken pipe.
const EXIT_OK: i32 = 0;
const EXIT_IO: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_WORKSPACE: i32 = 3;
/// The conversations' directory could not be used.
const EXIT_STORE: i32 = 4;
const USAGE: &str = "\
rust-agent desktop bridge

usage: desktop_bridge --workspace <dir> [--sessions-dir <dir>]

  --workspace <dir>    runtime workspace; must exist (default: the cwd)
  --sessions-dir <dir> where conversations are kept, created if missing
                       (default: <workspace>/.sessions)
  --help               print this text on stderr
  --version            print the version on stderr

Protocol: one JSON object per line on stdin, one JSON object per line on
stdout. Diagnostics go to stderr. See desktop/README.md for message shapes.
";

// ---------------------------------------------------------------------------
// Diagnostics: stderr only, one JSON object per line.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

fn log(level: Level, message: impl AsRef<str>) {
    let line = json!({
        "ts": Utc::now().to_rfc3339(),
        "level": level.as_str(),
        "target": "desktop_bridge",
        "message": message.as_ref(),
    });
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{line}");
}
// ---------------------------------------------------------------------------
// Protocol errors
// ---------------------------------------------------------------------------

const INVALID_JSON: &str = "invalid_json";
const INVALID_UTF8: &str = "invalid_utf8";
const INVALID_REQUEST: &str = "invalid_request";
const INVALID_PARAMS: &str = "invalid_params";
const MESSAGE_TOO_LARGE: &str = "message_too_large";
const UNKNOWN_METHOD: &str = "unknown_method";
const UNSUPPORTED_PROTOCOL_VERSION: &str = "unsupported_protocol_version";

/// A stable machine-readable failure, plus prose for a human log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProtocolError {
    code: String,
    message: String,
}

impl ProtocolError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        ProtocolError {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Inbound messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct Request {
    request_id: String,
    method: String,
    params: Value,
}

/// What one inbound line turned into.
#[derive(Debug, Clone, PartialEq)]
enum Parsed {
    Request(Request),
    /// The line could not be accepted. `request_id` is carried along whenever
    /// the line identified itself, so the host can still match the failure.
    Rejected {
        request_id: Option<String>,
        error: ProtocolError,
    },
}
fn parse_request(line: &str) -> Parsed {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            return Parsed::Rejected {
                request_id: None,
                error: ProtocolError::new(INVALID_JSON, format!("malformed JSON: {error}")),
            }
        }
    };

    let Value::Object(object) = value else {
        return Parsed::Rejected {
            request_id: None,
            error: ProtocolError::new(INVALID_REQUEST, "a protocol message must be a JSON object"),
        };
    };

    let request_id = object
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string);

    let reject = |code: &str, message: String| Parsed::Rejected {
        request_id: request_id.clone(),
        error: ProtocolError::new(code, message),
    };

    match object.get("type").and_then(Value::as_str) {
        // `type` is optional; a message that calls itself a request is one.
        None | Some("request") => {}
        Some(other) => {
            return reject(
                INVALID_REQUEST,
                format!("this bridge only accepts `type: \"request\"`; got `{other}`"),
            )
        }
    }

    match object.get("protocol_version").and_then(Value::as_u64) {
        Some(version) if version == u64::from(PROTOCOL_VERSION) => {}
        Some(version) => {
            return reject(
                UNSUPPORTED_PROTOCOL_VERSION,
                format!(
                    "protocol_version {version} is not supported; this bridge speaks \
                     {PROTOCOL_VERSION}"
                ),
            )
        }
        None => {
            return reject(
                UNSUPPORTED_PROTOCOL_VERSION,
                format!("protocol_version is required; this bridge speaks {PROTOCOL_VERSION}"),
            )
        }
    }
    let Some(request_id) = request_id.clone() else {
        return Parsed::Rejected {
            request_id: None,
            error: ProtocolError::new(INVALID_REQUEST, "request_id must be a non-empty string"),
        };
    };

    let Some(method) = object
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty())
        .map(str::to_string)
    else {
        return reject(
            INVALID_REQUEST,
            "method must be a non-empty string".to_string(),
        );
    };

    let params = object.get("params").cloned().unwrap_or(Value::Null);
    if !(params.is_null() || params.is_object()) {
        return reject(INVALID_PARAMS, "params must be a JSON object".to_string());
    }

    // Only `takes_params` methods take arguments. Refusing unknown keys on every
    // other method beats silently ignoring a host that thinks it is talking to a
    // newer bridge.
    if !takes_params(&method) {
        if let Some(map) = params.as_object() {
            if !map.is_empty() {
                let keys: Vec<&str> = map.keys().map(String::as_str).collect();
                return reject(
                    INVALID_PARAMS,
                    format!("`{method}` takes no parameters; got {}", keys.join(", ")),
                );
            }
        }
    }

    Parsed::Request(Request {
        request_id,
        method,
        params,
    })
}

// ---------------------------------------------------------------------------
// Outbound messages
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Response {
    protocol_version: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    request_id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ProtocolError>,
}
impl Response {
    fn ok(request_id: &str, result: Value) -> Self {
        Response {
            protocol_version: PROTOCOL_VERSION,
            kind: "response",
            request_id: request_id.to_string(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    fn failed(request_id: &str, error: ProtocolError) -> Self {
        Response {
            protocol_version: PROTOCOL_VERSION,
            kind: "response",
            request_id: request_id.to_string(),
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

/// An unattributable failure: there was no usable `request_id` to match on.
#[derive(Debug, Serialize)]
struct ErrorMessage {
    protocol_version: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    ts: String,
    error: ProtocolError,
}

impl ErrorMessage {
    fn new(error: ProtocolError) -> Self {
        ErrorMessage {
            protocol_version: PROTOCOL_VERSION,
            kind: "error",
            ts: Utc::now().to_rfc3339(),
            error,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventMessage {
    protocol_version: u32,
    #[serde(rename = "type")]
    kind: String,
    event: String,
    ts: String,
    data: Value,
}

fn event(name: &str, data: Value) -> EventMessage {
    EventMessage {
        protocol_version: PROTOCOL_VERSION,
        kind: "event".to_string(),
        event: name.to_string(),
        ts: Utc::now().to_rfc3339(),
        data,
    }
}

fn write_message<W: Write, M: Serialize>(out: &mut W, message: &M) -> io::Result<()> {
    let line = serde_json::to_string(message).map_err(io::Error::other)?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Write one protocol message to stdout.
///
/// The lock is taken per message rather than once for the process, because the
/// reader loop and whichever turn is running both report on stdout; holding it
/// for the loop's lifetime would block every turn's first event. `Stdout::lock`
/// is reentrant, so two threads can never interleave inside one line.
fn send<M: Serialize>(message: &M) -> io::Result<()> {
    write_message(&mut io::stdout().lock(), message)
}
// ---------------------------------------------------------------------------
// The bridge itself
// ---------------------------------------------------------------------------

/// Everything `runtime.status` reports, read from the runtime's own stores.
///
/// Nothing here is cached across calls: each request re-reads the workspace, so
/// the shell never shows stale counts.
struct Bridge {
    workspace: PathBuf,
    /// Where conversations are kept. The host decides this, so the desktop app
    /// can hold them in its own store instead of inside the project.
    sessions_dir: PathBuf,
    started: Instant,
    started_at: String,
    cron: CronScheduler,
    /// The MCP servers the runtime has connected. Shared with the chat, so a
    /// plugin the page installs is one the agent's tool pool already holds.
    mcp: McpRegistry,
    model: Option<String>,
    config_error: Option<String>,
    warnings: Vec<String>,
}

impl Bridge {
    fn new(workspace: PathBuf, sessions_dir: PathBuf) -> Self {
        let cron = CronScheduler::new(workspace.clone());
        let mut warnings = Vec::new();
        // The servers the workspace declares. Nothing is connected yet: a
        // server is connected by the page or by the model, and either way it is
        // this one registry that remembers it.
        let plugins = match load_plugin_specs(&workspace) {
            Ok(plugins) => plugins,
            Err(error) => {
                // Reported rather than fatal: a workspace with a broken
                // plugins.json still has a usable agent and built-in servers.
                warnings.push(format!("plugins: {error}"));
                Vec::new()
            }
        };
        let mcp = McpRegistry::new().with_plugins(plugins);
        match cron.load() {
            Ok(0) => {}
            Ok(count) => log(
                Level::Info,
                format!("cron: restored {count} durable job(s)"),
            ),
            Err(error) => warnings.push(format!("cron: {error}")),
        }

        // Read once, at startup, so the snapshot and a later turn agree on what
        // this process was configured with.
        let (model, config_error) = match Config::from_env() {
            Ok(config) => (Some(config.model), None),
            Err(error) => (None, Some(error.to_string())),
        };

        let bridge = Bridge {
            workspace,
            sessions_dir,
            started: Instant::now(),
            started_at: Utc::now().to_rfc3339(),
            cron,
            mcp,
            model,
            config_error,
            warnings,
        };
        if let Some(error) = &bridge.config_error {
            log(
                Level::Warn,
                format!("no model configured; `chat.send` will refuse until one is: {error}"),
            );
        }
        bridge
    }

    fn uptime_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn ping(&self) -> Value {
        json!({
            "pong": true,
            "ts": Utc::now().to_rfc3339(),
            "uptime_ms": self.uptime_ms(),
        })
    }
    fn snapshot(&self) -> Value {
        let mut warnings = self.warnings.clone();

        let skills = SkillLoader::new(self.workspace.join("skills"));
        let skill_names: Vec<String> = skills.skills().map(|skill| skill.name.clone()).collect();

        let tasks = TaskStore::new(&self.workspace);
        let task_list = match tasks.list() {
            Ok(list) => list,
            Err(error) => {
                warnings.push(format!("tasks: {error}"));
                Vec::new()
            }
        };
        let count_of = |status: TaskStatus| {
            task_list
                .iter()
                .filter(|task| task.status == status)
                .count()
        };

        let memories = MemoryStore::new(&self.workspace);
        let records = match memories.list() {
            Ok(records) => records,
            Err(error) => {
                warnings.push(format!("memory: {error}"));
                Vec::new()
            }
        };
        let mut by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
        for record in &records {
            *by_kind.entry(record.kind.as_str()).or_default() += 1;
        }

        let jobs = self.cron.jobs();
        let pending_jobs = jobs.iter().filter(|job| job.pending_delivery).count();
        let skill_count = skill_names.len();
        let connected_plugins = self.mcp.connected_names();
        let workforce = WorkforceStore::new(&self.workspace);
        let departments = match workforce.list() {
            Ok(list) => list,
            Err(error) => {
                warnings.push(format!("departments: {error}"));
                Vec::new()
            }
        };
        let agent_count: usize = departments.iter().map(|dept| dept.agents.len()).sum();

        json!({
            "protocol_version": PROTOCOL_VERSION,
            "bridge_version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "started_at": self.started_at,
            "uptime_ms": self.uptime_ms(),
            "workspace": self.workspace.display().to_string(),
            "methods": METHODS.as_slice(),
            "model": self.model,
            "config_error": self.config_error,
            "skills": { "count": skill_count, "names": skill_names },
            "tasks": {
                "total": task_list.len(),
                "pending": count_of(TaskStatus::Pending),
                "in_progress": count_of(TaskStatus::InProgress),
                "completed": count_of(TaskStatus::Completed),
            },
            "memory": { "records": records.len(), "by_kind": by_kind },
            "cron": { "jobs": jobs.len(), "pending_delivery": pending_jobs },
            "plugins": {
                "available": self.mcp.available().len(),
                "connected": connected_plugins.len(),
                "names": connected_plugins,
            },
            "workforce": {
                "departments": departments.len(),
                "agents": agent_count,
            },
            "workflow": { "last_run": last_run(&self.workspace) },
            "paths": {
                "workspace": self.workspace.display().to_string(),
                "skills": skills.dir().display().to_string(),
                "tasks": tasks.root().display().to_string(),
                "departments": workforce.root().display().to_string(),
                "memory": memories.dir().display().to_string(),
                "sessions": self.sessions_dir.display().to_string(),
                "mailboxes": self.workspace.join(MAILBOXES_DIR).display().to_string(),
                "workflow": self.workspace.join(WORKFLOWS_DIR).display().to_string(),
            },
            "warnings": warnings,
        })
    }
}
/// One dispatched request: what to answer, and whether to stop afterwards.
struct Turn {
    /// The answer to write back, or `None` when it is not this loop's to give:
    /// a `chat.cancel` is answered by the turn it stopped, and only once that
    /// turn is over.
    response: Option<Response>,
    shutdown: bool,
}

impl Turn {
    /// A request answered here and now.
    fn answered(response: Response) -> Self {
        Turn {
            response: Some(response),
            shutdown: false,
        }
    }

    /// A request whose answer the turn that is stopping still owes.
    fn owed() -> Self {
        Turn {
            response: None,
            shutdown: false,
        }
    }
}

fn dispatch(
    request: &Request,
    bridge: &Bridge,
    chat: &Arc<Chat>,
    handle: &tokio::runtime::Handle,
) -> Turn {
    match request.method.as_str() {
        "runtime.ping" => Turn::answered(Response::ok(&request.request_id, bridge.ping())),
        "runtime.status" => Turn::answered(Response::ok(&request.request_id, bridge.snapshot())),
        "runtime.shutdown" => Turn {
            response: Some(Response::ok(
                &request.request_id,
                json!({ "stopping": true }),
            )),
            shutdown: true,
        },
        "chat.send" => accept_chat(request, chat, handle),
        "chat.cancel" => accept_cancel(request, chat, handle),
        "chat.history" => Turn::answered(Response::ok(&request.request_id, chat.log.snapshot())),
        "chat.context" => {
            Turn::answered(Response::ok(&request.request_id, chat.context_snapshot()))
        }
        "chat.answer" => accept_answer(request, chat),
        "chat.approve" => accept_approve(request, chat),
        "sessions.list" => {
            Turn::answered(Response::ok(&request.request_id, chat.sessions_snapshot()))
        }
        "sessions.new" => accept_new_session(request, chat),
        "sessions.open" => accept_open_session(request, chat),
        "sessions.rename" => accept_rename_session(request, chat),
        "sessions.delete" => accept_delete_session(request, chat),
        "cron.list" => Turn::answered(Response::ok(&request.request_id, chat.cron_snapshot())),
        "cron.schedule" => accept_schedule_cron(request, chat),
        "cron.cancel" => accept_cancel_cron(request, chat),
        "tasks.list" => match tasks_snapshot(&chat.workspace) {
            Ok(tasks) => Turn::answered(Response::ok(&request.request_id, tasks)),
            Err(error) => Turn::answered(Response::failed(
                &request.request_id,
                ProtocolError::new(TASK_STORE, error.to_string()),
            )),
        },
        "tasks.create" => accept_create_task(request, chat),
        "tasks.claim" => accept_claim_task(request, chat),
        "tasks.release" => accept_release_task(request, chat),
        "tasks.complete" => accept_complete_task(request, chat),
        "departments.list" => match departments_snapshot(chat) {
            Ok(departments) => Turn::answered(Response::ok(&request.request_id, departments)),
            Err(error) => Turn::answered(Response::failed(
                &request.request_id,
                ProtocolError::new(WORKFORCE_STORE, error.to_string()),
            )),
        },
        "departments.create" => accept_create_department(request, chat),
        "departments.remove" => accept_remove_department(request, chat),
        "agents.add" => accept_add_agent(request, chat),
        "agents.remove" => accept_remove_agent(request, chat),
        "departments.run" => accept_run_department(request, chat, handle),
        "departments.stop" => accept_stop_department(request, chat),
        "skills.list" => Turn::answered(Response::ok(
            &request.request_id,
            skills_snapshot(&chat.workspace),
        )),
        "skills.read" => accept_read_skill(request, chat),
        "skills.import" => accept_import_skill(request, chat),
        "plugins.list" => Turn::answered(Response::ok(
            &request.request_id,
            plugins_snapshot(&chat.mcp),
        )),
        "plugins.connect" => accept_connect_plugin(request, chat),
        other => Turn::answered(Response::failed(
            &request.request_id,
            ProtocolError::new(
                UNKNOWN_METHOD,
                format!(
                    "unknown method `{other}`; this build answers: {}",
                    METHODS.join(", ")
                ),
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// stdin framing
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------------

/// Longest prompt the bridge accepts, in bytes.
const MAX_PROMPT_BYTES: usize = 64 * 1024;

/// How often the runtime looks at the clock while the host is quiet.
const CRON_TICK: Duration = Duration::from_secs(1);

/// Longest run of tool output echoed back to the host, in bytes. One `bash`
/// call can print megabytes, and the host drops a line it cannot frame.
const MAX_EVENT_TEXT_BYTES: usize = 8 * 1024;

/// How much of the conversation is kept for a page that asks for it again.
/// Oldest turns go first, and a turn that is still running is never dropped.
///
/// A snapshot of this is one line on the wire, and the host drops a line it
/// cannot frame: the budget has to stay inside the frame the host reads with,
/// or `chat.history` is answered with something nobody can read back. The host
/// takes the larger line of the two directions for exactly this answer -- see
/// `MAX_LINE_BYTES` in `desktop/src-tauri/src/runtime.rs`.
const MAX_LOG_TURNS: usize = 50;
const MAX_LOG_BYTES: usize = 4 * 1024 * 1024;

/// Codes this section adds to the protocol.
const CHAT_BUSY: &str = "chat_busy";
const CHAT_NOT_CONFIGURED: &str = "chat_not_configured";
/// Asked for a conversation this workspace does not hold.
const SESSION_UNKNOWN: &str = "session_unknown";
/// The conversations could not be read or written where they are kept.
const SESSION_STORE: &str = "session_store";
/// A cron expression this runtime does not accept.
const CRON_INVALID: &str = "cron_invalid";
/// Asked to cancel a cron job this runtime does not hold.
const CRON_UNKNOWN: &str = "cron_unknown";

/// The task board could not be read or written.
const TASK_STORE: &str = "task_store";
/// Asked for a task this workspace does not hold.
const TASK_UNKNOWN: &str = "task_unknown";
/// A task field this runtime will not take.
const TASK_INVALID: &str = "task_invalid";
/// The task is waiting on prerequisites that are not complete.
const TASK_BLOCKED: &str = "task_blocked";
/// The task is not in the state this action needs.
const TASK_NOT_READY: &str = "task_not_ready";
/// The task is claimed by somebody other than the page's user.
const TASK_NOT_OWNED: &str = "task_not_owned";
/// The task changed under the action, so the action did not land.
const TASK_CONFLICT: &str = "task_conflict";

/// The department store could not be read or written.
const WORKFORCE_STORE: &str = "workforce_store";
/// A department or agent field this runtime will not take.
const WORKFORCE_INVALID: &str = "workforce_invalid";
/// A shift is already running, so another one was not started.
const WORKFORCE_CONFLICT: &str = "workforce_conflict";
/// Asked for a department this workspace does not hold.
const DEPARTMENT_UNKNOWN: &str = "department_unknown";
/// Asked for an agent that is not in the department named.
const AGENT_UNKNOWN: &str = "agent_unknown";

/// The owner recorded when the page claims a task. Model turns use `agent` or
/// a teammate's name, so the two can never be mistaken for one another.
const PAGE_TASK_OWNER: &str = "user";

/// Asked to read a skill this workspace does not hold.
const SKILL_UNKNOWN: &str = "skill_unknown";
/// The folder to import from is not a folder the runtime can read.
const SKILL_SOURCE: &str = "skill_source";
/// Neither the folder nor any folder under it holds a `SKILL.md`.
const SKILL_INVALID: &str = "skill_invalid";
/// A skill of that name is already in the workspace.
const SKILL_EXISTS: &str = "skill_exists";
/// Asked to connect a plugin this build does not ship.
const PLUGIN_UNKNOWN: &str = "plugin_unknown";
/// A plugin this build ships could not be connected.
const PLUGIN_FAILED: &str = "plugin_failed";

/// Longest name a conversation may be given, in characters. Long enough for a
/// sentence, short enough that a list can show it.
const MAX_SESSION_TITLE_CHARS: usize = 120;

/// Which turn the shared observer should attribute its events to.
///
/// The loop is built once and keeps its history across prompts, so its observer
/// outlives any one turn. This is the one value the observer reads when an event
/// happens instead of when it is built. Only one turn runs at a time, so a plain
/// mutex is enough.
#[derive(Default)]
struct TurnTag(Mutex<String>);

impl TurnTag {
    fn set(&self, turn: &str) {
        let mut tag = self.0.lock().unwrap_or_else(|error| error.into_inner());
        *tag = turn.to_string();
    }

    fn get(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

/// What the bridge has told the host about each turn.
///
/// One turn is one prompt and everything reported about it. The events are
/// recorded where they are sent rather than rebuilt from the loop's history, so
/// a turn reads back exactly as it arrived the first time -- and a prompt the
/// loop had augmented with cron or teammate mail still shows as the words the
/// user typed. A page that reloaded asks for this instead of losing the
/// conversation, which it only ever held a view of.
struct TurnLog {
    inner: Mutex<LogInner>,
}

#[derive(Default)]
struct LogInner {
    turns: Vec<LoggedTurn>,
    /// Approximate size of everything above, so a long conversation is bounded
    /// by bytes and not only by how many turns it has.
    bytes: usize,
}

struct LoggedTurn {
    turn: String,
    prompt: String,
    /// `running`, `done`, `failed` or `cancelled`; the terminal event carries
    /// the detail.
    status: &'static str,
    error: Option<ProtocolError>,
    events: Vec<EventMessage>,
    weight: usize,
}

/// One turn as it is stored in a session file.
///
/// Deliberately the same shape `chat.history` answers with, so what is written
/// to disk is exactly what a page reads back, on the day as on the restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredTurn {
    turn: String,
    prompt: String,
    status: String,
    #[serde(default)]
    error: Option<ProtocolError>,
    #[serde(default)]
    events: Vec<EventMessage>,
}

impl TurnLog {
    fn new() -> Self {
        TurnLog {
            inner: Mutex::new(LogInner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LogInner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// A prompt the runtime has accepted; it is a turn from here on.
    fn begin(&self, turn: &str, prompt: &str) {
        let mut inner = self.lock();
        let weight = prompt.len() + 64;
        inner.bytes += weight;
        inner.turns.push(LoggedTurn {
            turn: turn.to_string(),
            prompt: prompt.to_string(),
            status: "running",
            error: None,
            events: Vec::new(),
            weight,
        });
        prune(&mut inner);
    }

    /// Send one turn event and keep it, so a page that asks later gets the turn
    /// as it arrived the first time.
    fn emit(&self, message: EventMessage) {
        self.record(&message);
        let _ = send(&message);
    }

    fn record(&self, message: &EventMessage) {
        let Some(turn_id) = message.data.get("turn").and_then(Value::as_str) else {
            return;
        };
        let weight = message.event.len() + message.data.to_string().len() + 64;
        let mut inner = self.lock();
        let Some(index) = inner.turns.iter().position(|logged| logged.turn == turn_id) else {
            return;
        };
        let turn = &mut inner.turns[index];
        match message.event.as_str() {
            "chat.done" => turn.status = "done",
            "chat.cancelled" => turn.status = "cancelled",
            "chat.failed" => {
                turn.status = "failed";
                turn.error = message
                    .data
                    .get("error")
                    .and_then(|value| serde_json::from_value(value.clone()).ok());
            }
            _ => {}
        }
        turn.weight += weight;
        turn.events.push(message.clone());
        inner.bytes += weight;
        prune(&mut inner);
    }

    /// The conversation so far, oldest turn first.
    fn snapshot(&self) -> Value {
        let inner = self.lock();
        let turns: Vec<StoredTurn> = inner
            .turns
            .iter()
            .map(|turn| StoredTurn {
                turn: turn.turn.clone(),
                prompt: turn.prompt.clone(),
                status: turn.status.to_string(),
                error: turn.error.clone(),
                events: turn.events.clone(),
            })
            .collect();
        json!({ "turns": turns })
    }

    /// A conversation read back from a session file.
    ///
    /// A turn that was still running when the file was written is reported as
    /// failed instead: whatever was going to finish it is gone, and a spinner
    /// that never stops is worse than saying so.
    fn from_snapshot(view: &Value) -> TurnLog {
        let stored: Vec<StoredTurn> =
            serde_json::from_value(view.get("turns").cloned().unwrap_or(Value::Null))
                .unwrap_or_default();
        let mut inner = LogInner::default();
        for turn in stored {
            let (status, error) = match turn.status.as_str() {
                "running" => (
                    "failed",
                    Some(ProtocolError::new(
                        "chat_interrupted",
                        "the runtime stopped before this turn finished",
                    )),
                ),
                "failed" => ("failed", turn.error),
                "cancelled" => ("cancelled", turn.error),
                _ => ("done", turn.error),
            };
            let mut weight = turn.prompt.len() + 64;
            for event in &turn.events {
                weight += event.event.len() + event.data.to_string().len() + 64;
            }
            inner.bytes += weight;
            inner.turns.push(LoggedTurn {
                turn: turn.turn,
                prompt: turn.prompt,
                status,
                error,
                events: turn.events,
                weight,
            });
        }
        prune(&mut inner);
        TurnLog {
            inner: Mutex::new(inner),
        }
    }

    /// The highest turn number reported, so the next id continues the count
    /// instead of naming a turn the conversation already holds.
    fn last_number(&self) -> u64 {
        self.lock()
            .turns
            .iter()
            .filter_map(|turn| turn.turn.strip_prefix("turn-")?.parse().ok())
            .max()
            .unwrap_or(0)
    }

    /// Replace the turns with another conversation's.
    fn adopt(&self, view: &Value) {
        let replacement = TurnLog::from_snapshot(view);
        let inner = std::mem::take(&mut *replacement.lock());
        *self.lock() = inner;
    }
}

/// Drop whole turns from the front until the log fits.
fn prune(inner: &mut LogInner) {
    while inner.turns.len() > MAX_LOG_TURNS
        || (inner.bytes > MAX_LOG_BYTES && inner.turns.len() > 1)
    {
        let dropped = inner.turns.remove(0);
        inner.bytes = inner.bytes.saturating_sub(dropped.weight);
    }
}

/// Streams one turn's progress to the host on the protocol stream.
struct ChatObserver {
    tag: Arc<TurnTag>,
    /// Where the events are also kept, for a page that asks again.
    log: Arc<TurnLog>,
    /// `main` for the conversation the user is having, `subagent` for a `task`
    /// call's own loop, whose chatter would otherwise read as an answer.
    source: &'static str,
    /// Whether this observer's text is the answer the user asked for.
    answers: bool,
    /// Where each compaction is counted, for the conversation's own observer.
    /// A subagent compacts nothing, so it has none.
    compactions: Option<Arc<AtomicUsize>>,
}

impl ChatObserver {
    /// The observer for the conversation itself.
    fn conversation(tag: Arc<TurnTag>, log: Arc<TurnLog>, compactions: Arc<AtomicUsize>) -> Self {
        ChatObserver {
            tag,
            log,
            source: "main",
            answers: true,
            compactions: Some(compactions),
        }
    }

    /// The observer for a subagent: tool use is worth reporting, its prose is
    /// not -- the `task` tool already returns that to the model as a result.
    fn task(tag: Arc<TurnTag>, log: Arc<TurnLog>) -> Self {
        ChatObserver {
            tag,
            log,
            source: "subagent",
            answers: false,
            compactions: None,
        }
    }
}

impl AgentObserver for ChatObserver {
    fn on_event(&self, signal: AgentEvent<'_>) {
        if matches!(signal, AgentEvent::Compacted { .. }) {
            if let Some(compactions) = &self.compactions {
                compactions.fetch_add(1, Ordering::SeqCst);
            }
        }
        let Some((name, mut data)) = describe(&signal, self.answers) else {
            return;
        };
        if let Some(fields) = data.as_object_mut() {
            fields.insert("turn".to_string(), json!(self.tag.get()));
            fields.insert("source".to_string(), json!(self.source));
        }
        self.log.emit(event(name, data));
    }
}

/// Turn one loop event into the name and payload the host renders.
///
/// `None` means the event is not worth a line: a step marker, or a subagent's
/// own answer text.
fn describe(event: &AgentEvent<'_>, answers: bool) -> Option<(&'static str, Value)> {
    let described = match event {
        AgentEvent::Step(_) => return None,
        AgentEvent::AssistantText(text) => {
            if !answers {
                return None;
            }
            ("chat.delta", json!({ "text": *text }))
        }
        AgentEvent::ToolCall { name, input } => (
            "chat.tool_call",
            json!({ "name": *name, "input": clamp(&input.to_string()) }),
        ),
        AgentEvent::ToolResult {
            name,
            output,
            is_error,
        } => (
            "chat.tool_result",
            json!({ "name": *name, "output": clamp(output), "is_error": *is_error }),
        ),
        AgentEvent::Blocked { name, reason } => {
            ("chat.blocked", json!({ "name": *name, "reason": *reason }))
        }
        AgentEvent::Compacted { kind, .. } => {
            notice("compacted", format!("history compacted ({kind})"))
        }
        AgentEvent::MemoryRecalled { count } => notice(
            "memory_recalled",
            format!("recalled {count} memory record(s)"),
        ),
        AgentEvent::MemoryStored {
            stored,
            consolidated,
        } => notice(
            "memory_stored",
            format!("stored {stored} memory record(s), consolidated {consolidated}"),
        ),
        AgentEvent::BackgroundStarted { task_id } => notice(
            "background_started",
            format!("background task {task_id} started"),
        ),
        AgentEvent::BackgroundCollected { count } => notice(
            "background_collected",
            format!("{count} background result(s) ready"),
        ),
        AgentEvent::CronDelivered { count } => notice(
            "cron_delivered",
            format!("{count} scheduled prompt(s) delivered"),
        ),
        AgentEvent::TeamEventDelivered { count } => notice(
            "team_event",
            format!("{count} teammate message(s) delivered"),
        ),
        AgentEvent::McpConnected { server, tools } => notice(
            "mcp_connected",
            format!("connected {server} ({tools} tool(s))"),
        ),
        AgentEvent::ModelRecovered {
            kind,
            attempt,
            delay_ms,
        } => notice(
            "model_recovered",
            format!("{kind} from the model; retrying (attempt {attempt}) in {delay_ms} ms"),
        ),
        AgentEvent::ModelFellBack { model } => {
            notice("model_fallback", format!("switched to {model}"))
        }
        AgentEvent::MaxTokensEscalated { max_tokens } => notice(
            "max_tokens",
            format!("the answer was cut off; retrying with {max_tokens} tokens"),
        ),
        AgentEvent::ContinuationRequested { count } => notice(
            "continuation",
            format!("asked the model to continue ({count})"),
        ),
        AgentEvent::WorkflowReported {
            run_id,
            kind,
            message,
        } => notice("workflow", format!("{run_id} {kind}: {message}")),
        AgentEvent::GoalReported {
            kind,
            reason,
            iterations,
            duration_ms,
            ..
        } => notice(
            "goal",
            format!("goal {kind}: {reason} ({iterations} iteration(s), {duration_ms} ms)"),
        ),
    };
    Some(described)
}

/// A progress note: something the turn did that is not an answer.
fn notice(kind: &str, text: String) -> (&'static str, Value) {
    ("chat.notice", json!({ "kind": kind, "text": text }))
}

/// Shorten a field so one tool result cannot blow past the host's line cap.
///
/// Trimming happens on a character boundary, because a tool result is arbitrary
/// UTF-8 and the protocol line has to stay valid JSON.
fn clamp(text: &str) -> String {
    if text.len() <= MAX_EVENT_TEXT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_EVENT_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n... [{} more bytes omitted]",
        &text[..end],
        text.len() - end
    )
}

/// The one department shift this runtime may be driving.
///
/// Shifts are sequential on purpose: every agent in a department works from
/// the same `.tasks` directory, and two shifts at once would make the board
/// disagree with itself between a claim and its completion. The page can start
/// another shift as soon as this one reports that it is over.
struct ActiveShift {
    started_at: String,
    agents_total: usize,
    agents_done: usize,
    current_agent: Option<String>,
    stop: watch::Sender<bool>,
}

/// Departments on disk, plus the shift in flight (if any).
struct WorkforceRuntime {
    store: WorkforceStore,
    shifts: Mutex<BTreeMap<String, ActiveShift>>,
}

impl WorkforceRuntime {
    fn new(workspace: impl Into<PathBuf>) -> Self {
        WorkforceRuntime {
            store: WorkforceStore::new(workspace),
            shifts: Mutex::new(BTreeMap::new()),
        }
    }

    fn lock_shifts(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ActiveShift>> {
        self.shifts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn active_department(&self) -> Option<String> {
        self.lock_shifts().keys().next().cloned()
    }

    /// Reserve the single shift slot. `None` means a shift is already running.
    fn start(&self, department_id: &str, agents_total: usize) -> Option<watch::Receiver<bool>> {
        let mut shifts = self.lock_shifts();
        if !shifts.is_empty() {
            return None;
        }
        let (stop, receiver) = watch::channel(false);
        shifts.insert(
            department_id.to_string(),
            ActiveShift {
                started_at: Utc::now().to_rfc3339(),
                agents_total,
                agents_done: 0,
                current_agent: None,
                stop,
            },
        );
        Some(receiver)
    }

    fn set_current_agent(&self, department_id: &str, agent_id: &str) {
        if let Some(shift) = self.lock_shifts().get_mut(department_id) {
            shift.current_agent = Some(agent_id.to_string());
        }
    }

    fn mark_agent_done(&self, department_id: &str) {
        if let Some(shift) = self.lock_shifts().get_mut(department_id) {
            shift.agents_done += 1;
            shift.current_agent = None;
        }
    }

    fn finish(&self, department_id: &str) {
        self.lock_shifts().remove(department_id);
    }

    /// Ask the shift to stop. `false` means there was nothing to stop.
    fn stop(&self, department_id: &str) -> bool {
        let shifts = self.lock_shifts();
        let Some(shift) = shifts.get(department_id) else {
            return false;
        };
        let _ = shift.stop.send(true);
        true
    }

    /// The shift in flight for one department, as the page lists it.
    fn shift_view(&self, department_id: &str) -> Option<Value> {
        let shifts = self.lock_shifts();
        let shift = shifts.get(department_id)?;
        Some(json!({
            "running": true,
            "startedAt": shift.started_at,
            "currentAgentId": shift.current_agent,
            "agentsDone": shift.agents_done,
            "agentsTotal": shift.agents_total,
        }))
    }
}

/// The conversation the shell is having, and the one turn that may be running.
struct Chat {
    workspace: PathBuf,
    /// The same job table the snapshot reports, so a scheduled prompt fires in
    /// the next turn rather than never.
    cron: CronScheduler,
    /// The same MCP registry the snapshot reports, so a plugin the page
    /// connects is in the pool the next model call sees.
    mcp: McpRegistry,
    /// Built on the first prompt and reused, so the conversation keeps its
    /// history. A build that failed is remembered with its reason: the answer
    /// will not change until the runtime is restarted.
    agent: Mutex<Option<Result<Arc<Agent>, ProtocolError>>>,
    /// The conversation being continued: the messages the model has seen, the
    /// view the page reads back, and the file both are written to. A turn takes
    /// the messages out for its length instead of holding the lock, so the
    /// reader loop keeps answering while the model thinks.
    session: Mutex<Session>,
    /// Where conversations live. The host decides this, so the desktop app can
    /// keep them in its own store; it is `<workspace>/.sessions` by default.
    sessions: SessionStore,
    /// The way out of the turn that is running.
    cancel: TurnCancel,
    /// Set while a turn runs: the protocol takes one prompt at a time.
    busy: AtomicBool,
    /// Monotonic turn counter, so a host can match events to its prompt.
    turns: AtomicU64,
    /// How many times the loop has compacted the conversation's history.
    ///
    /// Counted here rather than read back from the transcript so the page can
    /// say "it compacts by itself" with a number behind it.
    compactions: Arc<AtomicUsize>,
    tag: Arc<TurnTag>,
    /// Every turn reported so far, for a page that asks for them again.
    log: Arc<TurnLog>,
    /// Where a turn stops to ask the window something, and hears back.
    hub: Arc<InteractionHub>,
    /// The departments the page made, and the shift currently driving one.
    workforce: WorkforceRuntime,
}

impl Chat {
    fn new(
        workspace: PathBuf,
        sessions_dir: PathBuf,
        cron: CronScheduler,
        mcp: McpRegistry,
    ) -> Self {
        let sessions = SessionStore::at(sessions_dir);
        // Pick up where the last run left off. A store this build cannot read is
        // reported and stepped over: history is not worth refusing to start a
        // conversation over.
        let resumed = match sessions.latest() {
            Ok(session) => session,
            Err(error) => {
                log(
                    Level::Warn,
                    format!("could not read the saved conversations: {error}"),
                );
                None
            }
        };
        let (session, log) = match resumed {
            Some(session) => {
                log(
                    Level::Info,
                    format!(
                        "continuing session {} ({} messages)",
                        session.id,
                        session.messages.len()
                    ),
                );
                let log = Arc::new(TurnLog::from_snapshot(&session.view));
                (session, log)
            }
            None => (sessions.create(""), Arc::new(TurnLog::new())),
        };
        let turns = log.last_number();
        let workforce = WorkforceRuntime::new(workspace.clone());
        Chat {
            workspace,
            cron,
            mcp,
            agent: Mutex::new(None),
            session: Mutex::new(session),
            sessions,
            cancel: TurnCancel::default(),
            busy: AtomicBool::new(false),
            turns: AtomicU64::new(turns),
            compactions: Arc::new(AtomicUsize::new(0)),
            tag: Arc::new(TurnTag::default()),
            hub: Arc::new(InteractionHub::default()),
            log,
            workforce,
        }
    }

    /// The conversation being continued, as it stands right now.
    fn session(&self) -> Session {
        let guard = self
            .session
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        guard.clone()
    }

    /// Keep what a turn produced, and write the conversation to disk.
    ///
    /// Called once per turn, after the terminal event, so the file holds a
    /// finished turn and never a running one: a load can then tell a turn that
    /// was interrupted from one that is still going.
    ///
    /// `Some` replaces the conversation's messages; `None` writes it as it
    /// stands, which is what a prompt that never reached the model leaves.
    fn commit(&self, messages: Option<Vec<Message>>) {
        let mut guard = self
            .session
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(messages) = messages {
            guard.messages = messages;
        }
        guard.view = self.log.snapshot();
        if let Err(error) = self.sessions.save(&mut guard) {
            log(
                Level::Warn,
                format!("could not write the conversation to disk: {error}"),
            );
        }
    }

    /// Continue `session` instead of the one in hand.
    ///
    /// Only ever called with the busy slot free, so no turn is writing to the
    /// conversation being left behind.
    fn adopt(&self, session: Session) {
        self.log.adopt(&session.view);
        self.turns.store(self.log.last_number(), Ordering::SeqCst);
        let mut guard = self
            .session
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *guard = session;
    }

    /// Name the conversation after its first prompt, once.
    ///
    /// A conversation that has a title keeps it: an old one may have been
    /// opened, and a later prompt is not a better name for it than the first.
    fn name_session(&self, prompt: &str) {
        let mut guard = self
            .session
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if guard.title.is_empty() {
            guard.title = session_title(prompt);
        }
    }

    /// Give the conversation in hand a name of its own, whether or not it
    /// already had one.
    ///
    /// The file is written here rather than left to the next turn, so a rename
    /// survives a restart that happens before another prompt. A conversation
    /// that has no file yet keeps none: it is written the moment it has a turn
    /// worth keeping, and the name goes out with it.
    fn rename(&self, title: &str) -> Result<(), Error> {
        let mut guard = self
            .session
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        guard.title = title.to_string();
        // The store's own rename re-reads the file, so it answers `false` --
        // and writes nothing -- for a conversation that is not on disk.
        self.sessions.rename(&guard.id, title)?;
        Ok(())
    }

    /// Every conversation this workspace holds, and the one being continued.
    fn sessions_snapshot(&self) -> Value {
        let current = self.session();
        let mut sessions = match self.sessions.list() {
            Ok(sessions) => sessions,
            Err(error) => {
                log(
                    Level::Warn,
                    format!("could not list the saved conversations: {error}"),
                );
                Vec::new()
            }
        };
        // The conversation in hand is listed even before it has a turn worth
        // writing to disk. A new one has no file yet, and a list that leaves it
        // out reads as though the workspace only ever holds one conversation.
        if !sessions.iter().any(|session| session.id == current.id) {
            sessions.insert(0, SessionSummary::of(&current));
        }
        json!({ "current": current.id, "sessions": sessions })
    }

    /// Every cron job this workspace holds, with when each one next runs.
    ///
    /// The table is the runtime's own, the same one `schedule_cron` writes to,
    /// so the page and the model can never disagree about what is scheduled.
    fn cron_snapshot(&self) -> Value {
        let table = self.cron.jobs();
        let pending = table.iter().filter(|job| job.pending_delivery).count();
        let jobs: Vec<Value> = table.iter().map(cron_view).collect();
        json!({ "jobs": jobs, "pending_delivery": pending })
    }

    /// The agent loop for this workspace, built once.
    fn agent(&self) -> Result<Arc<Agent>, ProtocolError> {
        let mut cached = self.agent.lock().unwrap_or_else(|error| error.into_inner());
        let built = cached.get_or_insert_with(|| {
            build_agent(
                &self.workspace,
                &self.cron,
                &self.mcp,
                Arc::clone(&self.tag),
                Arc::clone(&self.log),
                Arc::clone(&self.compactions),
                Arc::clone(&self.hub),
            )
        });
        built.clone()
    }

    /// What the loop is holding for the conversation in hand.
    ///
    /// The runtime compacts a long conversation on its own, before every model
    /// call; nothing about that is visible from the page unless it is asked
    /// for. This is that answer: the size the summary step measures, the
    /// threshold it measures against, and how often it has already run.
    fn context_snapshot(&self) -> Value {
        let session = self.session();
        let messages = session.messages.len();
        let compacted = self.compactions.load(Ordering::SeqCst);
        // The size is a fact about the conversation, so it is answered whether
        // or not a model is configured; the threshold is the loop's, so it is
        // only known once the loop exists.
        let char_limit = self
            .agent()
            .ok()
            .and_then(|agent| agent.compactor().map(|c| c.config().context_char_limit));
        json!({
            "session": session.id,
            "messages": messages,
            "compacted": compacted,
            "chars": Compactor::estimate_chars(&session.messages),
            "char_limit": char_limit,
        })
    }

    /// The next turn id. Only ever called with the busy slot held.
    fn next_turn(&self) -> String {
        format!("turn-{}", self.turns.fetch_add(1, Ordering::SeqCst) + 1)
    }
}

/// Frees the one-turn-at-a-time slot however the turn ends, a panic in the loop
/// included.
struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The way out of the turn that is running, and the way to know it has gone.
///
/// One turn runs at a time, so one slot is enough: `chat.send` puts the sender
/// for its own turn here and hands the receiver to the turn, and `chat.cancel`
/// takes the sender and flips it. A turn that has already stopped is not
/// cancelled late: `disarm` empties the slot when the turn is over, so a stop
/// aimed at nothing answers that nothing was running.
///
/// A stop also takes the channel the turn closes when it is done, because it is
/// not answered on the spot. The page that pressed stop sends its next prompt on
/// the back of that answer, and that prompt has to find the one-turn slot free
/// rather than be told the turn it just ended is still running.
#[derive(Default)]
struct TurnCancel {
    /// The sender for the turn that is running, if any.
    running: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
    /// The receiver that resolves when that turn is over, if any.
    over: Mutex<Option<tokio::sync::watch::Receiver<bool>>>,
}

impl TurnCancel {
    /// A channel for a turn that is about to start: the stop it listens on, and
    /// the one it closes when it is done.
    fn arm(
        &self,
    ) -> (
        tokio::sync::watch::Receiver<bool>,
        tokio::sync::watch::Sender<bool>,
    ) {
        let (stop, listening) = tokio::sync::watch::channel(false);
        let (closing, over) = tokio::sync::watch::channel(false);
        {
            let mut slot = self
                .running
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *slot = Some(stop);
        }
        {
            let mut slot = self.over.lock().unwrap_or_else(|error| error.into_inner());
            *slot = Some(over);
        }
        (listening, closing)
    }

    /// Ask the running turn to stop, and take the channel that says when it
    /// has. `None` when nothing was running.
    fn stop(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        let running = {
            let mut slot = self
                .running
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            slot.take()
        };
        let over = {
            let mut slot = self.over.lock().unwrap_or_else(|error| error.into_inner());
            slot.take()
        };
        match running {
            Some(send) => {
                // A receiver that has an unseen version resolves `changed`
                // before it ever looks at whether the sender still exists,
                // which is what lets the stop land and the slot empty at once.
                let _ = send.send(true);
                over
            }
            None => None,
        }
    }

    /// The turn is over; its channels must not outlive it.
    fn disarm(&self) {
        {
            let mut slot = self
                .running
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *slot = None;
        }
        {
            let mut slot = self.over.lock().unwrap_or_else(|error| error.into_inner());
            *slot = None;
        }
    }
}

/// How long the runtime waits for a person before it gives up on an
/// interaction and tells the model nobody answered.
///
/// Long enough to read a diff and decide, short enough that a window closed on
/// a question does not leave the runtime holding a turn nobody will ever end.
const INTERACTION_TIMEOUT: Duration = Duration::from_secs(600);

/// The id prefix for a question the model asked, and for a decision the
/// permission gate is waiting on. A transcript can tell the two apart, and so
/// can an answer that arrives late.
const QUESTION_ID: &str = "ask";
const PERMISSION_ID: &str = "perm";

/// What a person said when the runtime asked them something.
enum Reply {
    /// Whether the flagged call may run.
    Verdict(bool),
    /// What the user typed, or `None` when they said nothing.
    Answer(Option<String>),
}

/// How a turn asks the window something, and hears back.
///
/// The permission gate and the `ask_user` tool both need the same thing: stop
/// here, put the question to whoever is driving this shell, carry on with
/// whatever they say. Both are reached from inside a tool call, which is to say
/// from inside the turn, so one slot is enough -- exactly like [`TurnCancel`] --
/// and an answer is matched to its question by id rather than by order.
///
/// The wait is a blocking one. The gates are synchronous hooks, so the turn
/// parks on a channel until somebody answers, the wait runs out, or the turn is
/// stopped and the slot is emptied under it.
#[derive(Default)]
struct InteractionHub {
    /// The interaction the turn is waiting on, if any.
    waiting: Mutex<Option<Waiting>>,
    /// Whether anybody is behind the turn that is running.
    ///
    /// A turn the runtime started on its own -- a due scheduled job -- has no
    /// page in front of it, and a question put to an empty room is a turn that
    /// hangs and holds the one-turn slot while it does. Such a turn is refused
    /// instead, which is what this bridge has always done.
    attended: AtomicBool,
    /// Names each interaction, so an answer can be matched to the question it
    /// belongs to.
    ids: AtomicU64,
}

/// One interaction the turn has parked on.
struct Waiting {
    id: String,
    replies: std::sync::mpsc::Sender<Reply>,
}

impl InteractionHub {
    /// Say whether anybody is behind the turn about to run.
    fn attend(&self, attended: bool) {
        self.attended.store(attended, Ordering::SeqCst);
    }

    /// The next id for a kind of interaction.
    fn next_id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.ids.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// Take the slot for `id`, and hand back the end that resolves when somebody
    /// answers it.
    ///
    /// `None` when nobody is watching: the caller must not wait at all. The
    /// slot is booked before the question is reported, so an answer that comes
    /// straight back -- somebody who was already looking -- cannot arrive before
    /// there is anything to give it to.
    fn park(&self, id: &str) -> Option<std::sync::mpsc::Receiver<Reply>> {
        if !self.attended.load(Ordering::SeqCst) {
            return None;
        }
        let (replies, waiting) = std::sync::mpsc::channel();
        self.replace(Some(Waiting {
            id: id.to_string(),
            replies,
        }));
        Some(waiting)
    }

    /// Wait for the answer to the slot that was parked, or for the wait to run
    /// out. Either way the slot is given back: a question is asked once.
    fn wait(&self, waiting: std::sync::mpsc::Receiver<Reply>) -> Option<Reply> {
        let reply = waiting.recv_timeout(INTERACTION_TIMEOUT).ok();
        // Whoever answered already emptied the slot; this is for a wait that
        // ran out, and for an answer that arrived a moment too late to matter.
        self.replace(None);
        reply
    }

    /// Hand `reply` to whoever is waiting on `id`. `false` when nobody is: an
    /// answer for an interaction that is already over.
    fn deliver(&self, id: &str, reply: Reply) -> bool {
        let mut slot = self
            .waiting
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match slot.as_ref() {
            Some(waiting) if waiting.id == id => {
                let delivered = waiting.replies.send(reply).is_ok();
                *slot = None;
                delivered
            }
            _ => false,
        }
    }

    /// Let whoever is waiting go, with nothing: the turn is over, or a stop has
    /// arrived. Dropping the sender is what `recv_timeout` reads as "nobody is
    /// going to answer", which is exactly what happened.
    fn withdraw(&self) {
        self.replace(None);
    }

    fn replace(&self, waiting: Option<Waiting>) {
        let mut slot = self
            .waiting
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *slot = waiting;
    }
}

/// Report one interaction to the host, and keep it, for the same reason a tool
/// call is kept: a page that reloads reads the conversation back rather than
/// losing the question it was in the middle of answering.
fn report_interaction(tag: &TurnTag, log: &TurnLog, name: &str, data: Value) {
    let mut data = data;
    if let Some(fields) = data.as_object_mut() {
        fields.insert("turn".to_string(), json!(tag.get()));
        fields.insert("source".to_string(), json!("main"));
    }
    log.emit(event(name, data));
}

/// Gate three, answered by the window instead of by a terminal.
///
/// The permission hooks are synchronous, so this is where a turn stops and the
/// page picks up: the question goes out on the protocol stream, and the answer
/// comes back as `chat.approve`.
struct WindowApprover {
    hub: Arc<InteractionHub>,
    tag: Arc<TurnTag>,
    log: Arc<TurnLog>,
}

impl Approver for WindowApprover {
    fn approve(&self, call: &ToolCall, reason: &str) -> bool {
        let id = self.hub.next_id(PERMISSION_ID);
        let Some(waiting) = self.hub.park(&id) else {
            // Nobody is behind this turn. The call is refused, and the
            // transcript already says why.
            return false;
        };
        report_interaction(
            &self.tag,
            &self.log,
            "chat.permission",
            json!({
                "id": id,
                "name": call.name,
                "input": clamp(&call.input.to_string()),
                "reason": reason,
            }),
        );
        let allowed = matches!(self.hub.wait(waiting), Some(Reply::Verdict(true)));
        report_interaction(
            &self.tag,
            &self.log,
            "chat.permission_result",
            json!({ "id": id, "allowed": allowed }),
        );
        allowed
    }
}

/// `ask_user`, answered by the window.
struct WindowAsker {
    hub: Arc<InteractionHub>,
    tag: Arc<TurnTag>,
    log: Arc<TurnLog>,
}

impl Asker for WindowAsker {
    fn ask(&self, question: &str, options: &[String]) -> Option<String> {
        let id = self.hub.next_id(QUESTION_ID);
        let Some(waiting) = self.hub.park(&id) else {
            return None;
        };
        report_interaction(
            &self.tag,
            &self.log,
            "chat.question",
            json!({ "id": id, "question": question, "options": options }),
        );
        let answer = match self.hub.wait(waiting) {
            Some(Reply::Answer(answer)) => answer,
            _ => None,
        };
        report_interaction(
            &self.tag,
            &self.log,
            "chat.question_result",
            json!({ "id": id, "answer": answer }),
        );
        answer
    }
}

/// The label a conversation list shows for a prompt: its first line, collapsed
/// to one line and cut to something a list can hold.
fn session_title(prompt: &str) -> String {
    /// Longest title kept, in characters.
    const MAX_TITLE_CHARS: usize = 48;

    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut title: String = line.chars().take(MAX_TITLE_CHARS).collect();
    if line.chars().count() > MAX_TITLE_CHARS {
        title.push('…');
    }
    title
}

/// Build the loop the shell talks to.
///
/// The same agent the CLI builds, with two differences that come from there
/// being no terminal behind a prompt: the observer streams to the protocol
/// stream instead of printing, and gate three asks the page rather than reading
/// a line. A call the permission policy flags stops the turn, is reported as
/// `chat.permission`, and runs -- or is refused -- according to `chat.approve`.
fn build_agent(
    workspace: &Path,
    cron: &CronScheduler,
    mcp: &McpRegistry,
    tag: Arc<TurnTag>,
    log: Arc<TurnLog>,
    compactions: Arc<AtomicUsize>,
    hub: Arc<InteractionHub>,
) -> Result<Arc<Agent>, ProtocolError> {
    let config = Config::from_env().map_err(|error| {
        ProtocolError::new(
            CHAT_NOT_CONFIGURED,
            format!(
                "{error}; set the model in Settings (or add a .env to the workspace), then \
                 restart the runtime"
            ),
        )
    })?;

    let client: Arc<dyn LlmClient> = Arc::new(AnthropicClient::new(&config));

    // The subagent: fresh context, base tools only, so it cannot delegate.
    let subagent = Agent::builder()
        .shared_client(Arc::clone(&client))
        .tools(ToolRegistry::base())
        .workspace(workspace.to_path_buf())
        .max_tokens(config.max_tokens)
        .max_steps(30)
        .interactive(false)
        .system_prompt(subagent_system_prompt())
        .observer(ChatObserver::task(Arc::clone(&tag), Arc::clone(&log)))
        .build();

    let mut tools = ToolRegistry::with_tasks();
    tools.register(TaskTool::new(subagent));
    // The one way the model can put a question to the person driving this
    // shell. The subagent above does not get it: its turn is not the user's.
    tools.register(AskUserTool::new(Arc::new(WindowAsker {
        hub: Arc::clone(&hub),
        tag: Arc::clone(&tag),
        log: Arc::clone(&log),
    })));

    // s15: retry transient model failures, and fall back when one is configured.
    let mut recovery = RecoveryConfig::default();
    if let Some(model) = &config.fallback_model {
        recovery = recovery.with_fallback_model(model.clone());
    }

    Ok(Arc::new(
        Agent::builder()
            .shared_client(client)
            .tools(tools)
            .workspace(workspace.to_path_buf())
            .skills(SkillLoader::new(workspace.join("skills")))
            .compaction(CompactConfig::default())
            .memory(MemoryConfig::default())
            .cron(cron.clone())
            // The registry the page reads through `plugins.*`, so a plugin
            // either side installs is in the pool the next model call sees.
            .mcp(mcp.clone())
            .recovery(recovery)
            // s03's approval gate, pointed at the window: a call the policy
            // flags stops the turn, is reported as `chat.permission`, and runs
            // or is refused according to `chat.approve`. The subagent above
            // keeps `interactive(false)`, so a teammate's own turn is refused
            // rather than left waiting on a prompt nobody is there to see.
            .approver(WindowApprover {
                hub: Arc::clone(&hub),
                tag: Arc::clone(&tag),
                log: Arc::clone(&log),
            })
            .interactive(true)
            .max_tokens(config.max_tokens)
            .system_prompt(format!(
                "{} {} {} {}",
                default_system_prompt(),
                TASKS_PROMPT,
                CRON_PROMPT,
                MCP_PROMPT
            ))
            .observer(ChatObserver::conversation(tag, log, compactions))
            .build(),
    ))
}

/// Accept one prompt, or explain why it cannot run.
fn accept_chat(request: &Request, chat: &Arc<Chat>, handle: &tokio::runtime::Handle) -> Turn {
    let rejected = |error| Turn {
        response: Some(Response::failed(&request.request_id, error)),
        shutdown: false,
    };

    let Some(prompt) = request.params.get("prompt").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`chat.send` needs a `prompt` string",
        ));
    };
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`prompt` must not be empty",
        ));
    }
    if prompt.len() > MAX_PROMPT_BYTES {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            format!("`prompt` is longer than {MAX_PROMPT_BYTES} bytes"),
        ));
    }

    // Built here rather than inside the turn, so a runtime with no model
    // configured answers `ok: false` instead of accepting a prompt it is about
    // to fail. It is built once, so this only costs anything on the first call.
    if let Err(error) = chat.agent() {
        return rejected(error);
    }

    if chat.busy.swap(true, Ordering::SeqCst) {
        return rejected(ProtocolError::new(
            CHAT_BUSY,
            "a turn is already running; wait for `chat.done` before sending another prompt",
        ));
    }

    // A page that sent this prompt is a page to answer it: the approval gate
    // and `ask_user` wait on the window rather than refusing outright.
    chat.hub.attend(true);
    let turn = chat.next_turn();
    chat.log.begin(&turn, prompt);
    chat.name_session(prompt);
    // Armed before the turn starts, so a stop that arrives while the first model
    // call is still being set up is not lost.
    let (stop, over) = chat.cancel.arm();
    let chat = Arc::clone(chat);
    let prompt = prompt.to_string();
    let running = turn.clone();
    handle.spawn(async move {
        let busy = Busy(&chat.busy);
        chat.tag.set(&running);
        let stopped = run_turn(&chat, &running, Some(&prompt), stop).await;
        chat.cancel.disarm();
        chat.hub.withdraw();
        // The slot is free before anything says the turn is over: a prompt sent
        // the moment the page hears that must be taken, not told a turn it has
        // just seen end is still running. Nothing waits on `over` unless a stop
        // was asked for, and a stop that did gets its answer here.
        drop(busy);
        let _ = over.send(stopped);
    });

    Turn {
        response: Some(Response::ok(
            &request.request_id,
            json!({ "accepted": true, "turn": turn }),
        )),
        shutdown: false,
    }
}

/// Start a fresh conversation, leaving the one in hand where it is.
///
/// Nothing is dropped: every finished turn of the old conversation is on disk
/// already, and the new one is written the moment it has a turn worth keeping.
fn accept_new_session(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    if chat.busy.load(Ordering::SeqCst) {
        return rejected(ProtocolError::new(
            CHAT_BUSY,
            "a turn is running; stop it or wait for it before starting another conversation",
        ));
    }

    let session = chat.sessions.create("");
    let id = session.id.clone();
    chat.adopt(session);
    log(Level::Info, format!("started session {id}"));
    Turn::answered(Response::ok(
        &request.request_id,
        json!({ "session": id, "turns": 0 }),
    ))
}

/// Continue a conversation this workspace already holds.
fn accept_open_session(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    if chat.busy.load(Ordering::SeqCst) {
        return rejected(ProtocolError::new(
            CHAT_BUSY,
            "a turn is running; stop it or wait for it before opening another conversation",
        ));
    }

    let Some(id) = request.params.get("id").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`sessions.open` needs an `id` string",
        ));
    };
    // A session id is a file name, so it is checked here rather than left to the
    // store: what the host asked for is wrong, not what the store holds.
    if !is_session_id(id) {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            format!("`{id}` is not a session id"),
        ));
    }
    let session = match chat.sessions.load(id) {
        Ok(session) => session,
        Err(error) => return rejected(ProtocolError::new(SESSION_UNKNOWN, error.to_string())),
    };

    let turns = session
        .view
        .get("turns")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let id = session.id.clone();
    chat.adopt(session);
    log(
        Level::Info,
        format!("continuing session {id} ({turns} turns)"),
    );
    Turn {
        response: Some(Response::ok(
            &request.request_id,
            json!({ "session": id, "turns": turns }),
        )),
        shutdown: false,
    }
}

/// Rename a conversation.
///
/// The one in hand is renamed in memory first and written after, so a file that
/// cannot be written still leaves the conversation named on the page; any other
/// one is renamed in its file, and an id this workspace does not hold is refused
/// rather than invented. A name is not a file name, so it is bounded but never
/// sanitised: what the user typed is what the list shows back.
fn accept_rename_session(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    if chat.busy.load(Ordering::SeqCst) {
        return rejected(ProtocolError::new(
            CHAT_BUSY,
            "a turn is running; stop it or wait for it before renaming a conversation",
        ));
    }

    let Some(id) = request.params.get("id").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`sessions.rename` needs an `id` string",
        ));
    };
    if !is_session_id(id) {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            format!("`{id}` is not a session id"),
        ));
    }
    let Some(title) = request.params.get("title").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`sessions.rename` needs a `title` string",
        ));
    };
    let title = title.trim();
    if title.chars().count() > MAX_SESSION_TITLE_CHARS {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            format!("a conversation name is at most {MAX_SESSION_TITLE_CHARS} characters"),
        ));
    }

    if chat.session().id == id {
        if let Err(error) = chat.rename(title) {
            log(Level::Warn, format!("could not write the rename: {error}"));
        }
    } else {
        match chat.sessions.rename(id, title) {
            Ok(true) => {}
            Ok(false) => {
                return rejected(ProtocolError::new(
                    SESSION_UNKNOWN,
                    format!("no session `{id}`"),
                ))
            }
            Err(error) => return rejected(ProtocolError::new(SESSION_STORE, error.to_string())),
        }
    }

    log(Level::Info, format!("renamed session {id}"));
    Turn::answered(Response::ok(
        &request.request_id,
        json!({ "session": id, "title": title }),
    ))
}

/// Forget a conversation.
///
/// The one in hand can be deleted as well: the runtime then continues the newest
/// conversation that is left, or starts an empty one when nothing is left,
/// because the next prompt still has to have somewhere to go. The answer names
/// the conversation the page is in now, whether or not it changed.
fn accept_delete_session(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    if chat.busy.load(Ordering::SeqCst) {
        return rejected(ProtocolError::new(
            CHAT_BUSY,
            "a turn is running; stop it or wait for it before deleting a conversation",
        ));
    }

    let Some(id) = request.params.get("id").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`sessions.delete` needs an `id` string",
        ));
    };
    if !is_session_id(id) {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            format!("`{id}` is not a session id"),
        ));
    }

    let in_hand = chat.session().id == id;
    let on_disk = match chat.sessions.delete(id) {
        Ok(removed) => removed,
        Err(error) => return rejected(ProtocolError::new(SESSION_STORE, error.to_string())),
    };
    // The one in hand is a conversation the page showed even before it had a
    // file, so deleting it is deleting it; any other has to be on disk.
    if !on_disk && !in_hand {
        return rejected(ProtocolError::new(
            SESSION_UNKNOWN,
            format!("no session `{id}`"),
        ));
    }

    if in_hand {
        let next = match chat.sessions.latest() {
            Ok(next) => next,
            Err(error) => return rejected(ProtocolError::new(SESSION_STORE, error.to_string())),
        };
        chat.adopt(next.unwrap_or_else(|| chat.sessions.create("")));
    }

    let current = chat.session().id;
    log(
        Level::Info,
        format!("deleted session {id}; continuing {current}"),
    );
    Turn::answered(Response::ok(
        &request.request_id,
        json!({ "deleted": id, "current": current }),
    ))
}

/// One cron job as the page renders it: the job as stored, plus when it next
/// runs and how far away that is.
///
/// A one-shot that has already fired has no next run: it is on its way to the
/// model now, and the expression it came from would only name a minute that has
/// nothing left to do.
fn cron_view(job: &CronJob) -> Value {
    let now = CronScheduler::now();
    let next_run = if job.recurring || job.last_fired.is_none() {
        next_match(&job.cron, now)
    } else {
        None
    };
    json!({
        "id": job.id,
        "cron": job.cron,
        "prompt": job.prompt,
        "recurring": job.recurring,
        "durable": job.durable,
        "pending_delivery": job.pending_delivery,
        "last_fired": job.last_fired,
        // Local wall-clock, so a page on this machine reads back the moment the
        // runtime will actually fire; `next_run_ms` is for countdowns.
        "next_run": next_run.map(|moment| moment.format("%Y-%m-%dT%H:%M:%S").to_string()),
        "next_run_ms": next_run.map(|moment| (moment - now).num_milliseconds().max(0) as u64),
    })
}

/// The message inside a cron tool error, without the tool wrapper around it.
fn cron_message(error: &Error) -> String {
    match error {
        Error::Tool { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// Add a job to the scheduler the runtime already uses.
///
/// The expression is validated before it is stored, so one this runtime cannot
/// parse is refused here rather than accepted and silently never run. The page
/// names the expression and the prompt; both are its to choose, and a prompt
/// longer than one turn takes is refused rather than truncated.
fn accept_schedule_cron(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));

    let Some(cron) = request.params.get("cron").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`cron.schedule` needs a `cron` string",
        ));
    };
    let cron = cron.trim();
    if let Err(error) = validate_cron(cron) {
        return rejected(ProtocolError::new(CRON_INVALID, cron_message(&error)));
    }

    let Some(prompt) = request.params.get("prompt").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`cron.schedule` needs a `prompt` string",
        ));
    };
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`prompt` must not be empty",
        ));
    }
    if prompt.len() > MAX_PROMPT_BYTES {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            format!("`prompt` is longer than {MAX_PROMPT_BYTES} bytes"),
        ));
    }

    let (recurring, durable) = match (
        optional_flag(&request.params, "recurring", true),
        optional_flag(&request.params, "durable", true),
    ) {
        (Ok(recurring), Ok(durable)) => (recurring, durable),
        (Err(error), _) | (_, Err(error)) => return rejected(error),
    };

    match chat.cron.schedule(cron, prompt, recurring, durable) {
        Ok(job) => {
            log(
                Level::Info,
                format!("scheduled {}: {} -> {}", job.id, job.cron, job.prompt),
            );
            Turn::answered(Response::ok(
                &request.request_id,
                json!({ "job": cron_view(&job) }),
            ))
        }
        Err(error) => rejected(ProtocolError::new(CRON_INVALID, cron_message(&error))),
    }
}

/// An optional flag: absent means `default`, and anything but a boolean is
/// refused. A host that sends `"recurring": "no"` is told so, rather than
/// answered with a job that repeats for ever.
fn optional_flag(params: &Value, key: &str, default: bool) -> Result<bool, ProtocolError> {
    match params.get(key) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ProtocolError::new(
            INVALID_PARAMS,
            format!("`{key}` must be `true` or `false`"),
        )),
    }
}

/// Forget a cron job. Nothing but the runtime's own job table is touched: a job
/// is not a file on disk the page could name.
fn accept_cancel_cron(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let Some(id) = request.params.get("id").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`cron.cancel` needs an `id` string",
        ));
    };
    let id = id.trim();
    // A job id is not a path, and it is not free-form either: the page may only
    // name one the runtime itself handed out.
    if !is_valid_cron_id(id) {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            format!("`{id}` is not a cron job id"),
        ));
    }

    match chat.cron.cancel(id) {
        Ok(_) => {
            log(Level::Info, format!("cancelled cron job {id}"));
            Turn::answered(Response::ok(
                &request.request_id,
                json!({ "cancelled": id }),
            ))
        }
        Err(error) => rejected(ProtocolError::new(CRON_UNKNOWN, cron_message(&error))),
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

/// Every task on the workspace's board, with the state its dependencies imply.
///
/// `TaskStore` is the same store the model's `create_task` / `claim_task` /
/// `complete_task` tools write to, so a board read here is the graph the loop
/// works from rather than a second copy. A dependency that has gone missing is
/// treated as incomplete, exactly as `TaskStore::incomplete_dependencies` does.
fn tasks_snapshot(workspace: &Path) -> Result<Value, Error> {
    let store = TaskStore::new(workspace);
    let tasks = store.list()?;
    let views: Vec<Value> = tasks.iter().map(|task| task_view(&store, task)).collect();
    Ok(json!({
        "dir": store.root().display().to_string(),
        "count": views.len(),
        "tasks": views,
    }))
}

/// One task as the page lists it: the persisted fields, plus whether it can be
/// claimed now and which prerequisites are still open.
fn task_view(store: &TaskStore, task: &Task) -> Value {
    let blocked = store.incomplete_dependencies(task);
    let ready = task.status == TaskStatus::Pending && blocked.is_empty();
    json!({
        "id": task.id,
        "subject": task.subject,
        "description": task.description,
        "status": task.status,
        "owner": task.owner,
        "blockedBy": task.blocked_by,
        "worktree": task.worktree,
        "department": task.department,
        "createdBy": task.created_by,
        "blocked": blocked,
        "ready": ready,
    })
}

/// The id a task action names. It has to be one the runtime itself handed out,
/// not any string that could name a file.
fn task_id_param<'a>(request: &'a Request, method: &str) -> Result<&'a str, ProtocolError> {
    let Some(id) = request.params.get("id").and_then(Value::as_str) else {
        return Err(ProtocolError::new(
            INVALID_PARAMS,
            format!("`{method}` needs an `id` string"),
        ));
    };
    let id = id.trim();
    if !is_valid_task_id(id) {
        return Err(ProtocolError::new(
            INVALID_PARAMS,
            format!("`{id}` is not a task id"),
        ));
    }
    Ok(id)
}

/// Read one task, telling "not there" apart from "the board is unreadable".
fn load_task(store: &TaskStore, id: &str) -> Result<Task, ProtocolError> {
    if !store.exists(id) {
        return Err(ProtocolError::new(
            TASK_UNKNOWN,
            format!("task not found: {id}"),
        ));
    }
    store
        .load(id)
        .map_err(|error| ProtocolError::new(TASK_STORE, error.to_string()))
}

fn task_status_name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Completed => "completed",
    }
}

/// Add a node to the board, optionally behind prerequisites the page picked.
///
/// The dependencies are checked before the task is written, so a typo does not
/// leave a node behind that nobody asked for. A new node cannot be part of a
/// cycle: nothing can depend on an id that does not exist yet.
fn accept_create_task(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let Some(subject) = request.params.get("subject").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`tasks.create` needs a `subject` string",
        ));
    };
    let subject = subject.trim();
    if subject.is_empty() {
        return rejected(ProtocolError::new(
            TASK_INVALID,
            "a task subject cannot be empty",
        ));
    }

    let description = match request.params.get("description") {
        None => "",
        Some(Value::String(value)) => value.trim(),
        Some(_) => {
            return rejected(ProtocolError::new(
                INVALID_PARAMS,
                "`description` must be a string",
            ))
        }
    };

    let department = match request.params.get("department") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if value.trim().is_empty() => None,
        Some(Value::String(value)) => {
            let id = value.trim();
            if !is_valid_department_id(id) {
                return rejected(ProtocolError::new(
                    TASK_INVALID,
                    format!("`{id}` is not a department id"),
                ));
            }
            if !WorkforceStore::new(&chat.workspace).exists(id) {
                return rejected(ProtocolError::new(
                    DEPARTMENT_UNKNOWN,
                    format!("department not found: {id}"),
                ));
            }
            Some(id.to_string())
        }
        Some(_) => {
            return rejected(ProtocolError::new(
                INVALID_PARAMS,
                "`department` must be a department id string",
            ))
        }
    };

    let dependencies = match request.params.get("blocked_by") {
        None => Vec::new(),
        Some(Value::Array(items)) => {
            let mut dependencies = Vec::with_capacity(items.len());
            for item in items {
                let Some(id) = item.as_str() else {
                    return rejected(ProtocolError::new(
                        INVALID_PARAMS,
                        "`blocked_by` must be an array of task ids",
                    ));
                };
                let id = id.trim();
                if !is_valid_task_id(id) {
                    return rejected(ProtocolError::new(
                        TASK_INVALID,
                        format!("`{id}` is not a task id"),
                    ));
                }
                dependencies.push(id.to_string());
            }
            dependencies
        }
        Some(_) => {
            return rejected(ProtocolError::new(
                INVALID_PARAMS,
                "`blocked_by` must be an array of task ids",
            ))
        }
    };

    let store = TaskStore::new(&chat.workspace);
    for dependency in &dependencies {
        if !store.exists(dependency) {
            return rejected(ProtocolError::new(
                TASK_UNKNOWN,
                format!("task dependency not found: {dependency}"),
            ));
        }
    }

    let task = match store.create_scoped(
        subject,
        description,
        department.as_deref(),
        Some(PAGE_TASK_OWNER),
    ) {
        Ok(task) => task,
        Err(error) => return rejected(ProtocolError::new(TASK_STORE, error.to_string())),
    };
    if !dependencies.is_empty() {
        if let Err(error) = store.update_dependencies(&task.id, &dependencies) {
            return rejected(ProtocolError::new(TASK_STORE, error.to_string()));
        }
    }
    let task = match store.load(&task.id) {
        Ok(task) => task,
        Err(error) => return rejected(ProtocolError::new(TASK_STORE, error.to_string())),
    };
    log(
        Level::Info,
        format!("task {} created: {}", task.id, task.subject),
    );
    Turn::answered(Response::ok(
        &request.request_id,
        json!({ "task": task_view(&store, &task) }),
    ))
}

/// Claim a ready task for the person at the page.
///
/// A task owned by a model turn is not the page's to take: the store records
/// one owner, and completion is checked against it. The page claims as `user`,
/// which no model turn uses.
fn accept_claim_task(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let id = match task_id_param(request, "tasks.claim") {
        Ok(id) => id,
        Err(error) => return rejected(error),
    };
    let store = TaskStore::new(&chat.workspace);
    let task = match load_task(&store, id) {
        Ok(task) => task,
        Err(error) => return rejected(error),
    };
    if task.status != TaskStatus::Pending {
        return rejected(ProtocolError::new(
            TASK_NOT_READY,
            format!(
                "task {id} is {}, not pending",
                task_status_name(task.status)
            ),
        ));
    }
    let blocked = store.incomplete_dependencies(&task);
    if !blocked.is_empty() {
        return rejected(ProtocolError::new(
            TASK_BLOCKED,
            format!("task {id} is blocked by {}", blocked.join(", ")),
        ));
    }

    let message = match store.claim(id, PAGE_TASK_OWNER) {
        Ok(message) => message,
        Err(error) => return rejected(ProtocolError::new(TASK_STORE, error.to_string())),
    };
    let task = match store.load(id) {
        Ok(task) => task,
        Err(error) => return rejected(ProtocolError::new(TASK_STORE, error.to_string())),
    };
    if task.status != TaskStatus::InProgress || task.owner.as_deref() != Some(PAGE_TASK_OWNER) {
        // Another writer took it between the check and the claim.
        return rejected(ProtocolError::new(TASK_CONFLICT, message));
    }
    log(
        Level::Info,
        format!("task {id} claimed by {PAGE_TASK_OWNER}"),
    );
    Turn::answered(Response::ok(
        &request.request_id,
        json!({ "task": task_view(&store, &task), "message": message }),
    ))
}

/// Give a task the page claimed back to the board.
fn accept_release_task(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let id = match task_id_param(request, "tasks.release") {
        Ok(id) => id,
        Err(error) => return rejected(error),
    };
    let store = TaskStore::new(&chat.workspace);
    let task = match load_task(&store, id) {
        Ok(task) => task,
        Err(error) => return rejected(error),
    };
    if task.status != TaskStatus::InProgress {
        return rejected(ProtocolError::new(
            TASK_NOT_READY,
            format!("task {id} is {}", task_status_name(task.status)),
        ));
    }
    if task.owner.as_deref() != Some(PAGE_TASK_OWNER) {
        return rejected(ProtocolError::new(
            TASK_NOT_OWNED,
            format!(
                "task {id} is owned by {}, not {PAGE_TASK_OWNER}",
                task.owner.as_deref().unwrap_or("nobody")
            ),
        ));
    }
    let task = match store.release(id) {
        Ok(task) => task,
        Err(error) => return rejected(ProtocolError::new(TASK_STORE, error.to_string())),
    };
    log(Level::Info, format!("task {id} released"));
    Turn::answered(Response::ok(
        &request.request_id,
        json!({ "task": task_view(&store, &task) }),
    ))
}

/// Close a task the page claimed, reporting what that unblocked.
fn accept_complete_task(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let id = match task_id_param(request, "tasks.complete") {
        Ok(id) => id,
        Err(error) => return rejected(error),
    };
    let store = TaskStore::new(&chat.workspace);
    let task = match load_task(&store, id) {
        Ok(task) => task,
        Err(error) => return rejected(error),
    };
    if task.status != TaskStatus::InProgress {
        return rejected(ProtocolError::new(
            TASK_NOT_READY,
            format!("task {id} is {}", task_status_name(task.status)),
        ));
    }
    if task.owner.as_deref() != Some(PAGE_TASK_OWNER) {
        return rejected(ProtocolError::new(
            TASK_NOT_OWNED,
            format!(
                "task {id} is owned by {}, not {PAGE_TASK_OWNER}",
                task.owner.as_deref().unwrap_or("nobody")
            ),
        ));
    }

    let message = match store.complete(id, PAGE_TASK_OWNER) {
        Ok(message) => message,
        Err(error) => return rejected(ProtocolError::new(TASK_STORE, error.to_string())),
    };
    let task = match store.load(id) {
        Ok(task) => task,
        Err(error) => return rejected(ProtocolError::new(TASK_STORE, error.to_string())),
    };
    if task.status != TaskStatus::Completed {
        return rejected(ProtocolError::new(TASK_CONFLICT, message));
    }
    log(Level::Info, format!("task {id} completed"));
    Turn::answered(Response::ok(
        &request.request_id,
        json!({ "task": task_view(&store, &task), "message": message }),
    ))
}

// ---------------------------------------------------------------------------
// Departments and their agents
// ---------------------------------------------------------------------------

/// Every department in the workspace, with its members, group chat, task
/// counts, and the shift currently driving it.
fn departments_snapshot(chat: &Chat) -> Result<Value, Error> {
    let store = &chat.workforce.store;
    let tasks = TaskStore::new(&chat.workspace);
    let task_list = tasks.list()?;
    let departments = store.list()?;
    let views: Vec<Value> = departments
        .iter()
        .map(|department| {
            department_view(
                store,
                &tasks,
                &task_list,
                department,
                chat.workforce.shift_view(&department.id),
            )
        })
        .collect();
    Ok(json!({
        "dir": store.root().display().to_string(),
        "count": views.len(),
        "departments": views,
    }))
}

/// One department as the page lists it.
fn department_view(
    _store: &WorkforceStore,
    tasks: &TaskStore,
    task_list: &[Task],
    department: &Department,
    shift: Option<Value>,
) -> Value {
    let owned: Vec<&Task> = task_list
        .iter()
        .filter(|task| task.department.as_deref() == Some(department.id.as_str()))
        .collect();
    let count = |status: TaskStatus| owned.iter().filter(|task| task.status == status).count();
    let ready = owned
        .iter()
        .filter(|task| {
            task.status == TaskStatus::Pending && tasks.incomplete_dependencies(task).is_empty()
        })
        .count();
    let messages: Vec<Value> = department
        .messages
        .iter()
        .map(department_message_view)
        .collect();
    let agents: Vec<Value> = department.agents.iter().map(agent_view).collect();
    json!({
        "id": department.id,
        "name": department.name,
        "description": department.description,
        "instructions": department.instructions,
        "parentId": department.parent_id,
        "createdAt": department.created_at,
        "agents": agents,
        "messages": messages,
        "tasks": {
            "total": owned.len(),
            "pending": count(TaskStatus::Pending),
            "inProgress": count(TaskStatus::InProgress),
            "completed": count(TaskStatus::Completed),
            "ready": ready,
        },
        "shift": shift,
    })
}

fn agent_view(agent: &AgentProfile) -> Value {
    json!({
        "id": agent.id,
        "name": agent.name,
        "role": agent.role,
        "instructions": agent.instructions,
        "lastRunAt": agent.last_run_at,
        "lastStatus": agent.last_status,
        "lastSummary": agent.last_summary,
        "lastCreated": agent.last_created,
        "lastCompleted": agent.last_completed,
    })
}

fn department_message_view(message: &DepartmentMessage) -> Value {
    json!({
        "id": message.id,
        "senderKind": message.sender_kind,
        "senderId": message.sender_id,
        "senderName": message.sender_name,
        "content": message.content,
        "createdAt": message.created_at,
    })
}

/// The department id a request names. Like task ids, this is not a path.
fn department_id_param<'a>(request: &'a Request, method: &str) -> Result<&'a str, ProtocolError> {
    let Some(id) = request
        .params
        .get("id")
        .or_else(|| request.params.get("department_id"))
        .and_then(Value::as_str)
    else {
        return Err(ProtocolError::new(
            INVALID_PARAMS,
            format!("`{method}` needs a department id"),
        ));
    };
    let id = id.trim();
    if !is_valid_department_id(id) {
        return Err(ProtocolError::new(
            INVALID_PARAMS,
            format!("`{id}` is not a department id"),
        ));
    }
    Ok(id)
}

fn agent_id_param<'a>(request: &'a Request, method: &str) -> Result<&'a str, ProtocolError> {
    let Some(id) = request.params.get("agent_id").and_then(Value::as_str) else {
        return Err(ProtocolError::new(
            INVALID_PARAMS,
            format!("`{method}` needs an `agent_id` string"),
        ));
    };
    let id = id.trim();
    if !is_valid_agent_id(id) {
        return Err(ProtocolError::new(
            INVALID_PARAMS,
            format!("`{id}` is not an agent id"),
        ));
    }
    Ok(id)
}

fn workforce_code(error: &Error) -> &'static str {
    match error {
        Error::Workforce(message) if message.contains("not found") => DEPARTMENT_UNKNOWN,
        Error::Workforce(_) => WORKFORCE_INVALID,
        _ => WORKFORCE_STORE,
    }
}

/// Read an optional string parameter, trimming it and treating an empty value
/// as absent.
fn optional_text<'a>(request: &'a Request, key: &str) -> Result<&'a str, ProtocolError> {
    match request.params.get(key) {
        None | Some(Value::Null) => Ok(""),
        Some(Value::String(value)) => Ok(value.trim()),
        Some(_) => Err(ProtocolError::new(
            INVALID_PARAMS,
            format!("`{key}` must be a string"),
        )),
    }
}

/// Add a department, or a sub-department under an existing one.
fn accept_create_department(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let Some(name) = request.params.get("name").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`departments.create` needs a `name` string",
        ));
    };
    let description = match optional_text(request, "description") {
        Ok(value) => value,
        Err(error) => return rejected(error),
    };
    let instructions = match optional_text(request, "instructions") {
        Ok(value) => value,
        Err(error) => return rejected(error),
    };
    let parent_id = match optional_text(request, "parent_id") {
        Ok("") => None,
        Ok(value) => {
            if !is_valid_department_id(value) {
                return rejected(ProtocolError::new(
                    WORKFORCE_INVALID,
                    format!("`{value}` is not a department id"),
                ));
            }
            Some(value)
        }
        Err(error) => return rejected(error),
    };

    let store = WorkforceStore::new(&chat.workspace);
    if let Some(parent) = parent_id {
        if !store.exists(parent) {
            return rejected(ProtocolError::new(
                DEPARTMENT_UNKNOWN,
                format!("parent department not found: {parent}"),
            ));
        }
    }
    let created = match store.create_child(name, description, instructions, parent_id) {
        Ok(department) => department,
        Err(error) => {
            return rejected(ProtocolError::new(
                workforce_code(&error),
                error.to_string(),
            ))
        }
    };
    log(
        Level::Info,
        format!("department {} created: {}", created.id, created.name),
    );
    let tasks = TaskStore::new(&chat.workspace);
    let task_list = tasks.list().unwrap_or_default();
    Turn::answered(Response::ok(
        &request.request_id,
        json!({ "department": department_view(&store, &tasks, &task_list, &created, None) }),
    ))
}

/// Remove a department and every sub-department below it.
fn accept_remove_department(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let id = match department_id_param(request, "departments.remove") {
        Ok(id) => id,
        Err(error) => return rejected(error),
    };
    if let Some(active) = chat.workforce.active_department() {
        return rejected(ProtocolError::new(
            WORKFORCE_CONFLICT,
            format!("department {active} is still working; stop that shift first"),
        ));
    }
    let store = WorkforceStore::new(&chat.workspace);
    if !store.exists(id) {
        return rejected(ProtocolError::new(
            DEPARTMENT_UNKNOWN,
            format!("department not found: {id}"),
        ));
    }
    let removed = match store.remove_tree(id) {
        Ok(removed) => removed,
        Err(error) => {
            return rejected(ProtocolError::new(
                workforce_code(&error),
                error.to_string(),
            ))
        }
    };
    let ids: Vec<String> = removed
        .iter()
        .map(|department| department.id.clone())
        .collect();
    log(
        Level::Info,
        format!("removed {} department(s): {}", ids.len(), ids.join(", ")),
    );
    Turn::answered(Response::ok(&request.request_id, json!({ "removed": ids })))
}

/// Add one agent to a department.
fn accept_add_agent(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let id = match department_id_param(request, "agents.add") {
        Ok(id) => id,
        Err(error) => return rejected(error),
    };
    let Some(name) = request.params.get("name").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`agents.add` needs a `name` string",
        ));
    };
    let role = match optional_text(request, "role") {
        Ok(value) => value,
        Err(error) => return rejected(error),
    };
    let instructions = match optional_text(request, "instructions") {
        Ok(value) => value,
        Err(error) => return rejected(error),
    };
    let store = WorkforceStore::new(&chat.workspace);
    if !store.exists(id) {
        return rejected(ProtocolError::new(
            DEPARTMENT_UNKNOWN,
            format!("department not found: {id}"),
        ));
    }
    if chat.workforce.active_department().as_deref() == Some(id) {
        return rejected(ProtocolError::new(
            WORKFORCE_CONFLICT,
            "this department is working right now; add the agent after the shift stops",
        ));
    }
    let (department, agent) = match store.add_agent(id, name, role, instructions) {
        Ok(answer) => answer,
        Err(error) => {
            return rejected(ProtocolError::new(
                workforce_code(&error),
                error.to_string(),
            ))
        }
    };
    log(
        Level::Info,
        format!(
            "agent {} added to {}: {}",
            agent.id, department.id, agent.name
        ),
    );
    let tasks = TaskStore::new(&chat.workspace);
    let task_list = tasks.list().unwrap_or_default();
    Turn::answered(Response::ok(
        &request.request_id,
        json!({
            "department": department_view(
                &store,
                &tasks,
                &task_list,
                &department,
                chat.workforce.shift_view(&department.id),
            ),
            "agent": agent_view(&agent),
        }),
    ))
}

/// Remove one agent from a department.
fn accept_remove_agent(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let department_id = match department_id_param(request, "agents.remove") {
        Ok(id) => id,
        Err(error) => return rejected(error),
    };
    let agent_id = match agent_id_param(request, "agents.remove") {
        Ok(id) => id,
        Err(error) => return rejected(error),
    };
    let store = WorkforceStore::new(&chat.workspace);
    let department = match store.load(department_id) {
        Ok(department) => department,
        Err(error) => return rejected(ProtocolError::new(DEPARTMENT_UNKNOWN, error.to_string())),
    };
    if chat.workforce.active_department().as_deref() == Some(department_id) {
        return rejected(ProtocolError::new(
            WORKFORCE_CONFLICT,
            "this department is working right now; remove the agent after the shift stops",
        ));
    }
    if department.agent(agent_id).is_none() {
        return rejected(ProtocolError::new(
            AGENT_UNKNOWN,
            format!("agent not found in this department: {agent_id}"),
        ));
    }
    if chat
        .workforce
        .shift_view(department_id)
        .and_then(|shift| shift.get("currentAgentId").cloned())
        .and_then(|id| id.as_str().map(str::to_string))
        .as_deref()
        == Some(agent_id)
    {
        return rejected(ProtocolError::new(
            WORKFORCE_CONFLICT,
            "agent is working right now; stop the shift before removing it",
        ));
    }
    let department = match store.remove_agent(department_id, agent_id) {
        Ok(department) => department,
        Err(error) => {
            return rejected(ProtocolError::new(
                workforce_code(&error),
                error.to_string(),
            ))
        }
    };
    log(
        Level::Info,
        format!("agent {agent_id} removed from {department_id}"),
    );
    let tasks = TaskStore::new(&chat.workspace);
    let task_list = tasks.list().unwrap_or_default();
    Turn::answered(Response::ok(
        &request.request_id,
        json!({
            "department": department_view(
                &store,
                &tasks,
                &task_list,
                &department,
                chat.workforce.shift_view(&department.id),
            ),
        }),
    ))
}

/// Start a shift: every agent in the department, one after another, works on
/// its own department-scoped task board.
fn accept_run_department(
    request: &Request,
    chat: &Arc<Chat>,
    handle: &tokio::runtime::Handle,
) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let id = match department_id_param(request, "departments.run") {
        Ok(id) => id,
        Err(error) => return rejected(error),
    };
    let instruction = match optional_text(request, "instruction") {
        Ok(value) if value.chars().count() > MAX_MESSAGE_CHARS => {
            return rejected(ProtocolError::new(
                INVALID_PARAMS,
                format!("`instruction` is longer than {MAX_MESSAGE_CHARS} characters"),
            ))
        }
        Ok(value) => value,
        Err(error) => return rejected(error),
    };
    let store = WorkforceStore::new(&chat.workspace);
    let department = match store.load(id) {
        Ok(department) => department,
        Err(error) => return rejected(ProtocolError::new(DEPARTMENT_UNKNOWN, error.to_string())),
    };
    if department.agents.is_empty() {
        return rejected(ProtocolError::new(
            WORKFORCE_INVALID,
            "this department has no agents; add one before starting a shift",
        ));
    }
    if let Err(error) = Config::from_env() {
        return rejected(ProtocolError::new(
            CHAT_NOT_CONFIGURED,
            format!("{error}; set the model in Settings, then restart the runtime"),
        ));
    }
    let Some(stop) = chat.workforce.start(id, department.agents.len()) else {
        return rejected(ProtocolError::new(
            WORKFORCE_CONFLICT,
            "a department shift is already running; let it finish or stop it first",
        ));
    };

    let instruction = if instruction.is_empty() {
        None
    } else {
        Some(instruction.to_string())
    };
    let chat_message = instruction
        .clone()
        .unwrap_or_else(|| format!("让「{}」按部门简介开工。", department.name));
    if let Err(error) = store.append_message(id, "user", None, "我", &chat_message) {
        chat.workforce.finish(id);
        return rejected(ProtocolError::new(
            workforce_code(&error),
            error.to_string(),
        ));
    }

    log(
        Level::Info,
        format!(
            "department {} shift started with {} agent(s)",
            department.id,
            department.agents.len()
        ),
    );
    let worker = Arc::clone(chat);
    let department_id = id.to_string();
    let shift_instruction = instruction.clone();
    handle.spawn(async move {
        run_department_shift(worker, department_id, shift_instruction, stop).await;
    });

    let tasks = TaskStore::new(&chat.workspace);
    let task_list = tasks.list().unwrap_or_default();
    let department = match store.load(id) {
        Ok(department) => department,
        Err(error) => return rejected(ProtocolError::new(DEPARTMENT_UNKNOWN, error.to_string())),
    };
    Turn::answered(Response::ok(
        &request.request_id,
        json!({
            "accepted": true,
            "department": department_view(
                &store,
                &tasks,
                &task_list,
                &department,
                chat.workforce.shift_view(&department.id),
            ),
        }),
    ))
}

/// Ask the running shift to stop after the current model call.
fn accept_stop_department(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let id = match department_id_param(request, "departments.stop") {
        Ok(id) => id,
        Err(error) => return rejected(error),
    };
    let stopped = chat.workforce.stop(id);
    if stopped {
        log(Level::Info, format!("department {id} shift stop requested"));
    }
    Turn::answered(Response::ok(
        &request.request_id,
        json!({ "stopped": stopped }),
    ))
}

/// Finish a shift even if the future is dropped; a guard keeps the one-running
/// slot honest without every early return having to remember to clear it.
struct ShiftGuard {
    chat: Arc<Chat>,
    department_id: String,
}

impl Drop for ShiftGuard {
    fn drop(&mut self) {
        self.chat.workforce.finish(&self.department_id);
    }
}

/// Run the department's agents in order. Each one sees the same task board but
/// a tool pool scoped to this department and to its own owner id.
async fn run_department_shift(
    chat: Arc<Chat>,
    department_id: String,
    instruction: Option<String>,
    mut stop: watch::Receiver<bool>,
) {
    let _guard = ShiftGuard {
        chat: Arc::clone(&chat),
        department_id: department_id.clone(),
    };
    if let Err(error) =
        run_department_shift_inner(&chat, &department_id, instruction.as_deref(), &mut stop).await
    {
        log(
            Level::Error,
            format!("department {department_id} shift failed: {error}"),
        );
        let store = WorkforceStore::new(&chat.workspace);
        let _ = store.append_message(
            &department_id,
            "system",
            None,
            "系统",
            &format!("本轮工作失败：{error}"),
        );
        let _ = send(&event(
            "department.shift",
            json!({
                "department": department_id,
                "state": "failed",
                "message": error,
            }),
        ));
        return;
    }
    let _ = send(&event(
        "department.shift",
        json!({ "department": department_id, "state": "finished" }),
    ));
}

async fn run_department_shift_inner(
    chat: &Arc<Chat>,
    department_id: &str,
    instruction: Option<&str>,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let config = Config::from_env().map_err(|error| error.to_string())?;
    let client: Arc<dyn LlmClient> = Arc::new(AnthropicClient::new(&config));
    let store = WorkforceStore::new(&chat.workspace);
    let department = store
        .load(department_id)
        .map_err(|error| error.to_string())?;

    for agent in department.agents.clone() {
        if *stop.borrow() {
            break;
        }
        chat.workforce.set_current_agent(department_id, &agent.id);
        let _ = send(&event(
            "department.agent",
            json!({
                "department": department_id,
                "agent": agent.id,
                "name": agent.name,
                "state": "started",
            }),
        ));

        let task_store = TaskStore::new(&chat.workspace);
        let before = task_store.list().unwrap_or_default();
        let agent_loop = build_department_agent(
            &chat.workspace,
            Arc::clone(&client),
            &config,
            &department,
            &agent,
            instruction,
        );
        let outcome = tokio::select! {
            result = agent_loop.ask("Begin this shift now.") => Some(result),
            _ = wait_for_stop(stop) => None,
        };
        let after = task_store.list().unwrap_or_default();
        let (created, completed) = shift_counts(&before, &after, &agent.id);

        let (status, summary) = match outcome {
            None => (
                "stopped",
                "本轮被停止。未完成的任务可能仍留在任务板上。".to_string(),
            ),
            Some(Ok(text)) if text.trim().is_empty() => {
                ("empty", "本轮结束，但没有给出摘要。".to_string())
            }
            Some(Ok(text)) => ("completed", text.trim().to_string()),
            Some(Err(error)) => ("failed", format!("本轮失败：{error}")),
        };
        let recorded_summary = clamp(&summary);
        if let Err(error) = store.record_agent_run(
            department_id,
            &agent.id,
            status,
            &recorded_summary,
            created,
            completed,
        ) {
            log(
                Level::Warn,
                format!("could not record agent {} shift: {error}", agent.id),
            );
        }
        let report = if created == 0 && completed == 0 {
            summary.clone()
        } else {
            format!("{summary}\n\n新建 {created} 个任务，完成 {completed} 个任务。")
        };
        let recorded_report = clamp(&report);
        if let Err(error) = store.append_message(
            department_id,
            "agent",
            Some(&agent.id),
            &agent.name,
            &recorded_report,
        ) {
            log(
                Level::Warn,
                format!("could not write agent {} report: {error}", agent.id),
            );
        }
        chat.workforce.mark_agent_done(department_id);
        let _ = send(&event(
            "department.agent",
            json!({
                "department": department_id,
                "agent": agent.id,
                "name": agent.name,
                "state": "finished",
                "status": status,
                "created": created,
                "completed": completed,
            }),
        ));
        if *stop.borrow() {
            break;
        }
    }
    Ok(())
}

async fn wait_for_stop(stop: &mut watch::Receiver<bool>) {
    if *stop.borrow() {
        return;
    }
    let _ = stop.changed().await;
}

fn shift_counts(before: &[Task], after: &[Task], agent_id: &str) -> (usize, usize) {
    let before_ids: HashSet<&str> = before.iter().map(|task| task.id.as_str()).collect();
    let before_status: BTreeMap<&str, TaskStatus> = before
        .iter()
        .map(|task| (task.id.as_str(), task.status))
        .collect();
    let created = after
        .iter()
        .filter(|task| {
            task.created_by.as_deref() == Some(agent_id) && !before_ids.contains(task.id.as_str())
        })
        .count();
    let completed = after
        .iter()
        .filter(|task| {
            task.owner.as_deref() == Some(agent_id)
                && task.status == TaskStatus::Completed
                && before_status.get(task.id.as_str()) != Some(&TaskStatus::Completed)
        })
        .count();
    (created, completed)
}

/// The loop one department agent runs for one shift.
fn build_department_agent(
    workspace: &Path,
    client: Arc<dyn LlmClient>,
    config: &Config,
    department: &Department,
    agent: &AgentProfile,
    instruction: Option<&str>,
) -> Agent {
    let mut tools = ToolRegistry::with_defaults();
    tools.register(CreateTaskTool::scoped(&department.id, &agent.id));
    tools.register(UpdateTaskTool::in_department(&department.id));
    tools.register(ListTasksTool::in_department(&department.id));
    tools.register(GetTaskTool::in_department(&department.id));
    tools.register(ClaimTaskTool::scoped(&agent.id, &department.id));
    tools.register(CompleteTaskTool::owned_by(&agent.id));

    let mut recovery = RecoveryConfig::default();
    if let Some(model) = &config.fallback_model {
        recovery = recovery.with_fallback_model(model.clone());
    }
    Agent::builder()
        .shared_client(client)
        .tools(tools)
        .workspace(workspace.to_path_buf())
        .compaction(CompactConfig::default())
        .recovery(recovery)
        .interactive(false)
        .max_tokens(config.max_tokens)
        .max_steps(36)
        .system_prompt(department_agent_prompt(department, agent, instruction))
        .build()
}

/// What a department agent is asked to do for one shift.
fn department_agent_prompt(
    department: &Department,
    agent: &AgentProfile,
    instruction: Option<&str>,
) -> String {
    let goal = instruction
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| department.description.trim());
    let mut prompt = default_system_prompt();
    prompt.push_str(
        "\n\n## Department shift\n\
         You are an autonomous member of a department. You cannot ask the user a \
         question and no interactive approval is available. Use the task board as \
         the shared source of truth.\n",
    );
    prompt.push_str(&format!(
        "Department: {} ({})\nDepartment brief: {}\nCollaboration rules: {}\n\
         Your name: {}\nYour role: {}\nYour instructions: {}\n",
        department.name,
        department.id,
        if department.description.trim().is_empty() {
            "(none)"
        } else {
            department.description.trim()
        },
        if department.instructions.trim().is_empty() {
            "(none)"
        } else {
            department.instructions.trim()
        },
        agent.name,
        if agent.role.trim().is_empty() {
            "(generalist)"
        } else {
            agent.role.trim()
        },
        if agent.instructions.trim().is_empty() {
            "(none)"
        } else {
            agent.instructions.trim()
        },
    ));
    prompt.push_str(&format!(
        "\nThis shift's instruction: {}\n",
        if goal.is_empty() {
            "(none; improve the department's current work)"
        } else {
            goal
        }
    ));
    prompt.push_str(
        "\nHow to work this shift:\n\
         1. Call list_tasks first. It only shows this department's board.\n\
         2. If a pending, unblocked task is ready, claim one with claim_task. \
         Otherwise create one concrete task with create_task (and wire any \
         dependencies with update_task), then claim it.\n\
         3. Do the work with the tools you have. The task is not done just \
         because you described it: edit files, run checks, or otherwise verify it.\n\
         4. Call complete_task when the work is actually finished. If the work is \
         larger than one shift, complete the part that is finished and leave \
         specific follow-up tasks behind.\n\
         5. Do not claim or complete work owned by another agent, do not touch \
         another department's tasks, and do not create duplicate tasks.\n\
         6. Finish with a short report: what you created, what you claimed, what \
         you completed, and any blocker.\n",
    );
    prompt.push_str(TASKS_PROMPT);
    prompt
}

/// Every skill the loader finds, with the manifest each one came from.
///
/// The list is read fresh from `<workspace>/skills/*/SKILL.md`, the same
/// directory the loop's `SkillLoader` scans, so what the page shows is what the
/// system prompt catalogues.
fn skills_snapshot(workspace: &Path) -> Value {
    let loader = SkillLoader::new(workspace.join("skills"));
    let skills: Vec<Value> = loader.skills().map(skill_view).collect();
    json!({
        "dir": loader.dir().display().to_string(),
        "count": skills.len(),
        "skills": skills,
    })
}

/// One skill as the page lists it: the name `load_skill` takes, the line the
/// catalogue shows, and the manifest it was read from.
fn skill_view(skill: &Skill) -> Value {
    json!({
        "name": skill.name,
        "description": skill.description,
        "path": skill.path.display().to_string(),
    })
}

/// Hand back one full `SKILL.md`, the same text `load_skill` gives the model.
///
/// The page names a skill, never a path: the name is looked up in the loader the
/// runtime scanned, so this reads exactly the files the model can read.
fn accept_read_skill(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let Some(name) = request.params.get("name").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`skills.read` needs a `name` string",
        ));
    };
    let name = name.trim();
    if name.is_empty() {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`name` must not be empty",
        ));
    }

    let loader = SkillLoader::new(chat.workspace.join("skills"));
    match loader.load(name) {
        Ok(content) => {
            let mut detail = match loader.get(name) {
                Some(skill) => skill_view(skill),
                None => json!({ "name": name }),
            };
            detail["content"] = json!(content);
            Turn::answered(Response::ok(&request.request_id, detail))
        }
        Err(error) => rejected(ProtocolError::new(SKILL_UNKNOWN, error.to_string())),
    }
}

/// Copy skills into the workspace from a folder the host picked.
///
/// The folder may be one skill (`SKILL.md` in it) or a collection of them (one
/// `SKILL.md` per child folder) -- the shape a `git clone` of a skills
/// repository has. Either way the copy lands under `<workspace>/skills/`, the
/// one directory the loader scans, so the next `skills.list` sees it and the
/// model's catalogue carries it from the next turn on.
fn accept_import_skill(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |code, message| {
        Turn::answered(Response::failed(
            &request.request_id,
            ProtocolError::new(code, message),
        ))
    };
    let Some(path) = request.params.get("path").and_then(Value::as_str) else {
        return rejected(INVALID_PARAMS, "`skills.import` needs a `path` string");
    };
    let path = path.trim();
    if path.is_empty() {
        return rejected(INVALID_PARAMS, "`path` must not be empty");
    }

    match import_skills(&chat.workspace, Path::new(path)) {
        Ok(imported) => Turn::answered(Response::ok(
            &request.request_id,
            json!({
                "dir": chat.workspace.join("skills").display().to_string(),
                "count": imported.len(),
                "skills": imported,
            }),
        )),
        Err((code, message)) => rejected(code, &message),
    }
}

/// Copy every skill folder under `source` into `<workspace>/skills/`.
///
/// Existing skills are never overwritten: a name already in the workspace is
/// refused, so a re-import cannot silently replace instructions the user has
/// been editing.
fn import_skills(
    workspace: &Path,
    source: &Path,
) -> std::result::Result<Vec<Value>, (&'static str, String)> {
    if !source.is_dir() {
        return Err((
            SKILL_SOURCE,
            format!("{} is not a folder", source.display()),
        ));
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if source.join(SKILL_MANIFEST).is_file() {
        candidates.push(source.to_path_buf());
    } else {
        let entries = std::fs::read_dir(source).map_err(|error| {
            (
                SKILL_SOURCE,
                format!("could not read {}: {error}", source.display()),
            )
        })?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join(SKILL_MANIFEST).is_file() {
                candidates.push(path);
            }
        }
        candidates.sort();
    }
    if candidates.is_empty() {
        return Err((
            SKILL_INVALID,
            format!(
                "no {SKILL_MANIFEST} in {} or in any folder under it",
                source.display()
            ),
        ));
    }

    let skills_dir = workspace.join("skills");
    std::fs::create_dir_all(&skills_dir).map_err(|error| {
        (
            SKILL_SOURCE,
            format!("could not create {}: {error}", skills_dir.display()),
        )
    })?;
    // The names the loader already answers to, so an import cannot land beside
    // a skill it would silently shadow.
    let taken: Vec<String> = SkillLoader::new(&skills_dir)
        .skills()
        .map(|skill| skill.name.clone())
        .collect();

    for candidate in &candidates {
        let folder = candidate
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if folder.is_empty() {
            return Err((
                SKILL_INVALID,
                format!("{} has no folder name to import as", candidate.display()),
            ));
        }
        // The name the model will know it by: the manifest's, else the folder's.
        let manifest =
            std::fs::read_to_string(candidate.join(SKILL_MANIFEST)).map_err(|error| {
                (
                    SKILL_SOURCE,
                    format!("could not read {}: {error}", candidate.display()),
                )
            })?;
        let (metadata, _) = parse_frontmatter(&manifest);
        let name = match metadata.get("name").map(String::as_str).map(str::trim) {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => folder.clone(),
        };
        let target = skills_dir.join(&folder);
        if target.exists() || taken.contains(&name) {
            return Err((
                SKILL_EXISTS,
                format!(
                    "a skill named `{name}` is already in {}; remove it before \
                     importing it again",
                    skills_dir.display()
                ),
            ));
        }
        copy_tree(candidate, &target).map_err(|message| (SKILL_SOURCE, message))?;
    }

    // Read back through the same loader the page and the prompt use, so what is
    // reported is the name the skill is really known by.
    let loader = SkillLoader::new(&skills_dir);
    Ok(candidates
        .iter()
        .map(|candidate| {
            let target = skills_dir.join(
                candidate
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
            match loader
                .skills()
                .find(|skill| skill.path.starts_with(&target))
            {
                Some(skill) => skill_view(skill),
                None => json!({ "path": target.display().to_string() }),
            }
        })
        .collect())
}

/// Copy a folder tree. `.git` is left behind: a skill needs its files, not the
/// history it was cloned with.
fn copy_tree(from: &Path, to: &Path) -> std::result::Result<(), String> {
    std::fs::create_dir_all(to)
        .map_err(|error| format!("could not create {}: {error}", to.display()))?;
    let entries = std::fs::read_dir(from)
        .map_err(|error| format!("could not read {}: {error}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("could not read {}: {error}", from.display()))?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let target = to.join(&name);
        if path.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            std::fs::copy(&path, &target).map_err(|error| {
                format!(
                    "could not copy {} to {}: {error}",
                    path.display(),
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Every plugin this build ships, with the tools discovered from the ones the
/// runtime has connected.
///
/// A server's catalogue exists only once it is connected -- discovering it is
/// what `connect_mcp` does -- so an unconnected server lists no tools here and
/// the page says so, rather than showing a catalogue nobody has asked for.
fn plugins_snapshot(mcp: &McpRegistry) -> Value {
    let connected = mcp.connected_names();
    let definitions = mcp.tool_definitions();
    let servers: Vec<Value> = mcp
        .available()
        .iter()
        .map(|name| {
            let is_connected = connected.iter().any(|connected| connected == name);
            let prefix = format!("mcp__{name}__");
            let tools: Vec<Value> = definitions
                .iter()
                .filter(|definition| definition.name.starts_with(&prefix))
                .map(|definition| {
                    json!({
                        "name": definition.name,
                        "description": definition.description,
                        // The host decides this, not the server, so the page
                        // shows the policy the permission hook enforces.
                        "policy": mcp.policy_for(&definition.name).as_str(),
                    })
                })
                .collect();
            let declared = mcp.plugin(name);
            json!({
                "name": name,
                "connected": is_connected,
                "tools": tools,
                // Where the server comes from, so the page can say why it is
                // on the list: compiled in, or declared by the workspace.
                "source": if declared.is_some() { "workspace" } else { "builtin" },
                "command": declared.map(|spec| {
                    let mut command = spec.command;
                    if !spec.args.is_empty() {
                        command.push(' ');
                        command.push_str(&spec.args.join(" "));
                    }
                    command
                }),
            })
        })
        .collect();
    json!({
        "available": servers.len(),
        "connected": connected,
        "servers": servers,
    })
}

/// Connect a plugin this build ships, discovering its tools.
///
/// The name is checked against the ones this build knows before anything is
/// built, so an unknown name is refused with a code rather than answered with a
/// string the page would have to parse.
fn accept_connect_plugin(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let Some(server) = request.params.get("server").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`plugins.connect` needs a `server` string",
        ));
    };
    let server = server.trim();
    if !chat.mcp.available().iter().any(|known| known == server) {
        return rejected(ProtocolError::new(
            PLUGIN_UNKNOWN,
            format!(
                "unknown plugin `{server}`; available: {}",
                chat.mcp.available().join(", ")
            ),
        ));
    }

    match chat.mcp.connect(server) {
        Ok(message) => {
            log(Level::Info, format!("plugin: {message}"));
            Turn::answered(Response::ok(
                &request.request_id,
                json!({ "server": server, "message": message }),
            ))
        }
        Err(error) => rejected(ProtocolError::new(PLUGIN_FAILED, error.to_string())),
    }
}

/// Ask the turn that is running to stop.
///
/// A stop that finds nothing running answers at once. A stop that reaches a turn
/// is answered by that turn instead, once it is over and the slot it held is
/// free: the page that pressed stop then sends its next prompt, and that prompt
/// must be taken rather than refused as a turn already running.
fn accept_cancel(request: &Request, chat: &Arc<Chat>, handle: &tokio::runtime::Handle) -> Turn {
    // A turn parked on a question is not listening for the stop: it is blocked
    // inside a tool call. Letting it go first is what makes stopping work at
    // all while the runtime is waiting on the page.
    chat.hub.withdraw();
    let Some(mut over) = chat.cancel.stop() else {
        return Turn::answered(Response::ok(
            &request.request_id,
            json!({ "cancelled": false }),
        ));
    };

    let request_id = request.request_id.clone();
    handle.spawn(async move {
        // `false` is the honest answer for a turn that finished a moment after
        // the stop was asked for -- and the answer left behind by a turn that
        // ended some other way than through its own channel.
        let mut stopped = false;
        if over.changed().await.is_ok() {
            stopped = *over.borrow_and_update();
        }
        let _ = send(&Response::ok(&request_id, json!({ "cancelled": stopped })));
    });

    Turn::owed()
}

/// Answer the question the runtime is waiting on.
///
/// The answer is the page's to give, and nothing else here is: the id names a
/// question the runtime is already holding, and a question that is over -- or
/// that never reached this build -- is told so rather than answered twice.
fn accept_answer(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let Some(id) = request.params.get("id").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`chat.answer` needs the `id` of the question it answers",
        ));
    };
    let Some(answer) = request.params.get("answer").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`chat.answer` needs an `answer` string",
        ));
    };
    // Blank is the user saying nothing, which is a thing they are allowed to
    // do: the model is told nobody answered rather than handed an empty answer
    // to read something into.
    let answer = answer.trim();
    let answer = (!answer.is_empty()).then(|| answer.to_string());
    let accepted = chat.hub.deliver(id, Reply::Answer(answer));
    Turn::answered(Response::ok(
        &request.request_id,
        json!({ "accepted": accepted }),
    ))
}

/// Say what to do about the call the runtime is waiting on.
///
/// The same shape as [`accept_answer`] and for the same reason: the decision
/// goes to the one call the runtime is holding, and a decision that arrives
/// after that call is over changes nothing.
fn accept_approve(request: &Request, chat: &Arc<Chat>) -> Turn {
    let rejected = |error| Turn::answered(Response::failed(&request.request_id, error));
    let Some(id) = request.params.get("id").and_then(Value::as_str) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`chat.approve` needs the `id` of the call it decides",
        ));
    };
    let Some(allow) = request.params.get("allow").and_then(Value::as_bool) else {
        return rejected(ProtocolError::new(
            INVALID_PARAMS,
            "`chat.approve` needs an `allow` boolean",
        ));
    };
    let accepted = chat.hub.deliver(id, Reply::Verdict(allow));
    Turn::answered(Response::ok(
        &request.request_id,
        json!({ "accepted": accepted }),
    ))
}

/// Watch the clock, and start the turn a due job is owed.
///
/// The CLI and the TUI wake their loop the same way; without a wake here, a job
/// scheduled for 09:00 would not fire until somebody happened to send a prompt,
/// which makes the schedule a decoration. The ticker only marks jobs due and
/// starts the turn -- the loop injects the `[Scheduled]` message and
/// acknowledges the job itself, so there is exactly one delivery path.
fn spawn_cron_watch(chat: Arc<Chat>, handle: tokio::runtime::Handle) {
    let turns = handle.clone();
    handle.spawn(async move {
        let mut warned = false;
        let mut ticker = tokio::time::interval(CRON_TICK);
        loop {
            ticker.tick().await;
            if let Err(error) = chat.cron.poll(CronScheduler::now()) {
                log(Level::Warn, format!("cron: poll failed: {error}"));
                continue;
            }
            if chat.busy.load(Ordering::SeqCst) || !chat.cron.has_due() {
                continue;
            }
            // A due job with no model to hand it to cannot become a turn, and
            // saying so once beats a failure every second.
            if chat.agent().is_err() {
                if !warned {
                    warned = true;
                    log(
                        Level::Warn,
                        "cron: a scheduled job is due, but no model is configured to run it",
                    );
                }
                continue;
            }
            warned = false;
            start_scheduled_turn(&chat, &turns);
        }
    });
}

/// The transcript of a turn the runtime starts on its own: one `[Scheduled]`
/// line per job that is waiting to be delivered, in id order, or the empty
/// string when none is. It is what the loop is about to hand the model, so the
/// page shows the words the model actually received.
fn due_prompt(jobs: &[CronJob]) -> String {
    jobs.iter()
        .filter(|job| job.pending_delivery)
        .map(|job| format!("[Scheduled] {}", job.prompt))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Start a turn for the jobs that are due.
///
/// The turn carries no prompt of the host's: the loop reads the due jobs and
/// injects `[Scheduled] <prompt>` itself, so the transcript names what the model
/// actually received. The turn is otherwise a turn like any other -- it holds
/// the one-turn slot, it can be stopped, and what it produces is committed.
fn start_scheduled_turn(chat: &Arc<Chat>, handle: &tokio::runtime::Handle) {
    if chat.busy.swap(true, Ordering::SeqCst) {
        return;
    }
    // A scheduled turn has no page in front of it, so nothing is asked that
    // somebody would have to be there to answer.
    chat.hub.attend(false);
    let prompt = due_prompt(&chat.cron.jobs());
    if prompt.is_empty() {
        // Nothing left to deliver; the slot goes straight back.
        chat.busy.store(false, Ordering::SeqCst);
        return;
    }

    let turn = chat.next_turn();
    chat.log.begin(&turn, &prompt);
    let (stop, over) = chat.cancel.arm();
    let chat = Arc::clone(chat);
    let running = turn.clone();
    handle.spawn(async move {
        let busy = Busy(&chat.busy);
        chat.tag.set(&running);
        let stopped = run_turn(&chat, &running, None, stop).await;
        chat.cancel.disarm();
        chat.hub.withdraw();
        drop(busy);
        let _ = over.send(stopped);
    });
}

/// Run one prompt and stream it back.
///
/// A second prompt is refused while this one runs rather than queued behind it,
/// and a turn that failed -- or was stopped -- is rolled back instead of leaving
/// half an exchange for the model to trip over next time. `prompt` is the words
/// the user typed, or `None` for a turn the runtime started on its own: a due
/// scheduled job is injected by the loop itself, so there is no user message to
/// add here. Answers whether it was the stop, rather than the model, that ended
/// the turn.
async fn run_turn(
    chat: &Chat,
    turn: &str,
    prompt: Option<&str>,
    stop: tokio::sync::watch::Receiver<bool>,
) -> bool {
    let agent = match chat.agent() {
        Ok(agent) => agent,
        Err(error) => {
            let failed = failed_event(turn, &error);
            chat.log.record(&failed);
            chat.commit(None);
            let _ = send(&failed);
            return false;
        }
    };

    // The messages are taken out for the turn rather than locked for it, so a
    // page asking what the conversation holds is not made to wait for the model.
    let mut history = chat.session().messages;
    let rollback = history.len();
    if let Some(prompt) = prompt {
        history.push(Message::user_text(agent.prepare_prompt(prompt)));
    }

    let mut stop = stop;
    let outcome = tokio::select! {
        // The turn finishing beats a stop that arrives with it: there is an
        // answer, and it is not thrown away for a button pressed too late.
        biased;
        result = agent.run_turn(&mut history) => Some(result),
        _ = stop.changed() => None,
    };

    match outcome {
        Some(Ok(())) => {
            let text = history
                .last()
                .filter(|message| message.role == Role::Assistant)
                .map(Message::text)
                .unwrap_or_default();
            let done = event(
                "chat.done",
                json!({ "turn": turn, "ok": true, "text": text }),
            );
            // Written down before it is announced. `chat.done` is the page's
            // signal that the turn is over, so by the time it arrives the
            // conversation on disk agrees -- and a prompt that follows it is
            // refused only if a turn really is still running, never because a
            // file is still being written.
            chat.log.record(&done);
            chat.commit(Some(history));
            let _ = send(&done);
            false
        }
        Some(Err(error)) => {
            history.truncate(rollback);
            let failed = failed_event(turn, &turn_error(&error));
            chat.log.record(&failed);
            chat.commit(Some(history));
            let _ = send(&failed);
            false
        }
        None => {
            // Stopped on request. Whatever the model streamed stays on the page,
            // but the attempt is not fed back to it: the loop would otherwise
            // have to reason about an answer that was never finished.
            history.truncate(rollback);
            let cancelled = event("chat.cancelled", json!({ "turn": turn }));
            chat.log.record(&cancelled);
            chat.commit(Some(history));
            let _ = send(&cancelled);
            true
        }
    }
}

/// The event for a turn that produced no answer.
fn failed_event(turn: &str, error: &ProtocolError) -> EventMessage {
    event(
        "chat.failed",
        json!({
            "turn": turn,
            "ok": false,
            "error": { "code": error.code, "message": error.message },
        }),
    )
}

/// A stable code and the model's own words for a turn that failed.
fn turn_error(error: &Error) -> ProtocolError {
    let code = match error {
        Error::Api {
            status: 401 | 403, ..
        } => "chat_unauthorized",
        Error::Api { status: 429, .. } => "chat_rate_limited",
        Error::Api { .. } => "chat_api_error",
        Error::Config(_) => CHAT_NOT_CONFIGURED,
        Error::MaxIterations(_) => "chat_step_limit",
        Error::Recovery(_) => "chat_recovery_failed",
        _ => "chat_turn_failed",
    };
    ProtocolError::new(code, error.to_string())
}
/// One framed inbound line.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    Text(String),
    /// Empty or whitespace-only; hosts are allowed to pad the stream.
    Blank,
    /// Longer than the cap. The rest of the line was drained and dropped.
    TooLong,
    InvalidUtf8,
    Eof,
}

/// Read one newline-terminated line, never buffering more than `cap` bytes.
fn read_capped<R: BufRead>(reader: &mut R, cap: usize) -> io::Result<Line> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut eof = false;

    loop {
        let available = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            eof = true;
            break;
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let used = newline.map_or(available.len(), |position| position + 1);
        let payload = &available[..used - usize::from(newline.is_some())];
        if !truncated {
            if bytes.len() + payload.len() > cap {
                truncated = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(payload);
            }
        }
        reader.consume(used);
        if newline.is_some() {
            break;
        }
    }

    if truncated {
        return Ok(Line::TooLong);
    }
    if bytes.is_empty() {
        return Ok(if eof { Line::Eof } else { Line::Blank });
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    match String::from_utf8(bytes) {
        Ok(text) if text.trim().is_empty() => Ok(Line::Blank),
        Ok(text) => Ok(Line::Text(text)),
        Err(_) => Ok(Line::InvalidUtf8),
    }
}
// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

struct Options {
    workspace: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
}

/// `Ok(None)` means an informational flag was handled and we should exit 0.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Options>, String> {
    let args: Vec<String> = args.collect();
    let mut workspace = None;
    let mut sessions_dir = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" => {
                index += 1;
                match args.get(index) {
                    Some(value) => workspace = Some(PathBuf::from(value)),
                    None => return Err("--workspace needs a directory path".to_string()),
                }
            }
            "--sessions-dir" => {
                index += 1;
                match args.get(index) {
                    Some(value) => sessions_dir = Some(PathBuf::from(value)),
                    None => return Err("--sessions-dir needs a directory path".to_string()),
                }
            }
            "--help" | "-h" => {
                let _ = write!(io::stderr(), "{USAGE}");
                return Ok(None);
            }
            "--version" | "-V" => {
                let _ = writeln!(io::stderr(), "{}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
        index += 1;
    }
    Ok(Some(Options {
        workspace,
        sessions_dir,
    }))
}

/// Make `requested` absolute without touching the filesystem.
///
/// `canonicalize` would resolve more, but it hands Windows callers a `\\?\`
/// verbatim path, and `std::path::absolute` is newer than this crate's declared
/// minimum Rust version. A relative path is therefore resolved against the
/// current directory and an absolute one is kept as it is; `run` checks that the
/// result is a directory before the bridge starts.
fn resolve_path(requested: &Path) -> io::Result<PathBuf> {
    if requested.is_absolute() {
        return Ok(requested.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(requested))
}

/// Read the workspace's own `.env`, the way the runtime's other binaries read
/// one from their working directory.
///
/// `dotenvy` never overwrites a variable that is already set, and the host sets
/// the ones the user saved in Settings before spawning this process. So what
/// the user saved wins, and a `.env` in the project fills in only what is still
/// missing -- it cannot silently redirect a runtime the UI believes it
/// configured.
fn load_workspace_env(workspace: &Path) {
    let path = workspace.join(".env");
    match dotenvy::from_path(&path) {
        Ok(_) => log(Level::Info, format!("loaded {}", path.display())),
        Err(error) if error.not_found() => {}
        Err(error) => log(
            Level::Warn,
            format!("could not read {}: {error}", path.display()),
        ),
    }
}

fn run() -> i32 {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => return EXIT_OK,
        Err(message) => {
            log(Level::Error, message);
            let _ = write!(io::stderr(), "{USAGE}");
            return EXIT_USAGE;
        }
    };

    let requested = match options.workspace {
        Some(path) => path,
        None => match std::env::current_dir() {
            Ok(dir) => dir,
            Err(error) => {
                log(Level::Error, format!("no workspace and no cwd: {error}"));
                return EXIT_WORKSPACE;
            }
        },
    };

    let workspace = match resolve_path(&requested) {
        Ok(path) => path,
        Err(error) => {
            log(
                Level::Error,
                format!("could not resolve {}: {error}", requested.display()),
            );
            return EXIT_WORKSPACE;
        }
    };
    if !workspace.is_dir() {
        log(
            Level::Error,
            format!("workspace is not a directory: {}", workspace.display()),
        );
        return EXIT_WORKSPACE;
    }
    // Where conversations live. The host names it when it has a store of its
    // own; otherwise they stay under the workspace, which is where the
    // CLI-driven runtime has always kept them. It is created here so
    // `runtime.status` reports a directory that exists, and so a bad argument
    // fails the start rather than the first prompt.
    let sessions_dir = match options.sessions_dir {
        Some(requested) => {
            let dir = match resolve_path(&requested) {
                Ok(path) => path,
                Err(error) => {
                    log(
                        Level::Error,
                        format!("could not resolve {}: {error}", requested.display()),
                    );
                    return EXIT_STORE;
                }
            };
            if let Err(error) = std::fs::create_dir_all(&dir) {
                log(
                    Level::Error,
                    format!(
                        "could not use the sessions directory {}: {error}",
                        dir.display()
                    ),
                );
                return EXIT_STORE;
            }
            dir
        }
        None => workspace.join(SESSIONS_DIR),
    };
    // Before the bridge reads any configuration, so `runtime.ready` reports
    // what this process will actually use.
    load_workspace_env(&workspace);
    log(
        Level::Info,
        format!(
            "bridge {} starting (protocol {PROTOCOL_VERSION}, pid {})",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        ),
    );
    let bridge = Bridge::new(workspace.clone(), sessions_dir.clone());
    log(Level::Info, format!("workspace: {}", workspace.display()));
    log(Level::Info, format!("sessions: {}", sessions_dir.display()));

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());

    if let Err(error) = send(&event("runtime.ready", bridge.snapshot())) {
        log(Level::Error, format!("cannot write to stdout: {error}"));
        return EXIT_IO;
    }

    // The conversation outlives any one prompt, and a turn runs off this thread
    // so the reader loop keeps answering while a model call is in flight.
    let chat = Arc::new(Chat::new(
        workspace.clone(),
        sessions_dir,
        bridge.cron.clone(),
        bridge.mcp.clone(),
    ));
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("rust-agent-turn")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log(
                Level::Error,
                format!("could not start the turn runtime: {error}"),
            );
            return EXIT_IO;
        }
    };
    let handle = runtime.handle().clone();

    // A schedule nobody wakes is a decoration: this is what makes a due job
    // start a turn of its own while the host has nothing else to say.
    spawn_cron_watch(Arc::clone(&chat), handle.clone());

    let mut code = EXIT_OK;
    loop {
        let line = match read_capped(&mut reader, MAX_LINE_BYTES) {
            Ok(line) => line,
            Err(error) => {
                log(Level::Error, format!("stdin read failed: {error}"));
                code = EXIT_IO;
                break;
            }
        };

        let turn = match line {
            Line::Eof => {
                log(Level::Info, "stdin closed; host is gone, exiting");
                break;
            }
            Line::Blank => continue,
            Line::TooLong => {
                let error = ProtocolError::new(
                    MESSAGE_TOO_LARGE,
                    format!("message exceeded {MAX_LINE_BYTES} bytes and was discarded"),
                );
                let _ = send(&ErrorMessage::new(error));
                continue;
            }
            Line::InvalidUtf8 => {
                let error = ProtocolError::new(INVALID_UTF8, "message was not valid UTF-8");
                let _ = send(&ErrorMessage::new(error));
                continue;
            }
            Line::Text(text) => match parse_request(&text) {
                Parsed::Request(request) => dispatch(&request, &bridge, &chat, &handle),
                // We know which request failed, so answer in the response shape
                // the host is already waiting on.
                Parsed::Rejected {
                    request_id: Some(request_id),
                    error,
                } => Turn {
                    response: Some(Response::failed(&request_id, error)),
                    shutdown: false,
                },
                Parsed::Rejected {
                    request_id: None,
                    error,
                } => {
                    let _ = send(&ErrorMessage::new(error));
                    continue;
                }
            },
        };

        // A request whose answer is still owed is not written here: the turn
        // that owes it writes it, once the slot it held is free.
        if let Some(response) = &turn.response {
            if let Err(error) = send(response) {
                log(Level::Error, format!("cannot write to stdout: {error}"));
                code = EXIT_IO;
                break;
            }
        }

        if turn.shutdown {
            log(Level::Info, "shutdown requested; stopping");
            let _ = send(&event("runtime.stopping", json!({ "reason": "requested" })));
            break;
        }
    }

    log(Level::Info, format!("bridge stopped (exit {code})"));
    // A turn that is mid-call must not hold up the exit the host asked for;
    // `main` ends the process as soon as this returns.
    runtime.shutdown_background();
    code
}

fn main() {
    std::process::exit(run());
}
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;

    fn request(id: &str, method: &str) -> String {
        json!({
            "protocol_version": PROTOCOL_VERSION,
            "type": "request",
            "request_id": id,
            "method": method,
            "params": {},
        })
        .to_string()
    }

    fn bridge_at(dir: &std::path::Path) -> Bridge {
        Bridge::new(dir.to_path_buf(), dir.join(SESSIONS_DIR))
    }

    /// The pieces `dispatch` needs, for the tests that go through it.
    fn fixture(dir: &std::path::Path) -> (Bridge, Arc<Chat>, tokio::runtime::Runtime) {
        // The chat and the snapshot share one job table, exactly as `run`
        // wires them: a job the page schedules has to be the one the status
        // counts and the ticker fires.
        let bridge = bridge_at(dir);
        let chat = Arc::new(Chat::new(
            dir.to_path_buf(),
            dir.join(SESSIONS_DIR),
            bridge.cron.clone(),
            bridge.mcp.clone(),
        ));
        (bridge, chat, tokio::runtime::Runtime::new().unwrap())
    }

    /// The answer a dispatched request gave on the spot. A request whose answer
    /// the turn it stopped still owes has none here.
    fn answered(turn: &Turn) -> &Response {
        turn.response
            .as_ref()
            .expect("expected an answer from `dispatch`")
    }

    #[test]
    fn a_well_formed_request_parses() {
        match parse_request(&request("r-1", "runtime.ping")) {
            Parsed::Request(request) => {
                assert_eq!(request.request_id, "r-1");
                assert_eq!(request.method, "runtime.ping");
            }
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_type_is_still_a_request() {
        let line = json!({
            "protocol_version": PROTOCOL_VERSION,
            "request_id": "r-1",
            "method": "runtime.status",
        })
        .to_string();
        assert!(matches!(parse_request(&line), Parsed::Request(_)));
    }

    #[test]
    fn another_message_type_is_refused() {
        let line = json!({
            "protocol_version": PROTOCOL_VERSION,
            "type": "event",
            "request_id": "r-1",
            "method": "runtime.ping",
        })
        .to_string();
        match parse_request(&line) {
            Parsed::Rejected { request_id, error } => {
                assert_eq!(request_id.as_deref(), Some("r-1"));
                assert_eq!(error.code, INVALID_REQUEST);
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn an_unsupported_version_keeps_the_request_id_for_matching() {
        let line = json!({
            "protocol_version": PROTOCOL_VERSION + 1,
            "request_id": "r-9",
            "method": "runtime.ping",
        })
        .to_string();
        match parse_request(&line) {
            Parsed::Rejected { request_id, error } => {
                assert_eq!(request_id.as_deref(), Some("r-9"));
                assert_eq!(error.code, UNSUPPORTED_PROTOCOL_VERSION);
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_request_id_cannot_be_matched() {
        let line = json!({
            "protocol_version": PROTOCOL_VERSION,
            "method": "runtime.ping",
        })
        .to_string();
        match parse_request(&line) {
            Parsed::Rejected { request_id, error } => {
                assert!(request_id.is_none());
                assert_eq!(error.code, INVALID_REQUEST);
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }
    #[test]
    fn malformed_json_and_non_objects_are_reported_separately() {
        match parse_request("{not json") {
            Parsed::Rejected { error, .. } => assert_eq!(error.code, INVALID_JSON),
            other => panic!("expected a rejection, got {other:?}"),
        }
        match parse_request("[1, 2, 3]") {
            Parsed::Rejected { error, .. } => assert_eq!(error.code, INVALID_REQUEST),
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn array_params_and_unknown_keys_are_refused() {
        let line = json!({
            "protocol_version": PROTOCOL_VERSION,
            "request_id": "r-1",
            "method": "runtime.ping",
            "params": ["nope"],
        })
        .to_string();
        match parse_request(&line) {
            Parsed::Rejected { error, .. } => assert_eq!(error.code, INVALID_PARAMS),
            other => panic!("expected a rejection, got {other:?}"),
        }

        let line = json!({
            "protocol_version": PROTOCOL_VERSION,
            "request_id": "r-1",
            "method": "runtime.ping",
            "params": { "wait": true },
        })
        .to_string();
        match parse_request(&line) {
            Parsed::Rejected { error, .. } => {
                assert_eq!(error.code, INVALID_PARAMS);
                assert!(error.message.contains("wait"));
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_method_answers_on_the_same_request_id() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = bridge_at(dir.path());
        let Parsed::Request(request) = parse_request(&request("r-7", "runtime.teleport")) else {
            panic!("expected a request");
        };
        let chat = Arc::new(Chat::new(
            dir.path().to_path_buf(),
            dir.path().join(SESSIONS_DIR),
            bridge.cron.clone(),
            bridge.mcp.clone(),
        ));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let turn = dispatch(&request, &bridge, &chat, runtime.handle());
        assert!(!turn.shutdown);
        let answer = answered(&turn);
        assert_eq!(answer.request_id, "r-7");
        assert!(!answer.ok);
        assert_eq!(
            answer.error.as_ref().map(|error| error.code.as_str()),
            Some(UNKNOWN_METHOD)
        );
    }

    #[test]
    fn ping_answers_and_shutdown_asks_to_stop() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = bridge_at(dir.path());

        let Parsed::Request(ping) = parse_request(&request("r-1", "runtime.ping")) else {
            panic!("expected a request");
        };
        let chat = Arc::new(Chat::new(
            dir.path().to_path_buf(),
            dir.path().join(SESSIONS_DIR),
            bridge.cron.clone(),
            bridge.mcp.clone(),
        ));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let turn = dispatch(&ping, &bridge, &chat, runtime.handle());
        assert!(answered(&turn).ok);
        assert!(!turn.shutdown);
        assert_eq!(answered(&turn).result.clone().unwrap()["pong"], json!(true));

        let Parsed::Request(shutdown) = parse_request(&request("r-2", "runtime.shutdown")) else {
            panic!("expected a request");
        };
        let turn = dispatch(&shutdown, &bridge, &chat, runtime.handle());
        assert!(answered(&turn).ok);
        assert!(turn.shutdown);
    }

    #[test]
    fn chat_send_is_advertised_and_is_the_method_that_takes_parameters() {
        assert!(METHODS.contains(&"chat.send"));

        let line = json!({
            "protocol_version": PROTOCOL_VERSION,
            "request_id": "r-1",
            "method": "chat.send",
            "params": { "prompt": "hello" },
        })
        .to_string();
        match parse_request(&line) {
            Parsed::Request(request) => {
                assert_eq!(request.params["prompt"], json!("hello"));
            }
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[test]
    fn a_turn_reads_back_the_way_it_arrived() {
        let log = TurnLog::new();
        log.begin("turn-1", "总结一下这个工作区");
        log.record(&event(
            "chat.tool_call",
            json!({ "turn": "turn-1", "name": "bash", "input": "{}" }),
        ));
        log.record(&event(
            "chat.delta",
            json!({ "turn": "turn-1", "text": "好的" }),
        ));
        log.record(&event(
            "chat.done",
            json!({ "turn": "turn-1", "ok": true, "text": "好的" }),
        ));

        let snapshot = log.snapshot();
        let turns = snapshot["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["turn"], json!("turn-1"));
        assert_eq!(turns[0]["prompt"], json!("总结一下这个工作区"));
        assert_eq!(turns[0]["status"], json!("done"));
        assert_eq!(turns[0]["error"], json!(null));
        let names: Vec<&str> = turns[0]["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["event"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["chat.tool_call", "chat.delta", "chat.done"]);
    }

    #[test]
    fn a_turn_that_is_still_running_reads_back_unfinished() {
        let log = TurnLog::new();
        log.begin("turn-1", "hi");
        log.record(&event(
            "chat.delta",
            json!({ "turn": "turn-1", "text": "答" }),
        ));

        let snapshot = log.snapshot();
        assert_eq!(snapshot["turns"][0]["status"], json!("running"));
        assert_eq!(snapshot["turns"][0]["error"], json!(null));
    }

    #[test]
    fn a_failed_turn_keeps_the_code_the_page_branches_on() {
        let log = TurnLog::new();
        log.begin("turn-1", "hi");
        log.record(&event(
            "chat.failed",
            json!({
                "turn": "turn-1",
                "ok": false,
                "error": { "code": "chat_unauthorized", "message": "the key is wrong" },
            }),
        ));

        let snapshot = log.snapshot();
        assert_eq!(snapshot["turns"][0]["status"], json!("failed"));
        assert_eq!(
            snapshot["turns"][0]["error"]["code"],
            json!("chat_unauthorized")
        );
    }

    #[test]
    fn the_log_drops_the_oldest_turns_and_keeps_the_one_that_is_running() {
        let log = TurnLog::new();
        for index in 0..MAX_LOG_TURNS + 5 {
            let turn = format!("turn-{index}");
            log.begin(&turn, "hi");
            log.record(&event(
                "chat.done",
                json!({ "turn": turn, "ok": true, "text": "" }),
            ));
        }

        let snapshot = log.snapshot();
        let turns = snapshot["turns"].as_array().unwrap();
        assert_eq!(turns.len(), MAX_LOG_TURNS);
        assert_eq!(turns[0]["turn"], json!("turn-5"));
        assert_eq!(
            turns[turns.len() - 1]["turn"],
            json!(format!("turn-{}", MAX_LOG_TURNS + 4))
        );
    }

    #[test]
    fn an_event_for_a_turn_that_never_started_is_not_recorded() {
        let log = TurnLog::new();
        log.record(&event(
            "chat.delta",
            json!({ "turn": "turn-9", "text": "orphan" }),
        ));
        log.record(&event("chat.delta", json!({ "text": "no turn at all" })));

        assert!(log.snapshot()["turns"].as_array().unwrap().is_empty());
    }

    #[test]
    fn chat_history_answers_with_the_turns_the_bridge_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        chat.log.begin("turn-1", "hi");
        chat.log.record(&event(
            "chat.done",
            json!({ "turn": "turn-1", "ok": true, "text": "hello" }),
        ));

        let Parsed::Request(history) = parse_request(&request("r-1", "chat.history")) else {
            panic!("expected a request");
        };
        let turn = dispatch(&history, &bridge, &chat, runtime.handle());
        assert!(answered(&turn).ok);
        assert!(!turn.shutdown);

        let result = answered(&turn).result.clone().unwrap();
        assert_eq!(result["turns"][0]["prompt"], json!("hi"));
        assert_eq!(result["turns"][0]["status"], json!("done"));
    }

    #[test]
    fn chat_history_is_advertised_and_takes_no_parameters() {
        assert!(METHODS.contains(&"chat.history"));
        assert!(!takes_params("chat.history"));
    }

    #[test]
    fn a_prompt_that_cannot_run_is_refused_before_anything_starts() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());

        for (case, params) in [
            ("missing", json!({})),
            ("not a string", json!({ "prompt": 7 })),
            ("empty", json!({ "prompt": "   " })),
            (
                "too long",
                json!({ "prompt": "x".repeat(MAX_PROMPT_BYTES + 1) }),
            ),
        ] {
            let line = json!({
                "protocol_version": PROTOCOL_VERSION,
                "request_id": "r-1",
                "method": "chat.send",
                "params": params,
            })
            .to_string();
            let Parsed::Request(request) = parse_request(&line) else {
                panic!("expected a request");
            };
            let turn = dispatch(&request, &bridge, &chat, runtime.handle());
            assert!(!turn.shutdown);
            assert!(!answered(&turn).ok, "`{case}` was accepted");
            assert_eq!(
                answered(&turn)
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some(INVALID_PARAMS),
                "`{case}`"
            );
        }

        // Nothing was started, so the one-turn-at-a-time slot is still free.
        assert!(!chat.busy.load(Ordering::SeqCst));
    }

    #[test]
    fn a_failed_turn_carries_a_code_the_page_can_branch_on() {
        let status = |status: u16| Error::Api {
            status,
            message: "no".to_string(),
        };
        assert_eq!(turn_error(&status(401)).code, "chat_unauthorized");
        assert_eq!(turn_error(&status(403)).code, "chat_unauthorized");
        assert_eq!(turn_error(&status(429)).code, "chat_rate_limited");
        assert_eq!(turn_error(&status(500)).code, "chat_api_error");
        assert_eq!(
            turn_error(&Error::Config("no key".to_string())).code,
            CHAT_NOT_CONFIGURED
        );
        assert_eq!(
            turn_error(&Error::MaxIterations(64)).code,
            "chat_step_limit"
        );
        assert_eq!(
            turn_error(&Error::Stream("cut off".to_string())).code,
            "chat_turn_failed"
        );
    }

    #[test]
    fn a_long_tool_result_is_clamped_on_a_character_boundary() {
        assert_eq!(clamp("ok"), "ok");

        // The cap lands inside a multi-byte character, so the cut has to back up
        // to the boundary instead of splitting one.
        let text = format!(
            "{}{}",
            "a".repeat(MAX_EVENT_TEXT_BYTES - 1),
            "é".repeat(100)
        );
        let clamped = clamp(&text);
        assert!(clamped.len() < text.len());
        assert!(clamped.ends_with("more bytes omitted]"));
        assert_eq!(
            clamped.lines().next().unwrap().len(),
            MAX_EVENT_TEXT_BYTES - 1
        );
    }

    #[test]
    fn only_the_conversation_observers_prose_is_an_answer() {
        let input = json!({ "command": "ls" });
        let events = [
            AgentEvent::Step(0),
            AgentEvent::AssistantText("hello"),
            AgentEvent::ToolCall {
                name: "bash",
                input: &input,
            },
            AgentEvent::ToolResult {
                name: "bash",
                output: "a\nb",
                is_error: false,
            },
            AgentEvent::Blocked {
                name: "write_file",
                reason: "denied",
            },
        ];

        let names: Vec<Option<&str>> = events
            .iter()
            .map(|event| describe(event, true).map(|(name, _)| name))
            .collect();
        assert_eq!(
            names,
            vec![
                None,
                Some("chat.delta"),
                Some("chat.tool_call"),
                Some("chat.tool_result"),
                Some("chat.blocked"),
            ]
        );

        // A subagent's prose is not the user's answer; its tool use still is.
        assert!(describe(&events[1], false).is_none());
        assert_eq!(
            describe(&events[2], false).map(|(name, _)| name),
            Some("chat.tool_call")
        );
    }

    #[test]
    fn a_turn_is_attributed_to_the_tag_the_runner_set() {
        let tag = TurnTag::default();
        assert_eq!(tag.get(), "");
        tag.set("turn-2");
        assert_eq!(tag.get(), "turn-2");

        let observer = ChatObserver::conversation(
            Arc::new(tag),
            Arc::new(TurnLog::new()),
            Arc::new(AtomicUsize::new(0)),
        );
        assert!(observer.answers);
        assert_eq!(observer.source, "main");
        assert!(
            !ChatObserver::task(Arc::new(TurnTag::default()), Arc::new(TurnLog::new())).answers
        );
    }

    #[test]
    fn status_reports_real_workspace_facts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("skills").join("demo")).unwrap();
        fs::write(
            root.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: A demo skill.\n---\n\n# Demo\n",
        )
        .unwrap();
        fs::create_dir_all(root.join(".tasks")).unwrap();
        fs::write(
            root.join(".tasks").join("task_0123abcd.json"),
            json!({
                "id": "task_0123abcd",
                "subject": "Ship the shell",
                "status": "pending",
            })
            .to_string(),
        )
        .unwrap();

        let snapshot = bridge_at(root).snapshot();
        assert_eq!(snapshot["protocol_version"], json!(PROTOCOL_VERSION));
        assert_eq!(snapshot["pid"], json!(std::process::id()));
        assert_eq!(snapshot["skills"]["count"], json!(1));
        assert_eq!(snapshot["skills"]["names"][0], json!("demo"));
        assert_eq!(snapshot["tasks"]["total"], json!(1));
        assert_eq!(snapshot["tasks"]["pending"], json!(1));
        assert_eq!(snapshot["tasks"]["completed"], json!(0));
        assert_eq!(snapshot["memory"]["records"], json!(0));
        assert_eq!(snapshot["warnings"], json!([]));
        assert!(snapshot["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("runtime.shutdown")));
    }

    #[test]
    fn a_response_is_matchable_and_an_event_is_not() {
        let response = serde_json::to_value(Response::ok("r-1", json!({ "pong": true }))).unwrap();
        assert_eq!(response["type"], json!("response"));
        assert_eq!(response["request_id"], json!("r-1"));
        assert_eq!(response["ok"], json!(true));
        assert!(response.get("error").is_none());

        let failure = serde_json::to_value(Response::failed(
            "r-2",
            ProtocolError::new(UNKNOWN_METHOD, "nope"),
        ))
        .unwrap();
        assert_eq!(failure["ok"], json!(false));
        assert!(failure.get("result").is_none());
        assert_eq!(failure["error"]["code"], json!(UNKNOWN_METHOD));

        let outgoing = serde_json::to_value(event("runtime.ready", json!({}))).unwrap();
        assert_eq!(outgoing["type"], json!("event"));
        assert_eq!(outgoing["event"], json!("runtime.ready"));
        assert!(outgoing.get("request_id").is_none());
        assert!(outgoing.get("ok").is_none());
    }

    fn read_all(input: &str) -> Vec<Line> {
        let mut reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let mut lines = Vec::new();
        loop {
            let line = read_capped(&mut reader, 64).unwrap();
            if line == Line::Eof {
                break;
            }
            lines.push(line);
        }
        lines
    }

    #[test]
    fn framing_handles_crlf_blanks_and_a_missing_final_newline() {
        assert_eq!(
            read_all("{\"a\":1}\r\n\n{\"b\":2}"),
            vec![
                Line::Text("{\"a\":1}".to_string()),
                Line::Blank,
                Line::Text("{\"b\":2}".to_string()),
            ]
        );
        assert_eq!(read_all("   \n"), vec![Line::Blank]);
        assert_eq!(read_all(""), Vec::<Line>::new());
    }

    #[test]
    fn an_overlong_line_is_dropped_without_losing_the_next_one() {
        let long = "x".repeat(200);
        let input = format!("{long}\n{{\"a\":1}}\n");
        assert_eq!(
            read_all(&input),
            vec![Line::TooLong, Line::Text("{\"a\":1}".to_string())]
        );
    }

    #[test]
    fn invalid_utf8_is_reported_rather_than_replaced() {
        let mut reader = BufReader::new(Cursor::new(vec![0xff, 0xfe, b'\n']));
        assert_eq!(read_capped(&mut reader, 64).unwrap(), Line::InvalidUtf8);
    }

    #[test]
    fn a_relative_path_resolves_against_the_current_directory() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            resolve_path(Path::new("nested")).unwrap(),
            cwd.join("nested")
        );

        let absolute = cwd.join("elsewhere");
        assert_eq!(resolve_path(&absolute).unwrap(), absolute);
    }

    /// A request with parameters, taken through the reader loop's parser.
    fn request_with(id: &str, method: &str, params: Value) -> Request {
        let line = json!({
            "protocol_version": PROTOCOL_VERSION,
            "request_id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        match parse_request(&line) {
            Parsed::Request(request) => request,
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[test]
    fn a_stop_aimed_at_nothing_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        let turn = dispatch(
            &request_with("r-1", "chat.cancel", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&turn).ok);
        assert_eq!(answered(&turn).result, Some(json!({ "cancelled": false })));
    }

    #[test]
    fn a_stop_reaches_the_turn_that_armed_it() {
        let cancel = TurnCancel::default();
        let (mut armed, turn_over) = cancel.arm();
        let mut stop_over = cancel.stop().expect("a turn was running");
        // The turn learns about it, and reads the value a stop sets. The sender
        // is gone by then: a receiver that has an unseen version resolves
        // `changed` before it ever looks at whether the sender still exists,
        // which is what lets the stop arrive and the slot empty at once.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            assert!(armed.changed().await.is_ok());
            assert!(*armed.borrow_and_update());
        });

        // The slot is emptied by the stop itself: a second one finds nothing.
        assert!(cancel.stop().is_none());

        // The turn says how it ended on the channel the stop took with it. The
        // stop's answer waits on that, so a page told its stop took effect can
        // send its next prompt at once instead of hearing a turn is running.
        assert!(turn_over.send(true).is_ok());
        runtime.block_on(async {
            assert!(stop_over.changed().await.is_ok());
            assert!(*stop_over.borrow_and_update());
        });

        // A turn that is over takes its channels with it.
        let (_armed_again, _over_again) = cancel.arm();
        cancel.disarm();
        assert!(cancel.stop().is_none());
    }

    #[test]
    fn the_interaction_methods_are_advertised_and_take_parameters() {
        for method in ["chat.answer", "chat.approve"] {
            assert!(METHODS.contains(&method), "`{method}` is not advertised");
            assert!(takes_params(method), "`{method}` should take parameters");
        }
    }

    /// The event the turn reported for `turn`, if it reported one.
    fn reported(log: &TurnLog, event_name: &str) -> Option<Value> {
        log.snapshot()["turns"]
            .as_array()?
            .iter()
            .flat_map(|turn| turn["events"].as_array().cloned().unwrap_or_default())
            .find(|event| event["event"] == json!(event_name))
    }

    /// Wait for `event_name` to reach the log, or give up. The turn reports
    /// from another thread, so the test has to wait for it rather than assume
    /// the order it got there in.
    fn wait_for(log: &TurnLog, event_name: &str) -> Value {
        for _ in 0..2000 {
            if let Some(event) = reported(log, event_name) {
                return event;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("`{event_name}` was never reported");
    }

    #[test]
    fn an_answer_reaches_the_slot_that_was_parked() {
        let hub = InteractionHub::default();
        hub.attend(true);
        let id = hub.next_id(QUESTION_ID);
        assert_eq!(id, "ask-1");
        let waiting = hub.park(&id).expect("somebody is watching");
        assert!(hub.deliver(&id, Reply::Answer(Some("the second one".into()))));
        match hub.wait(waiting) {
            Some(Reply::Answer(answer)) => assert_eq!(answer.as_deref(), Some("the second one")),
            _ => panic!("expected the answer that was delivered"),
        }
        // The slot was given back, so the same answer cannot land twice.
        assert!(!hub.deliver(&id, Reply::Answer(Some("again".into()))));
    }

    #[test]
    fn an_answer_for_another_question_is_not_taken() {
        let hub = InteractionHub::default();
        hub.attend(true);
        let waiting = hub.park("ask-1").expect("somebody is watching");
        assert!(!hub.deliver("ask-2", Reply::Answer(Some("no".into()))));
        assert!(hub.deliver("ask-1", Reply::Answer(Some("yes".into()))));
        assert!(matches!(
            hub.wait(waiting),
            Some(Reply::Answer(Some(answer))) if answer == "yes"
        ));
    }

    #[test]
    fn a_turn_nobody_is_watching_waits_for_nothing() {
        let hub = InteractionHub::default();
        // Unattended is what a turn starts as, and what a scheduled turn stays:
        // a question put to an empty room would otherwise hang the one-turn
        // slot for ten minutes.
        assert!(hub.park("ask-1").is_none());

        hub.attend(true);
        let waiting = hub.park("ask-2").expect("somebody is watching");
        hub.withdraw();
        assert!(hub.wait(waiting).is_none());
    }

    #[test]
    fn an_approval_is_reported_and_answered_by_the_page() {
        let hub = Arc::new(InteractionHub::default());
        hub.attend(true);
        let tag = Arc::new(TurnTag::default());
        tag.set("turn-1");
        let log = Arc::new(TurnLog::new());
        log.begin("turn-1", "clean up the build directory");

        let approver = WindowApprover {
            hub: Arc::clone(&hub),
            tag: Arc::clone(&tag),
            log: Arc::clone(&log),
        };
        let asking = std::thread::spawn(move || {
            approver.approve(
                &ToolCall {
                    id: "call-1".to_string(),
                    name: "bash".to_string(),
                    input: json!({ "command": "rm -rf build" }),
                },
                "potentially destructive command",
            )
        });

        // The question reaches the host before the turn waits on it: a page
        // that answers the moment it sees one has something to answer into.
        let asked = wait_for(&log, "chat.permission");
        assert_eq!(asked["data"]["turn"], json!("turn-1"));
        assert_eq!(asked["data"]["name"], json!("bash"));
        assert_eq!(
            asked["data"]["reason"],
            json!("potentially destructive command")
        );
        assert!(asked["data"]["input"]
            .as_str()
            .expect("the call is shown as it was made")
            .contains("rm -rf build"));
        let id = asked["data"]["id"].as_str().expect("an id").to_string();
        assert!(id.starts_with("perm-"), "{id}");

        assert!(hub.deliver(&id, Reply::Verdict(true)));
        assert!(asking.join().expect("the turn did not panic"));

        let decided = reported(&log, "chat.permission_result").expect("a decision");
        assert_eq!(decided["data"]["id"], json!(id));
        assert_eq!(decided["data"]["allowed"], json!(true));
    }

    #[test]
    fn a_question_is_reported_and_the_answers_are_kept() {
        let hub = Arc::new(InteractionHub::default());
        hub.attend(true);
        let tag = Arc::new(TurnTag::default());
        tag.set("turn-1");
        let log = Arc::new(TurnLog::new());
        log.begin("turn-1", "which one?");

        let asker = WindowAsker {
            hub: Arc::clone(&hub),
            tag: Arc::clone(&tag),
            log: Arc::clone(&log),
        };
        let asking = std::thread::spawn(move || {
            asker.ask("which one?", &["first".to_string(), "second".to_string()])
        });

        let asked = wait_for(&log, "chat.question");
        assert_eq!(asked["data"]["question"], json!("which one?"));
        assert_eq!(asked["data"]["options"], json!(["first", "second"]));
        let id = asked["data"]["id"].as_str().expect("an id").to_string();

        assert!(hub.deliver(&id, Reply::Answer(Some("the second".to_string()))));
        assert_eq!(
            asking.join().expect("the turn did not panic").as_deref(),
            Some("the second")
        );

        let answered = reported(&log, "chat.question_result").expect("an answer");
        assert_eq!(answered["data"]["answer"], json!("the second"));
    }

    #[test]
    fn an_answer_without_a_question_is_refused_before_anything_is_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        for (case, params) in [
            ("no id", json!({ "answer": "hello" })),
            ("no answer", json!({ "id": "ask-1" })),
            ("id is not a string", json!({ "id": 7, "answer": "hello" })),
        ] {
            let turn = dispatch(
                &request_with("r-1", "chat.answer", params),
                &bridge,
                &chat,
                runtime.handle(),
            );
            assert!(!answered(&turn).ok, "`{case}` was accepted");
            assert_eq!(
                answered(&turn)
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some(INVALID_PARAMS),
                "`{case}`"
            );
        }
    }

    #[test]
    fn a_decision_that_reaches_no_call_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        let turn = dispatch(
            &request_with(
                "r-1",
                "chat.approve",
                json!({ "id": "perm-1", "allow": true }),
            ),
            &bridge,
            &chat,
            runtime.handle(),
        );
        // Nothing was parked on, so the decision was not taken -- which is an
        // answer, not a failure: the call it was aimed at is already over.
        assert!(answered(&turn).ok);
        assert_eq!(answered(&turn).result, Some(json!({ "accepted": false })));

        // A decision still needs to say what it is deciding.
        let turn = dispatch(
            &request_with("r-1", "chat.approve", json!({ "id": "perm-1" })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert_eq!(
            answered(&turn)
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some(INVALID_PARAMS)
        );
    }

    #[test]
    fn stopping_a_turn_lets_go_of_the_question_it_was_parked_on() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        chat.hub.attend(true);
        let waiting = chat.hub.park("ask-1").expect("somebody is watching");
        let held = Arc::clone(&chat);
        let answer = std::thread::spawn(move || held.hub.wait(waiting));
        // A stop is what a page presses when the runtime has gone quiet; the
        // wait the runtime is stuck in has to end there too, or the turn never
        // hears about it.
        let turn = dispatch(
            &request_with("r-1", "chat.cancel", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&turn).ok);
        assert!(answer.join().expect("the turn did not panic").is_none());
    }

    #[test]
    fn a_conversation_that_was_written_out_reads_back_whole() {
        let log = TurnLog::new();
        log.begin("turn-1", "first");
        log.record(&event(
            "chat.delta",
            json!({ "turn": "turn-1", "text": "hi" }),
        ));
        log.record(&event(
            "chat.done",
            json!({ "turn": "turn-1", "ok": true, "text": "hi" }),
        ));

        let view = log.snapshot();
        let read = TurnLog::from_snapshot(&view);
        assert_eq!(read.snapshot(), view);
        assert_eq!(read.last_number(), 1);

        // A turn the file caught mid-flight is reported as failed rather than
        // left spinning: whatever was going to finish it is gone.
        let mut interrupted = view.clone();
        interrupted["turns"][0]["status"] = json!("running");
        let turn = &TurnLog::from_snapshot(&interrupted).snapshot()["turns"][0];
        assert_eq!(turn["status"], json!("failed"));
        assert_eq!(turn["error"]["code"], json!("chat_interrupted"));
    }

    #[test]
    fn a_full_log_is_answered_in_one_frame() {
        // The host drops a line longer than this, and a dropped answer is a
        // conversation that reads back as nothing at all -- the page is left
        // waiting for a reply that never arrives. The log's own budget has to
        // stay inside it. (`MAX_LINE_BYTES` in `desktop/src-tauri/src/runtime.rs`.)
        const HOST_FRAME_BYTES: usize = 8 << 20;

        // Fill the log to its own budget: as many turns as it keeps, each with
        // events as large as a tool result may be echoed back, so both caps bind
        // and this is the heaviest answer the log can produce.
        let log = TurnLog::new();
        let text = "x".repeat(MAX_EVENT_TEXT_BYTES);
        let per_turn = MAX_LOG_BYTES / MAX_LOG_TURNS / MAX_EVENT_TEXT_BYTES + 1;
        for number in 1..=MAX_LOG_TURNS {
            let turn = format!("turn-{number}");
            log.begin(&turn, "a prompt");
            for _ in 0..per_turn {
                log.record(&event(
                    "chat.delta",
                    json!({ "turn": turn, "text": text.clone() }),
                ));
            }
            log.record(&event("chat.done", json!({ "turn": turn, "ok": true })));
        }

        let answer = serde_json::to_string(&log.snapshot()).unwrap();
        assert!(log.lock().bytes <= MAX_LOG_BYTES);
        assert!(log.lock().turns.len() <= MAX_LOG_TURNS);
        assert!(
            answer.len() < HOST_FRAME_BYTES,
            "a log at its budget answers with {} bytes, which the host drops",
            answer.len()
        );
    }

    #[test]
    fn a_conversation_is_named_after_its_first_line() {
        assert_eq!(session_title("first line\nsecond line"), "first line");
        assert_eq!(session_title("  \n  spaced  "), "spaced");

        let title = session_title(&"é".repeat(60));
        assert_eq!(title.chars().count(), 49);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn the_conversation_list_names_the_one_being_continued() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        let turn = dispatch(
            &request_with("r-1", "sessions.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&turn).ok);
        let result = answered(&turn).result.clone().unwrap();
        assert_eq!(result["current"], json!(chat.session().id));
        // The conversation in hand is listed even though it has no turn yet and
        // so no file: the page has to see the one a fresh start handed it, or
        // the list reads as though the workspace holds nothing at all.
        assert_eq!(result["sessions"][0]["id"], json!(chat.session().id));
        assert_eq!(result["sessions"][0]["messages"], json!(0));
    }

    #[test]
    fn another_conversation_can_be_started_and_opened_again() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());

        // A conversation this workspace already holds, written out the way a
        // previous run would have left it.
        let store = SessionStore::new(dir.path());
        let mut saved = store.create("an older conversation");
        saved.messages.push(Message::user_text("hello"));
        let log = TurnLog::new();
        log.begin("turn-1", "hello");
        log.record(&event(
            "chat.done",
            json!({ "turn": "turn-1", "ok": true, "text": "hi" }),
        ));
        saved.view = log.snapshot();
        assert!(store.save(&mut saved).unwrap().is_file());

        let started = dispatch(
            &request_with("r-1", "sessions.new", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&started).ok);
        assert_ne!(chat.session().id, saved.id);
        assert!(chat.session().messages.is_empty());

        let listed = dispatch(
            &request_with("r-2", "sessions.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let result = answered(&listed).result.clone().unwrap();
        // Both are listed: the new one, which is the one being continued and has
        // nothing written yet, and the one it was started beside.
        let entries = result["sessions"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["id"], json!(chat.session().id));
        let stored = entries
            .iter()
            .find(|session| session["id"] == json!(saved.id))
            .expect("the conversation that was left behind is still listed");
        assert_eq!(stored["title"], json!("an older conversation"));
        assert_eq!(stored["messages"], json!(1));

        // Opening it brings the messages and the turns back, and the next turn
        // continues the numbering instead of naming a turn that already exists.
        let opened = dispatch(
            &request_with("r-3", "sessions.open", json!({ "id": saved.id })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&opened).ok);
        assert_eq!(chat.session().id, saved.id);
        assert_eq!(chat.session().messages.len(), 1);
        assert_eq!(chat.log.snapshot(), log.snapshot());
        assert_eq!(chat.next_turn(), "turn-2");
    }

    #[test]
    fn opening_a_conversation_that_is_not_there_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        for (case, params, code) in [
            ("missing id", json!({}), INVALID_PARAMS),
            ("not a string", json!({ "id": 7 }), INVALID_PARAMS),
            ("not an id", json!({ "id": "../escape" }), INVALID_PARAMS),
            (
                "no such session",
                json!({ "id": "s1-2-3" }),
                SESSION_UNKNOWN,
            ),
        ] {
            let turn = dispatch(
                &request_with("r-1", "sessions.open", params),
                &bridge,
                &chat,
                runtime.handle(),
            );
            assert!(!answered(&turn).ok, "`{case}` was accepted");
            assert_eq!(
                answered(&turn)
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some(code),
                "`{case}`"
            );
        }
    }

    /// A conversation this workspace already holds, written out the way a
    /// previous run would have left it.
    fn stored_session(dir: &std::path::Path, title: &str, message: &str) -> Session {
        let store = SessionStore::new(dir);
        let mut session = store.create(title);
        session.messages.push(Message::user_text(message));
        store.save(&mut session).unwrap();
        session
    }

    #[test]
    fn a_conversation_can_be_renamed_in_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        let saved = stored_session(dir.path(), "an old name", "hello");

        let turn = dispatch(
            &request_with(
                "r-1",
                "sessions.rename",
                json!({ "id": saved.id, "title": "  a new name  " }),
            ),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&turn).ok);
        assert_eq!(
            answered(&turn).result.clone().unwrap()["title"],
            json!("a new name")
        );

        // The name is in the file, with the conversation still in it, and the
        // list the page reads shows the new name rather than the old one.
        let store = SessionStore::new(dir.path());
        let reloaded = store.load(&saved.id).unwrap();
        assert_eq!(reloaded.title, "a new name");
        assert_eq!(reloaded.messages.len(), 1);

        let listed = dispatch(
            &request_with("r-2", "sessions.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let result = answered(&listed).result.clone().unwrap();
        let entry = result["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["id"] == json!(saved.id))
            .expect("the renamed conversation is still listed");
        assert_eq!(entry["title"], json!("a new name"));
    }

    #[test]
    fn the_conversation_in_hand_can_be_named_without_a_file_being_made_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        let id = chat.session().id.clone();

        let turn = dispatch(
            &request_with(
                "r-1",
                "sessions.rename",
                json!({ "id": id, "title": "mine" }),
            ),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&turn).ok);
        assert_eq!(chat.session().title, "mine");

        // Naming a conversation is not a turn worth keeping, so it still has no
        // file -- and the name the page is told about is the new one.
        let store = SessionStore::new(dir.path());
        assert!(!store.dir().join(format!("{id}.json")).is_file());
        let listed = dispatch(
            &request_with("r-2", "sessions.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let result = answered(&listed).result.clone().unwrap();
        assert_eq!(result["sessions"][0]["id"], json!(id));
        assert_eq!(result["sessions"][0]["title"], json!("mine"));
    }

    #[test]
    fn a_rename_that_names_nothing_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        for (case, params, code) in [
            ("no id", json!({ "title": "x" }), INVALID_PARAMS),
            (
                "id is not a string",
                json!({ "id": 7, "title": "x" }),
                INVALID_PARAMS,
            ),
            (
                "id could name another file",
                json!({ "id": "../escape", "title": "x" }),
                INVALID_PARAMS,
            ),
            ("no title", json!({ "id": "s1-2-3" }), INVALID_PARAMS),
            (
                "title is not a string",
                json!({ "id": "s1-2-3", "title": 7 }),
                INVALID_PARAMS,
            ),
            (
                "a name that is too long",
                json!({ "id": "s1-2-3", "title": "e".repeat(MAX_SESSION_TITLE_CHARS + 1) }),
                INVALID_PARAMS,
            ),
            (
                "no such conversation",
                json!({ "id": "s1-2-3", "title": "x" }),
                SESSION_UNKNOWN,
            ),
        ] {
            let turn = dispatch(
                &request_with("r-1", "sessions.rename", params),
                &bridge,
                &chat,
                runtime.handle(),
            );
            assert!(!answered(&turn).ok, "`{case}` was accepted");
            assert_eq!(
                answered(&turn)
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some(code),
                "`{case}`"
            );
        }
    }

    #[test]
    fn deleting_a_conversation_removes_its_file_and_leaves_the_one_in_hand() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        let saved = stored_session(dir.path(), "older", "hello");
        let in_hand = chat.session().id.clone();

        let turn = dispatch(
            &request_with("r-1", "sessions.delete", json!({ "id": saved.id })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&turn).ok);
        let result = answered(&turn).result.clone().unwrap();
        assert_eq!(result["deleted"], json!(saved.id));
        assert_eq!(result["current"], json!(in_hand));
        assert_eq!(chat.session().id, in_hand);

        let store = SessionStore::new(dir.path());
        assert!(!store.dir().join(format!("{}.json", saved.id)).is_file());
    }

    #[test]
    fn deleting_the_conversation_in_hand_continues_the_newest_one_left() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        let first = stored_session(dir.path(), "first", "one");
        // Written second, so it is the newest and the one left behind.
        let second = stored_session(dir.path(), "second", "two");

        let opened = dispatch(
            &request_with("r-0", "sessions.open", json!({ "id": first.id })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&opened).ok);
        assert_eq!(chat.session().id, first.id);

        let turn = dispatch(
            &request_with("r-1", "sessions.delete", json!({ "id": first.id })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&turn).ok);
        let result = answered(&turn).result.clone().unwrap();
        assert_eq!(result["deleted"], json!(first.id));
        assert_eq!(result["current"], json!(second.id));
        assert_eq!(chat.session().id, second.id);
        assert_eq!(chat.session().title, "second");
        assert_eq!(chat.session().messages.len(), 1);

        let store = SessionStore::new(dir.path());
        assert!(!store.dir().join(format!("{}.json", first.id)).is_file());
        assert!(store.dir().join(format!("{}.json", second.id)).is_file());
    }

    #[test]
    fn deleting_the_last_conversation_leaves_somewhere_for_the_next_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        let only = stored_session(dir.path(), "only", "hello");

        let opened = dispatch(
            &request_with("r-0", "sessions.open", json!({ "id": only.id })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&opened).ok);

        let turn = dispatch(
            &request_with("r-1", "sessions.delete", json!({ "id": only.id })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&turn).ok);
        let result = answered(&turn).result.clone().unwrap();
        assert_eq!(result["deleted"], json!(only.id));
        // A fresh conversation, not nothing: the slot a prompt goes in is not
        // the store's to take away.
        assert_ne!(result["current"], json!(only.id));
        assert!(chat.session().messages.is_empty());
        assert!(SessionStore::new(dir.path()).list().unwrap().is_empty());
    }

    #[test]
    fn deleting_a_conversation_that_is_not_there_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        for (case, params, code) in [
            ("no id", json!({}), INVALID_PARAMS),
            ("id is not a string", json!({ "id": 7 }), INVALID_PARAMS),
            (
                "id could name another file",
                json!({ "id": "../escape" }),
                INVALID_PARAMS,
            ),
            (
                "no such conversation",
                json!({ "id": "s1-2-3" }),
                SESSION_UNKNOWN,
            ),
        ] {
            let turn = dispatch(
                &request_with("r-1", "sessions.delete", params),
                &bridge,
                &chat,
                runtime.handle(),
            );
            assert!(!answered(&turn).ok, "`{case}` was accepted");
            assert_eq!(
                answered(&turn)
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some(code),
                "`{case}`"
            );
        }
    }

    #[test]
    fn a_conversation_cannot_be_changed_while_a_turn_is_running() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        let id = chat.session().id.clone();
        // A turn in flight holds the slot until it is done; `Busy` gives it back
        // even if an assertion below fails.
        chat.busy.store(true, Ordering::SeqCst);
        let busy = Busy(&chat.busy);
        assert!(chat.busy.load(Ordering::SeqCst));

        for (method, params) in [
            ("sessions.new", json!({})),
            ("sessions.rename", json!({ "id": id, "title": "x" })),
            ("sessions.delete", json!({ "id": id })),
        ] {
            let turn = dispatch(
                &request_with("r-1", method, params),
                &bridge,
                &chat,
                runtime.handle(),
            );
            assert!(!answered(&turn).ok, "`{method}` was accepted mid-turn");
            assert_eq!(
                answered(&turn)
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some(CHAT_BUSY),
                "`{method}`"
            );
        }
        // Nothing was changed, because nothing was allowed to be.
        assert_eq!(chat.session().id, id);
        drop(busy);
    }

    #[test]
    fn a_cron_job_can_be_scheduled_listed_and_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());

        let created = dispatch(
            &request_with(
                "r-1",
                "cron.schedule",
                json!({
                    "cron": "0 9 * * 1-5",
                    "prompt": "weekday standup",
                    "recurring": true,
                    "durable": true,
                }),
            ),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&created).ok);
        let job = answered(&created).result.clone().unwrap()["job"].clone();
        assert!(is_valid_cron_id(job["id"].as_str().unwrap()));
        assert_eq!(job["cron"], json!("0 9 * * 1-5"));
        assert_eq!(job["prompt"], json!("weekday standup"));
        assert_eq!(job["recurring"], json!(true));
        assert_eq!(job["durable"], json!(true));
        assert_eq!(job["pending_delivery"], json!(false));
        // A recurring job always has somewhere to go next, and the answer says
        // both the wall-clock time and how far away it is.
        assert!(job["next_run"].is_string(), "{job}");
        assert!(job["next_run_ms"].as_u64().unwrap() > 0, "{job}");

        // It is stored in the runtime's own table, so `runtime.status` counts it
        // without being told twice.
        assert_eq!(bridge.snapshot()["cron"]["jobs"], json!(1));

        let listed = dispatch(
            &request_with("r-2", "cron.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let jobs = answered(&listed).result.clone().unwrap()["jobs"].clone();
        assert_eq!(jobs.as_array().unwrap().len(), 1);
        assert_eq!(jobs[0]["id"], job["id"]);

        let cancelled = dispatch(
            &request_with("r-3", "cron.cancel", json!({ "id": job["id"] })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&cancelled).ok);
        assert_eq!(
            answered(&cancelled).result.clone().unwrap()["cancelled"],
            job["id"]
        );

        let empty = dispatch(
            &request_with("r-4", "cron.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert_eq!(answered(&empty).result.clone().unwrap()["jobs"], json!([]));
    }

    #[test]
    fn a_cron_job_this_build_will_not_take_is_refused_with_a_code() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());

        for (case, method, params, code) in [
            (
                "no expression",
                "cron.schedule",
                json!({ "prompt": "x" }),
                INVALID_PARAMS,
            ),
            (
                "expression is not a string",
                "cron.schedule",
                json!({ "cron": 7, "prompt": "x" }),
                INVALID_PARAMS,
            ),
            (
                "expression does not parse",
                "cron.schedule",
                json!({ "cron": "every day at nine", "prompt": "x" }),
                CRON_INVALID,
            ),
            (
                "expression has the wrong field count",
                "cron.schedule",
                json!({ "cron": "0 9 * *", "prompt": "x" }),
                CRON_INVALID,
            ),
            (
                "no prompt",
                "cron.schedule",
                json!({ "cron": "0 9 * * *" }),
                INVALID_PARAMS,
            ),
            (
                "prompt is blank",
                "cron.schedule",
                json!({ "cron": "0 9 * * *", "prompt": "   " }),
                INVALID_PARAMS,
            ),
            (
                "a flag that is not a boolean",
                "cron.schedule",
                json!({ "cron": "0 9 * * *", "prompt": "x", "recurring": "no" }),
                INVALID_PARAMS,
            ),
            (
                "a durable flag that is not a boolean",
                "cron.schedule",
                json!({ "cron": "0 9 * * *", "prompt": "x", "durable": 1 }),
                INVALID_PARAMS,
            ),
            ("no id", "cron.cancel", json!({}), INVALID_PARAMS),
            (
                "id is not a job id",
                "cron.cancel",
                json!({ "id": "../cron_x" }),
                INVALID_PARAMS,
            ),
            (
                "no such job",
                "cron.cancel",
                json!({ "id": "cron_00000000" }),
                CRON_UNKNOWN,
            ),
        ] {
            let turn = dispatch(
                &request_with("r-1", method, params),
                &bridge,
                &chat,
                runtime.handle(),
            );
            assert!(!answered(&turn).ok, "`{case}` was accepted");
            assert_eq!(
                answered(&turn)
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some(code),
                "`{case}`"
            );
        }
        // A refusal stores nothing.
        assert!(chat.cron.jobs().is_empty());
    }

    #[test]
    fn a_due_job_names_the_turn_the_runtime_starts_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, _runtime) = fixture(dir.path());
        chat.cron
            .schedule("* * * * *", "run the tests", true, false)
            .unwrap();
        // A schedule no test run can land on, so only the first one is due.
        chat.cron
            .schedule("0 3 1 1 *", "new year", true, false)
            .unwrap();
        assert_eq!(due_prompt(&chat.cron.jobs()), "");

        chat.cron.poll(CronScheduler::now()).unwrap();
        assert_eq!(due_prompt(&chat.cron.jobs()), "[Scheduled] run the tests");

        // The same table the page reads: one job waiting, two in total.
        let snapshot = chat.cron_snapshot();
        assert_eq!(snapshot["pending_delivery"], json!(1));
        let jobs = snapshot["jobs"].as_array().unwrap();
        let due = jobs
            .iter()
            .find(|job| job["prompt"] == json!("run the tests"))
            .expect("the due job is in the list");
        assert_eq!(due["pending_delivery"], json!(true));
        // A job that has not come due has a next run and is not waiting.
        let later = jobs
            .iter()
            .find(|job| job["prompt"] == json!("new year"))
            .expect("the other job is in the list");
        assert_eq!(later["pending_delivery"], json!(false));
        assert!(later["next_run"].is_string());
        assert_eq!(bridge.snapshot()["cron"]["pending_delivery"], json!(1));
    }

    #[test]
    fn the_cron_methods_take_only_what_they_document() {
        for method in ["cron.list", "cron.schedule", "cron.cancel"] {
            assert!(METHODS.contains(&method), "{method} is not advertised");
        }
        assert!(!takes_params("cron.list"));
        assert!(takes_params("cron.schedule"));
        assert!(takes_params("cron.cancel"));

        // `cron.list` takes nothing, so a key on it is refused rather than
        // quietly ignored -- a host talking to a newer bridge finds out.
        let line = json!({
            "protocol_version": PROTOCOL_VERSION,
            "request_id": "r-1",
            "method": "cron.list",
            "params": { "id": "cron_00000000" },
        })
        .to_string();
        match parse_request(&line) {
            Parsed::Rejected { error, .. } => assert_eq!(error.code, INVALID_PARAMS),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_task_graph_can_be_created_claimed_released_and_completed() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());

        let created = dispatch(
            &request_with(
                "r-1",
                "tasks.create",
                json!({ "subject": "write the plan", "description": "the first node" }),
            ),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&created).ok, "{:?}", answered(&created).error);
        let first = answered(&created).result.clone().unwrap()["task"].clone();
        assert!(is_valid_task_id(first["id"].as_str().unwrap()));
        assert_eq!(first["status"], json!("pending"));
        assert_eq!(first["owner"], json!(null));
        assert_eq!(first["ready"], json!(true));
        assert_eq!(first["blocked"], json!([]));

        let dependent = dispatch(
            &request_with(
                "r-2",
                "tasks.create",
                json!({
                    "subject": "ship it",
                    "blocked_by": [first["id"].clone()],
                }),
            ),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&dependent).ok, "{:?}", answered(&dependent).error);
        let second = answered(&dependent).result.clone().unwrap()["task"].clone();
        let second_id = second["id"].clone();
        assert_eq!(second["blockedBy"], json!([first["id"].clone()]));
        assert_eq!(second["blocked"], json!([first["id"].clone()]));
        assert_eq!(second["ready"], json!(false));

        // A blocked node cannot be claimed, and the answer says what is open.
        let blocked = dispatch(
            &request_with("r-3", "tasks.claim", json!({ "id": second_id })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(!answered(&blocked).ok);
        assert_eq!(
            answered(&blocked).error.as_ref().map(|e| e.code.as_str()),
            Some(TASK_BLOCKED)
        );

        // The ready one can be, and it records the page's owner.
        let claimed = dispatch(
            &request_with("r-4", "tasks.claim", json!({ "id": first["id"] })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&claimed).ok, "{:?}", answered(&claimed).error);
        let claimed = answered(&claimed).result.clone().unwrap()["task"].clone();
        assert_eq!(claimed["status"], json!("in_progress"));
        assert_eq!(claimed["owner"], json!(PAGE_TASK_OWNER));
        assert_eq!(claimed["ready"], json!(false));

        // Giving it back clears the owner and makes it ready again.
        let released = dispatch(
            &request_with("r-5", "tasks.release", json!({ "id": first["id"] })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&released).ok, "{:?}", answered(&released).error);
        let released = answered(&released).result.clone().unwrap()["task"].clone();
        assert_eq!(released["status"], json!("pending"));
        assert_eq!(released["owner"], json!(null));
        assert_eq!(released["ready"], json!(true));

        // Claim and complete. The completion names the node it unblocked.
        dispatch(
            &request_with("r-6", "tasks.claim", json!({ "id": first["id"] })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let completed = dispatch(
            &request_with("r-7", "tasks.complete", json!({ "id": first["id"] })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&completed).ok, "{:?}", answered(&completed).error);
        let answer = answered(&completed).result.clone().unwrap();
        assert_eq!(answer["task"]["status"], json!("completed"));
        assert_eq!(answer["task"]["owner"], json!(PAGE_TASK_OWNER));
        assert!(
            answer["message"]
                .as_str()
                .unwrap()
                .contains("Unblocked: ship it"),
            "{answer}"
        );

        // The list is the graph after all of that, and the status counts it.
        let listed = dispatch(
            &request_with("r-8", "tasks.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let tasks = answered(&listed).result.clone().unwrap();
        assert_eq!(tasks["count"], json!(2));
        let second = tasks["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["id"] == second_id)
            .unwrap();
        assert_eq!(second["ready"], json!(true));
        assert_eq!(second["blocked"], json!([]));
        assert_eq!(bridge.snapshot()["tasks"]["completed"], json!(1));
        assert_eq!(bridge.snapshot()["tasks"]["pending"], json!(1));
    }

    #[test]
    fn a_task_action_this_build_will_not_take_is_refused_with_a_code() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        let store = TaskStore::new(dir.path());

        let mut done = store.create("already done", "").unwrap();
        done.status = TaskStatus::Completed;
        store.save(&done).unwrap();
        let pending = store.create("not started", "").unwrap();
        let mut owned = store.create("agent work", "").unwrap();
        owned.status = TaskStatus::InProgress;
        owned.owner = Some("agent".to_string());
        store.save(&owned).unwrap();

        for (case, method, params, code) in [
            ("no subject", "tasks.create", json!({}), INVALID_PARAMS),
            (
                "blank subject",
                "tasks.create",
                json!({ "subject": "   " }),
                TASK_INVALID,
            ),
            (
                "description is not a string",
                "tasks.create",
                json!({ "subject": "x", "description": 7 }),
                INVALID_PARAMS,
            ),
            (
                "dependencies are not an array",
                "tasks.create",
                json!({ "subject": "x", "blocked_by": "task_0123abcd" }),
                INVALID_PARAMS,
            ),
            (
                "a dependency is not a string",
                "tasks.create",
                json!({ "subject": "x", "blocked_by": [7] }),
                INVALID_PARAMS,
            ),
            (
                "a dependency is not a task id",
                "tasks.create",
                json!({ "subject": "x", "blocked_by": ["../escape"] }),
                TASK_INVALID,
            ),
            (
                "a dependency is missing",
                "tasks.create",
                json!({ "subject": "x", "blocked_by": ["task_00000000"] }),
                TASK_UNKNOWN,
            ),
            ("no id", "tasks.claim", json!({}), INVALID_PARAMS),
            (
                "id is not a task id",
                "tasks.claim",
                json!({ "id": "../escape" }),
                INVALID_PARAMS,
            ),
            (
                "no such task",
                "tasks.claim",
                json!({ "id": "task_00000000" }),
                TASK_UNKNOWN,
            ),
            (
                "claiming a completed task",
                "tasks.claim",
                json!({ "id": done.id }),
                TASK_NOT_READY,
            ),
            (
                "releasing a pending task",
                "tasks.release",
                json!({ "id": done.id }),
                TASK_NOT_READY,
            ),
            (
                "releasing somebody else's task",
                "tasks.release",
                json!({ "id": owned.id }),
                TASK_NOT_OWNED,
            ),
            (
                "completing somebody else's task",
                "tasks.complete",
                json!({ "id": owned.id }),
                TASK_NOT_OWNED,
            ),
            (
                "completing a pending task",
                "tasks.complete",
                json!({ "id": pending.id }),
                TASK_NOT_READY,
            ),
            (
                "completing an already completed task",
                "tasks.complete",
                json!({ "id": done.id }),
                TASK_NOT_READY,
            ),
        ] {
            let turn = dispatch(
                &request_with("r-1", method, params),
                &bridge,
                &chat,
                runtime.handle(),
            );
            assert!(!answered(&turn).ok, "`{case}` was accepted");
            assert_eq!(
                answered(&turn)
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some(code),
                "`{case}`"
            );
        }
    }

    #[test]
    fn the_task_methods_take_only_what_they_document() {
        for method in [
            "tasks.list",
            "tasks.create",
            "tasks.claim",
            "tasks.release",
            "tasks.complete",
        ] {
            assert!(METHODS.contains(&method), "{method} is not advertised");
        }
        assert!(!takes_params("tasks.list"));
        assert!(takes_params("tasks.create"));
        assert!(takes_params("tasks.claim"));
        assert!(takes_params("tasks.release"));
        assert!(takes_params("tasks.complete"));

        let line = json!({
            "protocol_version": PROTOCOL_VERSION,
            "request_id": "r-1",
            "method": "tasks.list",
            "params": { "id": "task_0123abcd" },
        })
        .to_string();
        match parse_request(&line) {
            Parsed::Rejected { error, .. } => assert_eq!(error.code, INVALID_PARAMS),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn departments_can_be_created_populated_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());

        let created = dispatch(
            &request_with(
                "r-1",
                "departments.create",
                json!({
                    "name": "研发部",
                    "description": "build the thing",
                    "instructions": "small changes",
                }),
            ),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let created = answered(&created);
        assert!(created.ok, "{created:?}");
        let department_id = created.result.as_ref().unwrap()["department"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let child = dispatch(
            &request_with(
                "r-2",
                "departments.create",
                json!({ "name": "前端组", "parent_id": department_id }),
            ),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let child = answered(&child);
        assert!(child.ok, "{child:?}");
        assert_eq!(
            child.result.as_ref().unwrap()["department"]["parentId"],
            json!(department_id)
        );

        let added = dispatch(
            &request_with(
                "r-3",
                "agents.add",
                json!({
                    "department_id": department_id,
                    "name": "后端",
                    "role": "owns the API",
                    "instructions": "run the tests",
                }),
            ),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let added = answered(&added);
        assert!(added.ok, "{added:?}");
        assert_eq!(
            added.result.as_ref().unwrap()["department"]["agents"][0]["name"],
            json!("后端")
        );

        let listed = dispatch(
            &request_with("r-4", "departments.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let listed = answered(&listed);
        assert!(listed.ok, "{listed:?}");
        assert_eq!(listed.result.as_ref().unwrap()["count"], json!(2));

        let removed = dispatch(
            &request_with("r-5", "departments.remove", json!({ "id": department_id })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let removed = answered(&removed);
        assert!(removed.ok, "{removed:?}");
        assert_eq!(
            removed.result.as_ref().unwrap()["removed"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        let listed = dispatch(
            &request_with("r-6", "departments.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert_eq!(
            answered(&listed).result.as_ref().unwrap()["count"],
            json!(0)
        );
    }

    #[test]
    fn the_department_methods_are_advertised_and_take_parameters() {
        for method in [
            "departments.list",
            "departments.create",
            "departments.remove",
            "agents.add",
            "agents.remove",
            "departments.run",
            "departments.stop",
        ] {
            assert!(METHODS.contains(&method), "{method} is not advertised");
        }
        assert!(!takes_params("departments.list"));
        assert!(takes_params("departments.create"));
        assert!(takes_params("departments.remove"));
        assert!(takes_params("agents.add"));
        assert!(takes_params("agents.remove"));
        assert!(takes_params("departments.run"));
        assert!(takes_params("departments.stop"));
    }

    #[test]
    fn a_skill_can_be_listed_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("skills").join("demo")).unwrap();
        fs::write(
            root.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: A demo skill.\n---\n\n# Demo\n\nSay hi.\n",
        )
        .unwrap();
        let (bridge, chat, runtime) = fixture(root);

        let listed = dispatch(
            &request_with("r-1", "skills.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let result = answered(&listed).result.clone().unwrap();
        assert_eq!(result["count"], json!(1));
        assert_eq!(
            result["dir"],
            json!(root.join("skills").display().to_string())
        );
        assert_eq!(result["skills"][0]["name"], json!("demo"));
        assert_eq!(result["skills"][0]["description"], json!("A demo skill."));
        assert!(result["skills"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("SKILL.md"));

        // Reading hands back the manifest the model would get from `load_skill`.
        let read = dispatch(
            &request_with("r-2", "skills.read", json!({ "name": "demo" })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&read).ok);
        let skill = answered(&read).result.clone().unwrap();
        assert_eq!(skill["name"], json!("demo"));
        assert!(skill["content"].as_str().unwrap().contains("Say hi."));
    }

    #[test]
    fn a_skill_this_workspace_does_not_hold_is_refused_with_a_code() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());

        for (case, params, code) in [
            ("no name", json!({}), INVALID_PARAMS),
            ("name is not a string", json!({ "name": 7 }), INVALID_PARAMS),
            ("name is blank", json!({ "name": "  " }), INVALID_PARAMS),
            ("no such skill", json!({ "name": "nope" }), SKILL_UNKNOWN),
        ] {
            let turn = dispatch(
                &request_with("r-1", "skills.read", params),
                &bridge,
                &chat,
                runtime.handle(),
            );
            assert!(!answered(&turn).ok, "`{case}` was accepted");
            assert_eq!(
                answered(&turn)
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some(code),
                "`{case}`"
            );
        }
    }

    #[test]
    fn a_plugin_can_be_connected_and_listed() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());
        // Nothing is connected, so this fixture's registry is the built-ins
        // only: no `plugins.json` in the workspace.
        assert_eq!(chat.mcp.available().len(), 2);

        let before = dispatch(
            &request_with("r-1", "plugins.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let listed = answered(&before).result.clone().unwrap();
        assert_eq!(listed["available"], json!(2));
        assert_eq!(listed["connected"], json!([]));
        let servers = listed["servers"].as_array().unwrap();
        assert!(servers.iter().any(|server| server["name"] == json!("docs")));
        // Nothing is connected, so nothing has a catalogue to show yet.
        assert!(servers.iter().all(|server| server["tools"] == json!([])));
        assert_eq!(bridge.snapshot()["plugins"]["connected"], json!(0));

        let connected = dispatch(
            &request_with("r-2", "plugins.connect", json!({ "server": "docs" })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&connected).ok);
        assert_eq!(
            answered(&connected).result.clone().unwrap()["server"],
            json!("docs")
        );

        // The same registry the snapshot reads, so the status counts the
        // connection without being told twice.
        assert_eq!(bridge.snapshot()["plugins"]["connected"], json!(1));
        assert!(chat.mcp.is_connected("docs"));

        let after = dispatch(
            &request_with("r-3", "plugins.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let listed = answered(&after).result.clone().unwrap();
        assert_eq!(listed["connected"], json!(["docs"]));
        let docs = listed["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|server| server["name"] == json!("docs"))
            .unwrap();
        assert_eq!(docs["connected"], json!(true));
        let tools = docs["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
        assert!(tools
            .iter()
            .all(|tool| tool["name"].as_str().unwrap().starts_with("mcp__docs__")));
        assert!(tools.iter().any(|tool| tool["policy"].as_str().is_some()));
    }

    #[test]
    fn a_skill_folder_can_be_imported_into_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());

        // A clone of a skills repository: one skill per child folder.
        let source = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta"] {
            let folder = source.path().join(name);
            std::fs::create_dir_all(folder.join("references")).unwrap();
            std::fs::write(
                folder.join(SKILL_MANIFEST),
                format!("---\nname: {name}\ndescription: The {name} skill.\n---\n\n# {name}\n"),
            )
            .unwrap();
            std::fs::write(folder.join("references").join("notes.md"), "notes\n").unwrap();
        }
        // A repository's history is not part of the skill.
        std::fs::create_dir_all(source.path().join(".git")).unwrap();
        std::fs::write(source.path().join(".git").join("HEAD"), "ref\n").unwrap();

        let imported = dispatch(
            &request_with(
                "r-1",
                "skills.import",
                json!({ "path": source.path().display().to_string() }),
            ),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&imported).ok, "{:?}", answered(&imported).error);
        let result = answered(&imported).result.clone().unwrap();
        assert_eq!(result["count"], json!(2));
        assert_eq!(
            result["dir"],
            json!(dir.path().join("skills").display().to_string())
        );
        // The whole folder came across, and the copy is a real skill.
        assert!(dir
            .path()
            .join("skills/alpha/references/notes.md")
            .is_file());
        assert!(!dir.path().join("skills/alpha/.git").exists());

        // It is the loader's skill now, so the catalogue and the model agree.
        let listed = dispatch(
            &request_with("r-2", "skills.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let listed = answered(&listed).result.clone().unwrap();
        assert_eq!(listed["count"], json!(2));
        assert!(listed["skills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|skill| skill["name"] == json!("alpha")));
        assert_eq!(bridge.snapshot()["skills"]["count"], json!(2));

        let read = dispatch(
            &request_with("r-3", "skills.read", json!({ "name": "beta" })),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&read).ok);
        assert!(answered(&read).result.clone().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("# beta"));
    }

    #[test]
    fn an_import_that_would_not_land_is_refused_with_a_code() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());

        // An empty folder, and one skill already on disk to collide with.
        let empty = tempfile::tempdir().unwrap();
        let single = tempfile::tempdir().unwrap();
        std::fs::write(
            single.path().join(SKILL_MANIFEST),
            "---\nname: demo\n---\n\n# Demo\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("skills").join("demo")).unwrap();
        std::fs::write(
            dir.path().join("skills").join("demo").join(SKILL_MANIFEST),
            "---\nname: demo\n---\n\n# Taken\n",
        )
        .unwrap();

        let cases = [
            ("no path", json!({}), INVALID_PARAMS),
            ("path is not a string", json!({ "path": 7 }), INVALID_PARAMS),
            ("path is blank", json!({ "path": "  " }), INVALID_PARAMS),
            (
                "no such folder",
                json!({ "path": dir.path().join("nope").display().to_string() }),
                SKILL_SOURCE,
            ),
            (
                "no manifest anywhere",
                json!({ "path": empty.path().display().to_string() }),
                SKILL_INVALID,
            ),
            (
                "the name is taken",
                json!({ "path": single.path().display().to_string() }),
                SKILL_EXISTS,
            ),
        ];
        for (case, params, code) in cases {
            let turn = dispatch(
                &request_with("r-1", "skills.import", params),
                &bridge,
                &chat,
                runtime.handle(),
            );
            assert!(!answered(&turn).ok, "`{case}` was accepted");
            assert_eq!(
                answered(&turn)
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some(code),
                "`{case}`"
            );
        }
        // Nothing was half-imported by the refusals above.
        assert_eq!(bridge.snapshot()["skills"]["count"], json!(1));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("skills/demo/SKILL.md")).unwrap(),
            "---\nname: demo\n---\n\n# Taken\n"
        );
    }

    #[test]
    fn the_context_reads_back_the_size_the_compactor_measures() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());

        let fresh = dispatch(
            &request_with("r-1", "chat.context", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        assert!(answered(&fresh).ok);
        let context = answered(&fresh).result.clone().unwrap();
        assert_eq!(context["messages"], json!(0));
        assert_eq!(context["compacted"], json!(0));
        assert!(context["session"].as_str().is_some_and(|id| !id.is_empty()));
        assert!(!takes_params("chat.context"));

        // Remember a conversation, then ask again: the size is the one the
        // compactor would measure, and it is no longer zero.
        chat.commit(Some(vec![Message::user_text("x".repeat(400).as_str())]));
        let after = dispatch(
            &request_with("r-2", "chat.context", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let context = answered(&after).result.clone().unwrap();
        assert_eq!(context["messages"], json!(1));
        assert!(context["chars"].as_u64().unwrap() >= 400);
    }

    #[test]
    fn a_workspace_can_declare_its_own_plugins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plugins.json"),
            r#"{
  "servers": [
    {
      "name": "notes",
      "command": "node",
      "args": ["tools/notes.mjs"],
      "policy": "allow"
    }
  ]
}"#,
        )
        .unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());

        // Declared is not connected: the list carries it, the pool does not.
        let listed = dispatch(
            &request_with("r-1", "plugins.list", json!({})),
            &bridge,
            &chat,
            runtime.handle(),
        );
        let listed = answered(&listed).result.clone().unwrap();
        assert_eq!(listed["available"], json!(3));
        let notes = listed["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|server| server["name"] == json!("notes"))
            .expect("the declared plugin is listed");
        assert_eq!(notes["source"], json!("workspace"));
        assert_eq!(notes["command"], json!("node tools/notes.mjs"));
        assert_eq!(notes["connected"], json!(false));
        assert_eq!(notes["tools"], json!([]));
        assert_eq!(bridge.snapshot()["plugins"]["available"], json!(3));
        assert!(chat.mcp.connected_names().is_empty());

        // The model is offered it too, through the tool that connects servers.
        assert!(chat.mcp.available().contains(&"notes".to_string()));

        // A plugins.json that cannot be read is a warning, not a dead runtime.
        std::fs::write(dir.path().join("plugins.json"), "{ not json").unwrap();
        let broken = bridge_at(dir.path());
        assert!(broken
            .warnings
            .iter()
            .any(|warning| warning.contains("plugins.json")));
        assert_eq!(broken.mcp.available().len(), 2);
    }

    #[test]
    fn a_plugin_this_build_will_not_take_is_refused_with_a_code() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, chat, runtime) = fixture(dir.path());

        for (case, params, code) in [
            ("no server", json!({}), INVALID_PARAMS),
            (
                "server is not a string",
                json!({ "server": 7 }),
                INVALID_PARAMS,
            ),
            ("server is blank", json!({ "server": " " }), PLUGIN_UNKNOWN),
            (
                "no such server",
                json!({ "server": "nope" }),
                PLUGIN_UNKNOWN,
            ),
        ] {
            let turn = dispatch(
                &request_with("r-1", "plugins.connect", params),
                &bridge,
                &chat,
                runtime.handle(),
            );
            assert!(!answered(&turn).ok, "`{case}` was accepted");
            assert_eq!(
                answered(&turn)
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some(code),
                "`{case}`"
            );
        }
        // A refusal connects nothing.
        assert!(chat.mcp.connected_names().is_empty());
    }

    #[test]
    fn the_skill_and_plugin_methods_take_only_what_they_document() {
        for method in [
            "skills.list",
            "skills.read",
            "skills.import",
            "plugins.list",
            "plugins.connect",
        ] {
            assert!(METHODS.contains(&method), "{method} is not advertised");
        }
        assert!(!takes_params("skills.list"));
        assert!(takes_params("skills.read"));
        assert!(takes_params("skills.import"));
        assert!(!takes_params("plugins.list"));
        assert!(takes_params("plugins.connect"));

        // Both list methods take nothing, so a key on one is refused rather than
        // quietly ignored -- a host talking to a newer bridge finds out.
        for method in ["skills.list", "plugins.list"] {
            let line = json!({
                "protocol_version": PROTOCOL_VERSION,
                "request_id": "r-1",
                "method": method,
                "params": { "name": "docs" },
            })
            .to_string();
            match parse_request(&line) {
                Parsed::Rejected { error, .. } => assert_eq!(error.code, INVALID_PARAMS),
                other => panic!("expected a refusal from {method}, got {other:?}"),
            }
        }
    }
}
