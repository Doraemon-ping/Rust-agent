//! The runtime child process: spawn, frame, correlate, and stop.
//!
//! The host owns the child's lifetime. Requests go out as one JSON line on the
//! child's stdin; responses and events come back one JSON line at a time on
//! stdout; stderr is diagnostics and never parsed as protocol. A dedicated
//! writer task owns stdin so no request path can block while holding the lock,
//! and a watcher task reaps the child so every in-flight request fails with a
//! reason instead of hanging.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::paths;
use crate::projects::ProjectsStore;
use crate::protocol::{self, Inbound};
use crate::state::{self, HostError, LogLevel, LogLine, Ping, State, Status, PROTOCOL_VERSION};

/// Event names the frontend subscribes to.
pub const EVENT_STATUS: &str = "runtime://status";
pub const EVENT_MESSAGE: &str = "runtime://message";
pub const EVENT_LOG: &str = "runtime://log";

/// Longest single line the host will buffer from the runtime.
const MAX_LINE_BYTES: usize = 1 << 20;

/// How long one request may take before the caller is told it failed.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `runtime.ready` may take after the process starts.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a graceful `runtime.shutdown` may take.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Poll interval while waiting for the child or a state change.
const POLL: Duration = Duration::from_millis(50);

/// Log lines and events kept in memory for the UI.
const LOG_CAPACITY: usize = 500;

/// Cap on a single stored log line, so one runaway line cannot fill memory.
const MAX_LOG_CHARS: usize = 2000;

/// Which frame one read produced.
enum Frame {
    Line,
    /// Longer than the cap; the rest of the line was drained and dropped.
    Oversized,
    Eof,
}

/// One request waiting for its response.
struct Pending {
    method: String,
    reply: oneshot::Sender<Result<Value, HostError>>,
}

/// Mutable state, all behind one lock.
struct Core {
    child: Option<Child>,
    /// Lines to write to the child; closed to signal end of input.
    writer: Option<mpsc::UnboundedSender<String>>,
    status: Status,
    pending: HashMap<String, Pending>,
    logs: VecDeque<LogLine>,
    /// Monotonic request counter, so ids never repeat within a session.
    counter: u64,
    /// Bumped per spawn so stale reader tasks cannot touch a newer runtime.
    generation: u64,
    /// True while a stop the host asked for is in flight.
    stop_requested: bool,
}

/// The host's handle on the runtime process. Cheap to clone.
#[derive(Clone)]
pub struct RuntimeManager {
    app: AppHandle,
    /// Where the workspace comes from, and what the runtime runs in.
    projects: ProjectsStore,
    inner: Arc<Mutex<Core>>,
}

impl RuntimeManager {
    pub fn new(app: AppHandle, projects: ProjectsStore) -> Self {
        RuntimeManager {
            app,
            projects,
            inner: Arc::new(Mutex::new(Core {
                child: None,
                writer: None,
                status: Status::stopped(),
                pending: HashMap::new(),
                logs: VecDeque::new(),
                counter: 0,
                generation: 0,
                stop_requested: false,
            })),
        }
    }

    /// The current status. Safe to call at any time, including after a crash.
    pub async fn status(&self) -> Status {
        self.inner.lock().await.status.clone()
    }

    /// The most recent `limit` log lines, oldest first.
    pub async fn logs(&self, limit: usize) -> Vec<LogLine> {
        let core = self.inner.lock().await;
        let skip = core.logs.len().saturating_sub(limit);
        core.logs.iter().skip(skip).cloned().collect()
    }
}

impl RuntimeManager {
    /// Start the runtime if it is not already running.
    ///
    /// Idempotent: a live child wins, so double-clicking "start" cannot produce
    /// a second process. Failures are recorded in the status and returned.
    pub async fn start(&self) -> Result<Status, HostError> {
        match self.try_start().await {
            Ok(status) => Ok(status),
            Err(error) => {
                self.record_error(&error).await;
                Err(error)
            }
        }
    }

    async fn try_start(&self) -> Result<Status, HostError> {
        let bridge = paths::resolve_bridge()?;
        let workspace = self.workspace().await?;

        let (pid, generation) = {
            let mut core = self.inner.lock().await;

            if let Some(child) = core.child.as_mut() {
                match child.try_wait() {
                    // Still running: that is the only runtime there may be.
                    Ok(None) => return Ok(core.status.clone()),
                    // Already gone: fall through and replace it.
                    Ok(Some(_)) | Err(_) => {
                        core.child = None;
                        core.writer = None;
                    }
                }
            }

            let mut command = Command::new(&bridge);
            command
                .arg("--workspace")
                .arg(&workspace)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                // Insurance against an orphan if this process dies abruptly.
                .kill_on_drop(true);
            #[cfg(windows)]
            {
                // Do not flash a console window for a GUI-launched process.
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                command.creation_flags(CREATE_NO_WINDOW);
            }

            let mut child = command.spawn().map_err(|error| {
                HostError::new(
                    "runtime_spawn_failed",
                    format!("could not start {}: {error}", bridge.display()),
                )
            })?;
            let pid = child.id();
            let stdin = child.stdin.take();
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();

            core.generation += 1;
            let generation = core.generation;
            core.child = Some(child);
            core.stop_requested = false;

            let (writer, receiver) = mpsc::unbounded_channel::<String>();
            core.writer = Some(writer);
            core.status = Status {
                state: State::Starting,
                pid,
                bridge_path: Some(bridge.display().to_string()),
                protocol_version: Some(PROTOCOL_VERSION),
                started_at: Some(state::now()),
                changed_at: state::now(),
                last_error: None,
                last_ping: None,
                snapshot: None,
            };

            if let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) {
                spawn_writer(self.clone(), stdin, receiver);
                spawn_reader(self.clone(), stdout, generation, Stream::Stdout);
                spawn_reader(self.clone(), stderr, generation, Stream::Stderr);
            }
            spawn_watcher(self.clone(), generation);

            (pid, generation)
        };

        let message = match pid {
            Some(pid) => format!("runtime starting (pid {pid})"),
            None => "runtime starting".to_string(),
        };
        self.log(LogLevel::Info, message).await;
        self.emit_status().await;

        // Wait for the handshake so `start()` means "connected", not "spawned".
        if self.wait_for_ready(generation).await {
            return Ok(self.status().await);
        }

        let status = self.status().await;
        if status.state == State::Error {
            return Ok(status);
        }
        Err(HostError::new(
            "runtime_not_ready",
            format!(
                "the runtime started but did not report runtime.ready within {}s",
                READY_TIMEOUT.as_secs()
            ),
        ))
    }

    /// Wait for `runtime.ready`, an error, or the process to disappear.
    async fn wait_for_ready(&self, generation: u64) -> bool {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            {
                let core = self.inner.lock().await;
                if core.generation != generation {
                    return true;
                }
                match core.status.state {
                    State::Ready => return true,
                    State::Error => return false,
                    _ if core.child.is_none() => return false,
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

impl RuntimeManager {
    /// Round-trip a ping. Never fails because the runtime said no: a failure
    /// comes back as `ok: false` with the reason attached, so the UI can show
    /// it without tearing down a working connection.
    pub async fn ping(&self) -> Result<Ping, HostError> {
        let started = Instant::now();
        let outcome = self
            .request("runtime.ping", json!({}), REQUEST_TIMEOUT)
            .await;
        let latency_ms = started.elapsed().as_millis() as u64;

        let ping = match outcome {
            Ok(value) => Ping {
                ok: true,
                latency_ms,
                at: state::now(),
                uptime_ms: value.get("uptime_ms").and_then(Value::as_u64).unwrap_or(0),
                error: None,
            },
            Err(error) => Ping {
                ok: false,
                latency_ms,
                at: state::now(),
                uptime_ms: 0,
                error: Some(error),
            },
        };

        {
            let mut core = self.inner.lock().await;
            core.status.last_ping = Some(ping.clone());
        }
        self.emit_status().await;
        Ok(ping)
    }

    /// Stop the runtime and wait for the process to actually exit.
    pub async fn stop(&self) -> Result<Status, HostError> {
        {
            let mut core = self.inner.lock().await;
            if core.child.is_none() && core.status.state != State::Starting {
                let mut status = core.status.clone();
                status.set_state(State::Stopped);
                status.pid = None;
                status.snapshot = None;
                core.status = status.clone();
                drop(core);
                self.emit_status_value(status.clone()).await;
                return Ok(status);
            }
            core.stop_requested = true;
            let status = {
                core.status.set_state(State::Stopping);
                core.status.clone()
            };
            drop(core);
            self.emit_status_value(status).await;
        }

        // Ask the runtime to stop on its own first: that exercises its own
        // shutdown path instead of just killing the process.
        if let Err(error) = self
            .request("runtime.shutdown", json!({}), SHUTDOWN_TIMEOUT)
            .await
        {
            self.log(
                LogLevel::Warn,
                format!("graceful shutdown was not acknowledged: {}", error.message),
            )
            .await;
        }

        if !self.wait_for_exit(SHUTDOWN_TIMEOUT).await {
            self.log(
                LogLevel::Warn,
                "runtime did not exit after shutdown; terminating it",
            )
            .await;
            self.force_kill().await;
            self.wait_for_exit(SHUTDOWN_TIMEOUT).await;
        }

        Ok(self.status().await)
    }

    /// Stop and start again. What "reconnect" calls.
    pub async fn restart(&self) -> Result<Status, HostError> {
        self.stop().await?;
        self.start().await
    }

    /// The folder the runtime is started in: the active project when the user
    /// has chosen one, otherwise the per-user workspace.
    ///
    /// A chosen project that is no longer a directory is an error rather than a
    /// fallback: quietly running somewhere else would contradict the project
    /// the UI is showing as active.
    async fn workspace(&self) -> Result<PathBuf, HostError> {
        match self.projects.active_path().await {
            Some(path) if path.is_dir() => Ok(path),
            Some(path) => Err(HostError::new(
                "workspace_missing",
                format!("the active project folder is gone: {}", path.display()),
            )),
            None => paths::resolve_workspace(&self.app),
        }
    }

    /// Apply a change to the active project.
    ///
    /// A live runtime is restarted so it picks up the new workspace; a stopped
    /// one is left stopped, because choosing a project is not a request to
    /// start a runtime. Returns the new status when a restart happened.
    pub async fn apply_workspace_change(&self) -> Result<Option<Status>, HostError> {
        let live = matches!(self.status().await.state, State::Starting | State::Ready);
        if !live {
            return Ok(None);
        }
        Ok(Some(self.restart().await?))
    }

    /// Best-effort shutdown for application exit.
    pub async fn shutdown(&self) {
        let deadline = SHUTDOWN_TIMEOUT + Duration::from_secs(1);
        if tokio::time::timeout(deadline, self.stop()).await.is_err() {
            self.log(
                LogLevel::Warn,
                "shutdown timed out; terminating the runtime",
            )
            .await;
        }
        self.force_kill().await;
    }

    /// Send a request and wait for the response with the same id.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, HostError> {
        let (request_id, receiver) = {
            let mut core = self.inner.lock().await;
            let Some(writer) = core.writer.clone() else {
                return Err(HostError::new(
                    "runtime_not_running",
                    format!("`{method}` needs a running runtime"),
                ));
            };

            core.counter += 1;
            let request_id = format!("host-{}", core.counter);
            let line = protocol::encode_request(&request_id, method, params)?;
            let (reply, receiver) = oneshot::channel();
            core.pending.insert(
                request_id.clone(),
                Pending {
                    method: method.to_string(),
                    reply,
                },
            );

            // The writer task owns stdin, so this send cannot block on I/O.
            if writer.send(line).is_err() {
                core.pending.remove(&request_id);
                return Err(HostError::new(
                    "runtime_not_running",
                    "the runtime stopped before the request was sent",
                ));
            }
            (request_id, receiver)
        };

        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(result)) => result,
            // The watcher fails pending requests when the child exits.
            Ok(Err(_)) => Err(HostError::new(
                "runtime_not_running",
                format!("the runtime stopped while `{method}` was in flight"),
            )),
            Err(_) => {
                let mut core = self.inner.lock().await;
                core.pending.remove(&request_id);
                Err(HostError::new(
                    "runtime_timeout",
                    format!(
                        "`{method}` did not answer within {} ms",
                        timeout.as_millis()
                    ),
                ))
            }
        }
    }
}

impl RuntimeManager {
    /// Push one log line: keep it, then tell the UI.
    async fn log(&self, level: LogLevel, message: impl Into<String>) {
        let line = LogLine {
            ts: state::now(),
            level,
            message: truncate(message.into(), MAX_LOG_CHARS),
        };
        {
            let mut core = self.inner.lock().await;
            if core.logs.len() >= LOG_CAPACITY {
                core.logs.pop_front();
            }
            core.logs.push_back(line.clone());
        }
        let _ = self.app.emit(EVENT_LOG, &line);
    }

    /// Push alert a stored log line that came from the runtime's stderr.
    async fn log_line(&self, line: LogLine) {
        {
            let mut core = self.inner.lock().await;
            if core.logs.len() >= LOG_CAPACITY {
                core.logs.pop_front();
            }
            core.logs.push_back(line.clone());
        }
        let _ = self.app.emit(EVENT_LOG, &line);
    }

    async fn emit_status(&self) {
        let status = self.status().await;
        self.emit_status_value(status).await;
    }

    async fn emit_status_value(&self, status: Status) {
        let _ = self.app.emit(EVENT_STATUS, &status);
    }

    /// Record a failure the caller also receives.
    async fn record_error(&self, error: &HostError) {
        {
            let mut core = self.inner.lock().await;
            core.status.last_error = Some(error.clone());
            core.status.pid = None;
            core.status.snapshot = None;
            core.status.set_state(State::Error);
        }
        self.emit_status().await;
        self.log(LogLevel::Error, format!("{}: {}", error.code, error.message))
            .await;
    }

    /// Poll until the child is gone.
    async fn wait_for_exit(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let core = self.inner.lock().await;
                if core.child.is_none() {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Terminate the child without asking. The last resort on stop and exit.
    async fn force_kill(&self) {
        let mut core = self.inner.lock().await;
        // Dropping the writer closes stdin, which the runtime treats as "host
        // is gone" and exits on its own.
        core.writer = None;
        if let Some(child) = core.child.as_mut() {
            if let Err(error) = child.start_kill() {
                drop(core);
                self.log(
                    LogLevel::Warn,
                    format!("could not terminate the runtime: {error}"),
                )
                .await;
            }
        }
    }
}
/// Which child stream a reader task owns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Stdout,
    Stderr,
}

/// Owns stdin so no request path has to await a write while holding the lock.
fn spawn_writer(
    manager: RuntimeManager,
    mut stdin: ChildStdin,
    mut queue: mpsc::UnboundedReceiver<String>,
) {
    tokio::spawn(async move {
        while let Some(line) = queue.recv().await {
            let write = async {
                stdin.write_all(line.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await
            };
            if let Err(error) = write.await {
                manager
                    .log(
                        LogLevel::Error,
                        format!("could not write to the runtime: {error}"),
                    )
                    .await;
                break;
            }
        }
        // Dropping stdin closes the pipe. The runtime reads that as "host is
        // gone" and exits, even if the host never got to ask politely.
    });
}

/// Read one stream line by line until it ends or this generation is replaced.
fn spawn_reader<R>(manager: RuntimeManager, reader: R, generation: u64, kind: Stream)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            buffer.clear();
            match read_frame(&mut reader, &mut buffer, MAX_LINE_BYTES).await {
                Ok(Frame::Eof) => break,
                Ok(Frame::Oversized) => {
                    if kind == Stream::Stdout {
                        manager
                            .log(
                                LogLevel::Error,
                                format!(
                                    "the runtime sent a line longer than {MAX_LINE_BYTES} bytes; \
                                     it was discarded"
                                ),
                            )
                            .await;
                    }
                    continue;
                }
                Ok(Frame::Line) => {}
                Err(error) => {
                    manager
                        .log(
                            LogLevel::Error,
                            format!("could not read the runtime's output: {error}"),
                        )
                        .await;
                    break;
                }
            }

            if !manager.is_current(generation).await {
                break;
            }

            let Ok(text) = std::str::from_utf8(&buffer) else {
                manager
                    .log(
                        LogLevel::Error,
                        "the runtime sent a line that was not valid UTF-8",
                    )
                    .await;
                continue;
            };
            let text = text.trim_end_matches(['\r', '\n']);
            if text.trim().is_empty() {
                continue;
            }

            match kind {
                // stdout is the protocol and nothing else.
                Stream::Stdout => manager.handle_message(text).await,
                Stream::Stderr => manager.log_line(parse_stderr(text)).await,
            }
        }
    });
}

impl RuntimeManager {
    /// Whether this reader still belongs to the runtime we are tracking.
    async fn is_current(&self, generation: u64) -> bool {
        self.inner.lock().await.generation == generation
    }

    /// Fold one protocol message into the status, the pending map, and the UI.
    async fn handle_message(&self, line: &str) {
        let message = match protocol::parse_inbound(line) {
            Ok(message) => message,
            Err(error) => {
                self.log(LogLevel::Error, format!("{}: {}", error.code, error.message))
                    .await;
                return;
            }
        };

        let mut replies: Vec<(oneshot::Sender<Result<Value, HostError>>, Result<Value, HostError>)> =
            Vec::new();
        let mut statuses: Vec<Status> = Vec::new();
        let mut logs: Vec<LogLine> = Vec::new();
        let mut messages: Vec<Value> = Vec::new();

        {
            let mut core = self.inner.lock().await;
            if let Some(version) = message.protocol_version() {
                core.status.protocol_version = Some(version);
            }

            match message {
                Inbound::Response {
                    request_id,
                    ok,
                    result,
                    error,
                    ..
                } => match core.pending.remove(&request_id) {
                    None => logs.push(LogLine {
                        ts: state::now(),
                        level: LogLevel::Warn,
                        message: format!("the runtime answered unknown request `{request_id}`"),
                    }),
                    Some(pending) => {
                        if pending.method == "runtime.status" && ok {
                            if let Some(value) = result.as_ref() {
                                core.status.snapshot = Some(value.clone());
                            }
                        }
                        let outcome = if ok {
                            Ok(result.unwrap_or(Value::Null))
                        } else {
                            let error = error.unwrap_or(protocol::RemoteError {
                                code: "runtime_error".to_string(),
                                message: "the runtime reported a failure without details"
                                    .to_string(),
                            });
                            Err(HostError::new(&error.code, error.message))
                        };
                        replies.push((pending.reply, outcome));
                    }
                },
                Inbound::Event {
                    event, ts, data, ..
                } => {
                    let ts = ts.unwrap_or_else(state::now);
                    match event.as_str() {
                        "runtime.ready" => {
                            if let Some(pid) = data.get("pid").and_then(Value::as_u64) {
                                core.status.pid = Some(pid as u32);
                            }
                            if let Some(started) = data.get("started_at").and_then(Value::as_str) {
                                core.status.started_at = Some(started.to_string());
                            }
                            core.status.snapshot = Some(data.clone());
                            core.status.last_error = None;
                            core.status.set_state(State::Ready);
                            statuses.push(core.status.clone());
                        }
                        "runtime.stopping" => {
                            core.status.set_state(State::Stopping);
                            statuses.push(core.status.clone());
                        }
                        _ => {}
                    }
                    messages.push(json!({ "event": event, "ts": ts, "data": data }));
                }
                Inbound::Error { ts, error, .. } => {
                    logs.push(LogLine {
                        ts: ts.unwrap_or_else(state::now),
                        level: LogLevel::Error,
                        message: format!("{}: {}", error.code, error.message),
                    });
                    messages.push(json!({
                        "event": "runtime.protocol_error",
                        "ts": state::now(),
                        "data": { "code": error.code, "message": error.message },
                    }));
                }
            }
        }

        for (reply, outcome) in replies {
            let _ = reply.send(outcome);
        }
        for status in statuses {
            self.emit_status_value(status).await;
        }
        for line in logs {
            self.log_line(line).await;
        }
        for message in messages {
            let _ = self.app.emit(EVENT_MESSAGE, &message);
        }
    }
}

/// Watch the child so a crash fails every in-flight request and updates the UI.
fn spawn_watcher(manager: RuntimeManager, generation: u64) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL).await;

            let mut core = manager.inner.lock().await;
            if core.generation != generation {
                return;
            }
            let Some(child) = core.child.as_mut() else {
                return;
            };
            let exit = match child.try_wait() {
                Ok(Some(exit)) => exit,
                Ok(None) => {
                    drop(core);
                    continue;
                }
                Err(error) => {
                    drop(core);
                    manager
                        .log(
                            LogLevel::Warn,
                            format!("could not check the runtime process: {error}"),
                        )
                        .await;
                    continue;
                }
            };

            core.child = None;
            core.writer = None;
            let requested = core.stop_requested;
            core.stop_requested = false;

            let replies: Vec<_> = core
                .pending
                .drain()
                .map(|(_, pending)| pending.reply)
                .collect();
            let reason = describe_exit(&exit);
            let failure = HostError::new(
                "runtime_exited",
                format!("the runtime exited before answering ({reason})"),
            );

            if requested {
                core.status.set_state(State::Stopped);
            } else {
                core.status.last_error = Some(HostError::new(
                    "runtime_exited",
                    format!("the runtime exited unexpectedly ({reason})"),
                ));
                core.status.set_state(State::Error);
            }
            core.status.pid = None;
            core.status.snapshot = None;
            let status = core.status.clone();
            drop(core);

            for reply in replies {
                let _ = reply.send(Err(failure.clone()));
            }
            manager.emit_status_value(status).await;
            manager
                .log(
                    LogLevel::Info,
                    format!("runtime exited ({reason})"),
                )
                .await;
            return;
        }
    });
}

fn describe_exit(exit: &ExitStatus) -> String {
    match exit.code() {
        Some(code) => format!("exit code {code}"),
        None => "terminated by signal".to_string(),
    }
}

/// Read one line, never buffering more than `max` bytes.
async fn read_frame<R>(reader: &mut R, out: &mut Vec<u8>, max: usize) -> std::io::Result<Frame>
where
    R: AsyncBufRead + Unpin,
{
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if oversized {
                return Ok(Frame::Oversized);
            }
            if out.is_empty() {
                return Ok(Frame::Eof);
            }
            return Ok(Frame::Line);
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let used = newline.map_or(available.len(), |position| position + 1);
        let payload = &available[..used - usize::from(newline.is_some())];
        if !oversized {
            if out.len() + payload.len() > max {
                oversized = true;
                out.clear();
            } else {
                out.extend_from_slice(payload);
            }
        }
        reader.consume(used);

        if newline.is_some() {
            return Ok(if oversized {
                Frame::Oversized
            } else {
                Frame::Line
            });
        }
    }
}

/// Turn one stderr line into a log line.
///
/// The bridge writes one JSON object per stderr line; anything else is kept
/// verbatim as `raw` so a crash message or a panic trace is still visible.
fn parse_stderr(line: &str) -> LogLine {
    #[derive(Deserialize)]
    struct BridgeLog {
        ts: Option<String>,
        level: Option<String>,
        message: Option<String>,
    }

    if let Ok(parsed) = serde_json::from_str::<BridgeLog>(line.trim()) {
        if let Some(message) = parsed.message {
            let level = match parsed.level.as_deref() {
                Some("error") => LogLevel::Error,
                Some("warn") => LogLevel::Warn,
                Some("info") => LogLevel::Info,
                _ => LogLevel::Raw,
            };
            return LogLine {
                ts: parsed.ts.unwrap_or_else(state::now),
                level,
                message: truncate(redact(message), MAX_LOG_CHARS),
            };
        }
    }

    LogLine {
        ts: state::now(),
        level: LogLevel::Raw,
        message: truncate(redact(line.trim_end().to_string()), MAX_LOG_CHARS),
    }
}

/// Cut on a character boundary, marking the cut.
fn truncate(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}... (truncated)")
}

/// Replace anything that looks like a credential.
///
/// Nothing in the runtime's log path prints a key today. This is a second line
/// of defence for the log file the user might share.
fn redact(text: String) -> String {
    const MARKERS: [&str; 2] = ["sk-", "Bearer "];

    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        let rest = &text[index..];
        match MARKERS.iter().find(|marker| rest.starts_with(**marker)) {
            Some(marker) => {
                out.push_str(marker);
                index += marker.len();
                let start = index;
                while index < bytes.len() && is_token_byte(bytes[index]) {
                    index += 1;
                }
                if index - start >= 8 {
                    out.push_str("***");
                } else {
                    out.push_str(&text[start..index]);
                }
            }
            None => {
                let character = rest
                    .chars()
                    .next()
                    .expect("index is always on a character boundary");
                out.push(character);
                index += character.len_utf8();
            }
        }
    }
    out
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader as TokioBufReader;

    fn frame(input: &[u8], max: usize) -> (Frame, String) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut reader = TokioBufReader::new(std::io::Cursor::new(input.to_vec()));
            let mut out = Vec::new();
            let frame = read_frame(&mut reader, &mut out, max).await.unwrap();
            let text = String::from_utf8_lossy(&out).to_string();
            match frame {
                Frame::Line => (Frame::Line, text),
                Frame::Oversized => (Frame::Oversized, text),
                Frame::Eof => (Frame::Eof, text),
            }
        })
    }

    #[test]
    fn framing_strips_the_newline_and_tolerates_crlf() {
        let (kind, text) = frame(b"{\"a\":1}\r\n", 64);
        assert!(matches!(kind, Frame::Line));
        assert_eq!(text, "{\"a\":1}\r");

        let (kind, text) = frame(b"{\"a\":1}\n", 64);
        assert!(matches!(kind, Frame::Line));
        assert_eq!(text, "{\"a\":1}");
    }

    #[test]
    fn framing_reports_eof_only_at_the_end() {
        let (kind, _) = frame(b"", 64);
        assert!(matches!(kind, Frame::Eof));
        let (kind, text) = frame(b"no trailing newline", 64);
        assert!(matches!(kind, Frame::Line));
        assert_eq!(text, "no trailing newline");
    }

    #[test]
    fn an_overlong_line_is_dropped_and_the_stream_continues() {
        let mut input = vec![b'x'; 200];
        input.push(b'\n');
        input.extend_from_slice(b"{\"a\":1}\n");
        let (kind, text) = frame(&input, 64);
        assert!(matches!(kind, Frame::Oversized));
        assert!(text.is_empty());

        let (kind, text) = frame(&input[201..], 64);
        assert!(matches!(kind, Frame::Line));
        assert_eq!(text, "{\"a\":1}");
    }

    #[test]
    fn a_structured_stderr_line_keeps_its_level() {
        let line = parse_stderr(
            r#"{"ts":"2026-01-01T00:00:00Z","level":"warn","target":"desktop_bridge","message":"cron: restored 2"}"#,
        );
        assert_eq!(line.level, LogLevel::Warn);
        assert_eq!(line.message, "cron: restored 2");
        assert_eq!(line.ts, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn an_unstructured_stderr_line_is_kept_as_raw_text() {
        let line = parse_stderr("thread 'main' panicked at src/main.rs:1");
        assert_eq!(line.level, LogLevel::Raw);
        assert!(line.message.contains("panicked"));
    }

    #[test]
    fn credentials_are_scrubbed_before_they_reach_a_log_file() {
        assert_eq!(
            redact("using sk-abcdef1234567890 now".to_string()),
            "using sk-*** now"
        );
        assert_eq!(
            redact("Authorization: Bearer abcdefghijklmnop".to_string()),
            "Authorization: Bearer ***"
        );
        // Short runs are left alone so ordinary prose survives.
        assert_eq!(redact("sk-short".to_string()), "sk-short");
        assert_eq!(redact("no secrets here".to_string()), "no secrets here");
        // Multi-byte text must not be cut mid-character.
        assert_eq!(redact("模型 sk-abcdef1234567890".to_string()), "模型 sk-***");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("abc".to_string(), 5), "abc");
        let long = truncate("abcdefgh".to_string(), 4);
        assert!(long.starts_with("abcd"));
        assert!(long.ends_with("(truncated)"));
    }

    #[test]
    fn a_missing_request_id_never_matches_a_pending_request() {
        let core_response =
            r#"{"protocol_version":1,"type":"response","request_id":"host-1","ok":true,"result":{}}"#;
        let parsed = protocol::parse_inbound(core_response).unwrap();
        assert_eq!(parsed.protocol_version(), Some(PROTOCOL_VERSION));
        assert!(matches!(parsed, protocol::Inbound::Response { .. }));
    }
}
