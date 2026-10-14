//! Long-running desktop bridge for the Tauri app.
//!
//! Speaks newline-delimited JSON over stdin/stdout: requests come in, responses
//! and asynchronous events go out. `stdout` carries protocol messages and
//! nothing else -- every diagnostic goes to `stderr` as one JSON object per
//! line, so a host can parse either stream without guessing.
//!
//! It answers three methods about itself -- `runtime.status`, `runtime.ping`,
//! and `runtime.shutdown` -- building the snapshot from the runtime's own stores
//! (`SkillLoader`, `TaskStore`, `CronScheduler`, `MemoryStore`), and two methods
//! that do work: `chat.send` runs a prompt through the same `Agent` loop the CLI
//! drives and reports the turn as `chat.*` events while it happens, and
//! `chat.history` hands those turns back, so a page that reloaded reads the
//! conversation it was only ever a view of instead of losing it.
//!
//! The agent is built on the first prompt, not at startup, so a workspace with
//! no model credentials still starts and still answers for itself.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use rust_agent::agent::{AgentEvent, AgentObserver};
use rust_agent::subagent::{subagent_system_prompt, TaskTool};
use rust_agent::{
    default_system_prompt, last_run, Agent, AnthropicClient, CompactConfig, Config, CronScheduler,
    Error, LlmClient, MemoryConfig, MemoryStore, Message, RecoveryConfig, Role, SkillLoader,
    TaskStatus, TaskStore, ToolRegistry, CRON_PROMPT, MAILBOXES_DIR, TASKS_PROMPT, WORKFLOWS_DIR,
};

/// Wire revision of the protocol. Bumped whenever a message shape changes.
const PROTOCOL_VERSION: u32 = 1;

/// Hard cap on one inbound line, so a runaway host cannot exhaust memory.
const MAX_LINE_BYTES: usize = 1 << 20;

/// Every method this build answers, in the order we advertise them.
const METHODS: [&str; 5] = [
    "runtime.status",
    "runtime.ping",
    "runtime.shutdown",
    "chat.send",
    "chat.history",
];

/// The one method that takes parameters. Everything else refuses them, so a
/// host that thinks it is talking to a newer bridge finds out at once.
fn takes_params(method: &str) -> bool {
    method == "chat.send"
}

/// Exit codes, so the host can tell a bad invocation from a broken pipe.
const EXIT_OK: i32 = 0;
const EXIT_IO: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_WORKSPACE: i32 = 3;
const USAGE: &str = "\
rust-agent desktop bridge

usage: desktop_bridge --workspace <dir>

  --workspace <dir>  runtime workspace; must exist (default: the cwd)
  --help             print this text on stderr
  --version          print the version on stderr

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

    // Only `chat.send` takes arguments. Refusing unknown keys on every other
    // method beats silently ignoring a host that thinks it is talking to a
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

#[derive(Debug, Clone, Serialize)]
struct EventMessage {
    protocol_version: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    event: String,
    ts: String,
    data: Value,
}

fn event(name: &str, data: Value) -> EventMessage {
    EventMessage {
        protocol_version: PROTOCOL_VERSION,
        kind: "event",
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
    started: Instant,
    started_at: String,
    cron: CronScheduler,
    model: Option<String>,
    config_error: Option<String>,
    warnings: Vec<String>,
}

impl Bridge {
    fn new(workspace: PathBuf) -> Self {
        let cron = CronScheduler::new(workspace.clone());
        let mut warnings = Vec::new();
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
            started: Instant::now(),
            started_at: Utc::now().to_rfc3339(),
            cron,
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

        json!({
            "protocol_version": PROTOCOL_VERSION,
            "bridge_version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "started_at": self.started_at,
            "uptime_ms": self.uptime_ms(),
            "workspace": self.workspace.display().to_string(),
            "methods": METHODS,
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
            "workflow": { "last_run": last_run(&self.workspace) },
            "paths": {
                "workspace": self.workspace.display().to_string(),
                "skills": skills.dir().display().to_string(),
                "tasks": tasks.root().display().to_string(),
                "memory": memories.dir().display().to_string(),
                "mailboxes": self.workspace.join(MAILBOXES_DIR).display().to_string(),
                "workflow": self.workspace.join(WORKFLOWS_DIR).display().to_string(),
            },
            "warnings": warnings,
        })
    }
}
/// One dispatched request: what to answer, and whether to stop afterwards.
struct Turn {
    response: Response,
    shutdown: bool,
}

fn dispatch(
    request: &Request,
    bridge: &Bridge,
    chat: &Arc<Chat>,
    handle: &tokio::runtime::Handle,
) -> Turn {
    match request.method.as_str() {
        "runtime.ping" => Turn {
            response: Response::ok(&request.request_id, bridge.ping()),
            shutdown: false,
        },
        "runtime.status" => Turn {
            response: Response::ok(&request.request_id, bridge.snapshot()),
            shutdown: false,
        },
        "runtime.shutdown" => Turn {
            response: Response::ok(&request.request_id, json!({ "stopping": true })),
            shutdown: true,
        },
        "chat.send" => accept_chat(request, chat, handle),
        "chat.history" => Turn {
            response: Response::ok(&request.request_id, chat.log.snapshot()),
            shutdown: false,
        },
        other => Turn {
            response: Response::failed(
                &request.request_id,
                ProtocolError::new(
                    UNKNOWN_METHOD,
                    format!(
                        "unknown method `{other}`; this build answers: {}",
                        METHODS.join(", ")
                    ),
                ),
            ),
            shutdown: false,
        },
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

/// Longest run of tool output echoed back to the host, in bytes. One `bash`
/// call can print megabytes, and the host drops a line it cannot frame.
const MAX_EVENT_TEXT_BYTES: usize = 8 * 1024;

/// How much of the conversation is kept for a page that asks for it again.
/// Oldest turns go first, and a turn that is still running is never dropped.
const MAX_LOG_TURNS: usize = 50;
const MAX_LOG_BYTES: usize = 4 * 1024 * 1024;

/// Codes this section adds to the protocol.
const CHAT_BUSY: &str = "chat_busy";
const CHAT_NOT_CONFIGURED: &str = "chat_not_configured";

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
    /// `running`, `done` or `failed`; the terminal events carry the detail.
    status: &'static str,
    error: Option<ProtocolError>,
    events: Vec<EventMessage>,
    weight: usize,
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
        let Some(index) = inner
            .turns
            .iter()
            .position(|logged| logged.turn == turn_id)
        else {
            return;
        };
        let turn = &mut inner.turns[index];
        match message.event.as_str() {
            "chat.done" => turn.status = "done",
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
        let turns: Vec<Value> = inner
            .turns
            .iter()
            .map(|turn| {
                json!({
                    "turn": turn.turn,
                    "prompt": turn.prompt,
                    "status": turn.status,
                    "error": turn.error,
                    "events": turn.events,
                })
            })
            .collect();
        json!({ "turns": turns })
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
}

impl ChatObserver {
    /// The observer for the conversation itself.
    fn conversation(tag: Arc<TurnTag>, log: Arc<TurnLog>) -> Self {
        ChatObserver {
            tag,
            log,
            source: "main",
            answers: true,
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
        }
    }
}

impl AgentObserver for ChatObserver {
    fn on_event(&self, signal: AgentEvent<'_>) {
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
        AgentEvent::Blocked { name, reason } => (
            "chat.blocked",
            json!({ "name": *name, "reason": *reason }),
        ),
        AgentEvent::Compacted { kind, .. } => {
            notice("compacted", format!("history compacted ({kind})"))
        }
        AgentEvent::MemoryRecalled { count } => {
            notice("memory_recalled", format!("recalled {count} memory record(s)"))
        }
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
        AgentEvent::ContinuationRequested { count } => {
            notice("continuation", format!("asked the model to continue ({count})"))
        }
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

/// The conversation the shell is having, and the one turn that may be running.
struct Chat {
    workspace: PathBuf,
    /// The same job table the snapshot reports, so a scheduled prompt fires in
    /// the next turn rather than never.
    cron: CronScheduler,
    /// Built on the first prompt and reused, so the conversation keeps its
    /// history. A build that failed is remembered with its reason: the answer
    /// will not change until the runtime is restarted.
    agent: Mutex<Option<Result<Arc<Agent>, ProtocolError>>>,
    /// The live conversation, locked for the whole of one turn.
    history: tokio::sync::Mutex<Vec<Message>>,
    /// Set while a turn runs: the protocol takes one prompt at a time.
    busy: AtomicBool,
    /// Monotonic turn counter, so a host can match events to its prompt.
    turns: AtomicU64,
    tag: Arc<TurnTag>,
    /// Every turn reported so far, for a page that asks for them again.
    log: Arc<TurnLog>,
}

impl Chat {
    fn new(workspace: PathBuf, cron: CronScheduler) -> Self {
        Chat {
            workspace,
            cron,
            agent: Mutex::new(None),
            history: tokio::sync::Mutex::new(Vec::new()),
            busy: AtomicBool::new(false),
            turns: AtomicU64::new(0),
            tag: Arc::new(TurnTag::default()),
            log: Arc::new(TurnLog::new()),
        }
    }

    /// The agent loop for this workspace, built once.
    fn agent(&self) -> Result<Arc<Agent>, ProtocolError> {
        let mut cached = self.agent.lock().unwrap_or_else(|error| error.into_inner());
        let built = cached.get_or_insert_with(|| {
            build_agent(
                &self.workspace,
                &self.cron,
                Arc::clone(&self.tag),
                Arc::clone(&self.log),
            )
        });
        built.clone()
    }

    /// The next turn id. Only ever called with the busy slot held.
    fn next_turn(&self) -> String {
        format!("turn-{}", self.turns.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// Report a turn that produced no answer.
    fn failed(&self, turn: &str, error: &ProtocolError) {
        self.log.emit(event(
            "chat.failed",
            json!({
                "turn": turn,
                "ok": false,
                "error": { "code": error.code, "message": error.message },
            }),
        ));
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

/// Build the loop the shell talks to.
///
/// The same agent the CLI builds, with two differences that come from there
/// being no terminal behind a prompt: the observer streams to the protocol
/// stream instead of printing, and the turn is non-interactive, so a call the
/// permission policy would have asked about is refused with a reason the
/// transcript can show rather than blocking on an approval nobody can give.
fn build_agent(
    workspace: &Path,
    cron: &CronScheduler,
    tag: Arc<TurnTag>,
    log: Arc<TurnLog>,
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
            .recovery(recovery)
            .interactive(false)
            .max_tokens(config.max_tokens)
            .system_prompt(format!(
                "{} {} {}",
                default_system_prompt(),
                TASKS_PROMPT,
                CRON_PROMPT
            ))
            .observer(ChatObserver::conversation(tag, log))
            .build(),
    ))
}

/// Accept one prompt, or explain why it cannot run.
fn accept_chat(request: &Request, chat: &Arc<Chat>, handle: &tokio::runtime::Handle) -> Turn {
    let rejected = |error| Turn {
        response: Response::failed(&request.request_id, error),
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

    let turn = chat.next_turn();
    chat.log.begin(&turn, prompt);
    let chat = Arc::clone(chat);
    let prompt = prompt.to_string();
    let running = turn.clone();
    handle.spawn(async move {
        let _busy = Busy(&chat.busy);
        chat.tag.set(&running);
        run_turn(&chat, &running, &prompt).await;
    });

    Turn {
        response: Response::ok(
            &request.request_id,
            json!({ "accepted": true, "turn": turn }),
        ),
        shutdown: false,
    }
}

/// Run one prompt and stream it back.
///
/// The history is locked for the whole turn: a second prompt is refused while
/// this one runs rather than queued behind it, and a turn that failed is rolled
/// back instead of leaving half an exchange for the model to trip over next time.
async fn run_turn(chat: &Chat, turn: &str, prompt: &str) {
    let agent = match chat.agent() {
        Ok(agent) => agent,
        Err(error) => {
            chat.failed(turn, &error);
            return;
        }
    };

    let mut history = chat.history.lock().await;
    let rollback = history.len();
    history.push(Message::user_text(agent.prepare_prompt(prompt)));

    match agent.run_turn(&mut history).await {
        Ok(()) => {
            let text = history
                .last()
                .filter(|message| message.role == Role::Assistant)
                .map(Message::text)
                .unwrap_or_default();
            drop(history);
            chat.log.emit(event(
                "chat.done",
                json!({ "turn": turn, "ok": true, "text": text }),
            ));
        }
        Err(error) => {
            history.truncate(rollback);
            drop(history);
            let error = turn_error(&error);
            chat.failed(turn, &error);
        }
    }
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
}

/// `Ok(None)` means an informational flag was handled and we should exit 0.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Options>, String> {
    let args: Vec<String> = args.collect();
    let mut workspace = None;
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
    Ok(Some(Options { workspace }))
}

/// Make `requested` absolute without touching the filesystem.
///
/// `canonicalize` would resolve more, but it hands Windows callers a `\\?\`
/// verbatim path, and `std::path::absolute` is newer than this crate's declared
/// minimum Rust version. A relative path is therefore resolved against the
/// current directory and an absolute one is kept as it is; `run` checks that the
/// result is a directory before the bridge starts.
fn resolve_workspace(requested: &Path) -> io::Result<PathBuf> {
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

    let workspace = match resolve_workspace(&requested) {
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
    let bridge = Bridge::new(workspace.clone());
    log(Level::Info, format!("workspace: {}", workspace.display()));

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());

    if let Err(error) = send(&event("runtime.ready", bridge.snapshot())) {
        log(Level::Error, format!("cannot write to stdout: {error}"));
        return EXIT_IO;
    }

    // The conversation outlives any one prompt, and a turn runs off this thread
    // so the reader loop keeps answering while a model call is in flight.
    let chat = Arc::new(Chat::new(workspace.clone(), bridge.cron.clone()));
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
                    response: Response::failed(&request_id, error),
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

        if let Err(error) = send(&turn.response) {
            log(Level::Error, format!("cannot write to stdout: {error}"));
            code = EXIT_IO;
            break;
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
        Bridge::new(dir.to_path_buf())
    }

    /// The pieces `dispatch` needs, for the tests that go through it.
    fn fixture(dir: &std::path::Path) -> (Bridge, Arc<Chat>, tokio::runtime::Runtime) {
        let chat = Arc::new(Chat::new(dir.to_path_buf(), CronScheduler::new(dir)));
        (bridge_at(dir), chat, tokio::runtime::Runtime::new().unwrap())
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
        let chat = Arc::new(Chat::new(dir.path().to_path_buf(), bridge.cron.clone()));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let turn = dispatch(&request, &bridge, &chat, runtime.handle());
        assert!(!turn.shutdown);
        assert_eq!(turn.response.request_id, "r-7");
        assert!(!turn.response.ok);
        assert_eq!(
            turn.response
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
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
        let chat = Arc::new(Chat::new(dir.path().to_path_buf(), bridge.cron.clone()));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let turn = dispatch(&ping, &bridge, &chat, runtime.handle());
        assert!(turn.response.ok);
        assert!(!turn.shutdown);
        assert_eq!(turn.response.result.unwrap()["pong"], json!(true));

        let Parsed::Request(shutdown) = parse_request(&request("r-2", "runtime.shutdown")) else {
            panic!("expected a request");
        };
        let turn = dispatch(&shutdown, &bridge, &chat, runtime.handle());
        assert!(turn.response.ok);
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
        assert_eq!(turns[turns.len() - 1]["turn"], json!(format!("turn-{}", MAX_LOG_TURNS + 4)));
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
        assert!(turn.response.ok);
        assert!(!turn.shutdown);

        let result = turn.response.result.unwrap();
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
            assert!(!turn.response.ok, "`{case}` was accepted");
            assert_eq!(
                turn.response.error.as_ref().map(|error| error.code.as_str()),
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
        assert_eq!(turn_error(&Error::MaxIterations(64)).code, "chat_step_limit");
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
        let text = format!("{}{}", "a".repeat(MAX_EVENT_TEXT_BYTES - 1), "é".repeat(100));
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

        let observer = ChatObserver::conversation(Arc::new(tag), Arc::new(TurnLog::new()));
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
    fn a_relative_workspace_resolves_against_the_current_directory() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            resolve_workspace(Path::new("nested")).unwrap(),
            cwd.join("nested")
        );

        let absolute = cwd.join("elsewhere");
        assert_eq!(resolve_workspace(&absolute).unwrap(), absolute);
    }
}
