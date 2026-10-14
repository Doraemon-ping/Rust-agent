//! Long-running desktop bridge for the Tauri app.
//!
//! Speaks newline-delimited JSON over stdin/stdout: requests come in, responses
//! and asynchronous events go out. `stdout` carries protocol messages and
//! nothing else -- every diagnostic goes to `stderr` as one JSON object per
//! line, so a host can parse either stream without guessing.
//!
//! Phase 1 answers three methods -- `runtime.status`, `runtime.ping`, and
//! `runtime.shutdown` -- and builds its snapshot from the runtime's own stores
//! (`SkillLoader`, `TaskStore`, `CronScheduler`, `MemoryStore`). It never
//! constructs an `Agent`, so it starts without model credentials and does not
//! duplicate the agent loop.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::Utc;
use serde::Serialize;
use serde_json::{json, Value};

use rust_agent::{
    last_run, Config, CronScheduler, MemoryStore, SkillLoader, TaskStatus, TaskStore,
    MAILBOXES_DIR, WORKFLOWS_DIR,
};

/// Wire revision of the protocol. Bumped whenever a message shape changes.
const PROTOCOL_VERSION: u32 = 1;

/// Hard cap on one inbound line, so a runaway host cannot exhaust memory.
const MAX_LINE_BYTES: usize = 1 << 20;

/// Every method this build answers, in the order we advertise them.
const METHODS: [&str; 3] = ["runtime.status", "runtime.ping", "runtime.shutdown"];

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

    // No phase-1 method takes arguments. Refusing unknown keys beats silently
    // ignoring a host that thinks it is talking to a newer bridge.
    if let Some(map) = params.as_object() {
        if !map.is_empty() {
            let keys: Vec<&str> = map.keys().map(String::as_str).collect();
            return reject(
                INVALID_PARAMS,
                format!("`{method}` takes no parameters; got {}", keys.join(", ")),
            );
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

#[derive(Debug, Serialize)]
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

        // The model is only reported, never used: phase 1 never calls out.
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
                format!("no model configured (phase 1 does not need one): {error}"),
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

fn dispatch(request: &Request, bridge: &Bridge) -> Turn {
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
    let stdout = io::stdout();
    let mut out = stdout.lock();

    if let Err(error) = write_message(&mut out, &event("runtime.ready", bridge.snapshot())) {
        log(Level::Error, format!("cannot write to stdout: {error}"));
        return EXIT_IO;
    }

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
                let _ = write_message(&mut out, &ErrorMessage::new(error));
                continue;
            }
            Line::InvalidUtf8 => {
                let error = ProtocolError::new(INVALID_UTF8, "message was not valid UTF-8");
                let _ = write_message(&mut out, &ErrorMessage::new(error));
                continue;
            }
            Line::Text(text) => match parse_request(&text) {
                Parsed::Request(request) => dispatch(&request, &bridge),
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
                    let _ = write_message(&mut out, &ErrorMessage::new(error));
                    continue;
                }
            },
        };

        if let Err(error) = write_message(&mut out, &turn.response) {
            log(Level::Error, format!("cannot write to stdout: {error}"));
            code = EXIT_IO;
            break;
        }

        if turn.shutdown {
            log(Level::Info, "shutdown requested; stopping");
            let _ = write_message(
                &mut out,
                &event("runtime.stopping", json!({ "reason": "requested" })),
            );
            break;
        }
    }

    log(Level::Info, format!("bridge stopped (exit {code})"));
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
        let turn = dispatch(&request, &bridge);
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
        let turn = dispatch(&ping, &bridge);
        assert!(turn.response.ok);
        assert!(!turn.shutdown);
        assert_eq!(turn.response.result.unwrap()["pong"], json!(true));

        let Parsed::Request(shutdown) = parse_request(&request("r-2", "runtime.shutdown")) else {
            panic!("expected a request");
        };
        let turn = dispatch(&shutdown, &bridge);
        assert!(turn.response.ok);
        assert!(turn.shutdown);
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
