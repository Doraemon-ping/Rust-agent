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
use crate::provider::ProviderStore;
use crate::state::{self, HostError, LogLevel, LogLine, Ping, State, Status, PROTOCOL_VERSION};

/// Event names the frontend subscribes to.
pub const EVENT_STATUS: &str = "runtime://status";
pub const EVENT_MESSAGE: &str = "runtime://message";
pub const EVENT_CHAT: &str = "runtime://chat";
pub const EVENT_LOG: &str = "runtime://log";

/// Longest single line the host will buffer from the runtime.
///
/// Larger than the cap the runtime reads with, because the runtime's one
/// answer that grows with the conversation is the history it keeps: up to 4 MiB
/// of turns (`MAX_LOG_BYTES` in `src/bin/desktop_bridge.rs`) plus the envelope
/// around them. A line this reader drops is a conversation the page can never
/// read back -- the request that asked for it is left unanswered and times out
/// -- so the two numbers are chosen together rather than independently.
const MAX_LINE_BYTES: usize = 8 << 20;

/// How long one request may take before the caller is told it failed.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the runtime may take to accept a prompt. It answers before the turn
/// runs, so this only covers reading the settings and building the agent.
const CHAT_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest prompt the host will send, in bytes.
const MAX_PROMPT_BYTES: usize = 64 * 1024;

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
    /// The environment the runtime is started with.
    provider: ProviderStore,
    inner: Arc<Mutex<Core>>,
}

impl RuntimeManager {
    pub fn new(app: AppHandle, projects: ProjectsStore, provider: ProviderStore) -> Self {
        RuntimeManager {
            app,
            projects,
            provider,
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
        // Conversations are kept in the app's own store, one directory per
        // workspace, so the project the runtime works in stays the project's.
        let sessions = paths::sessions_dir(&self.app, &workspace)?;
        // Read once per start, before the lock, so the environment a child gets
        // is one snapshot of the store rather than a value that moves underneath
        // it.
        let env = self.provider.env().await;

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
                .arg("--sessions-dir")
                .arg(&sessions)
                // The runtime's working directory is the workspace it was given,
                // so the system prompt it builds from the current directory, and
                // any relative path inside a tool, agree with the project the UI
                // shows as active.
                .current_dir(&workspace)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                // Insurance against an orphan if this process dies abruptly.
                .kill_on_drop(true);
            // What the user saved in Settings becomes this child's environment.
            // A variable the store has nothing for is not set at all, which
            // leaves the runtime's own default and any `.env` in the workspace
            // intact.
            for (key, value) in &env {
                command.env(key, value);
            }
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

    /// Hand one prompt to the runtime.
    ///
    /// The turn itself comes back as `chat.*` events on [`EVENT_CHAT`]; this
    /// returns as soon as the runtime has accepted it, so the page can show the
    /// prompt without waiting for an answer that may take minutes.
    pub async fn chat_send(&self, prompt: &str) -> Result<Value, HostError> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(HostError::new("empty_prompt", "a prompt cannot be empty"));
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(HostError::new(
                "prompt_too_long",
                format!("a prompt cannot be longer than {MAX_PROMPT_BYTES} bytes"),
            ));
        }
        self.request("chat.send", json!({ "prompt": prompt }), CHAT_TIMEOUT)
            .await
    }

    /// The turns the runtime has reported so far.
    ///
    /// A reloaded page asks for this instead of losing the conversation, which
    /// it was only ever a view of: reloading the window does not restart the
    /// runtime, so the turns are still there to be read.
    pub async fn chat_history(&self) -> Result<Value, HostError> {
        self.request("chat.history", json!({}), CHAT_TIMEOUT).await
    }

    /// Stop the turn that is running, if one is.
    ///
    /// The runtime answers whether it stopped anything, so a page that asks
    /// with nothing running is told so instead of being handed a guess.
    pub async fn chat_cancel(&self) -> Result<Value, HostError> {
        self.request("chat.cancel", json!({}), CHAT_TIMEOUT).await
    }

    /// Answer the question the runtime is waiting on.
    ///
    /// The id is one the runtime itself reported, and the answer is the user's
    /// words: nothing about which question is being answered is decided here.
    /// The runtime answers whether the answer reached the question, so a page
    /// that answered one that is already over is told so rather than left to
    /// believe it was heard.
    pub async fn chat_answer(&self, id: &str, answer: &str) -> Result<Value, HostError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(HostError::new(
                "empty_interaction",
                "an interaction id cannot be empty",
            ));
        }
        self.request(
            "chat.answer",
            json!({ "id": id, "answer": answer }),
            CHAT_TIMEOUT,
        )
        .await
    }

    /// Say what to do about the call the runtime is waiting on.
    pub async fn chat_approve(&self, id: &str, allow: bool) -> Result<Value, HostError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(HostError::new(
                "empty_interaction",
                "an interaction id cannot be empty",
            ));
        }
        self.request(
            "chat.approve",
            json!({ "id": id, "allow": allow }),
            CHAT_TIMEOUT,
        )
        .await
    }

    /// The conversations this workspace holds, and the one being continued.
    pub async fn chat_sessions(&self) -> Result<Value, HostError> {
        self.request("sessions.list", json!({}), CHAT_TIMEOUT).await
    }

    /// Start a fresh conversation in this workspace.
    ///
    /// The conversation in hand is not lost: the runtime writes every finished
    /// turn down as it happens.
    pub async fn chat_new_session(&self) -> Result<Value, HostError> {
        self.request("sessions.new", json!({}), CHAT_TIMEOUT).await
    }

    /// Continue a conversation this workspace already holds.
    pub async fn chat_open_session(&self, id: &str) -> Result<Value, HostError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(HostError::new(
                "empty_session",
                "a session id cannot be empty",
            ));
        }
        self.request("sessions.open", json!({ "id": id }), CHAT_TIMEOUT)
            .await
    }

    /// Give a conversation this workspace holds a name of its own.
    ///
    /// The page may name a conversation and the words it names it with; it still
    /// cannot name a path. An id that is not one this runtime listed is refused
    /// on the other side of the pipe.
    pub async fn chat_rename_session(&self, id: &str, title: &str) -> Result<Value, HostError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(HostError::new(
                "empty_session",
                "a session id cannot be empty",
            ));
        }
        self.request(
            "sessions.rename",
            json!({ "id": id, "title": title }),
            CHAT_TIMEOUT,
        )
        .await
    }

    /// Forget a conversation. The runtime says which one it is continuing now.
    pub async fn chat_delete_session(&self, id: &str) -> Result<Value, HostError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(HostError::new(
                "empty_session",
                "a session id cannot be empty",
            ));
        }
        self.request("sessions.delete", json!({ "id": id }), CHAT_TIMEOUT)
            .await
    }

    /// Every cron job this workspace holds, with when each one next runs.
    ///
    /// The jobs live in the runtime, not here: the page asks what the runtime
    /// has, and the same table is what the ticker fires and the status counts.
    pub async fn cron_list(&self) -> Result<Value, HostError> {
        self.request("cron.list", json!({}), REQUEST_TIMEOUT).await
    }

    /// Schedule a prompt on the runtime's own clock.
    ///
    /// The expression and the prompt are the page's to choose; the check here
    /// is only the cheap one -- the runtime validates the expression and owns
    /// the job it creates. The status is re-read afterwards so the counts every
    /// other surface shows agree with the schedule the page just changed.
    pub async fn cron_schedule(
        &self,
        cron: &str,
        prompt: &str,
        recurring: bool,
        durable: bool,
    ) -> Result<Value, HostError> {
        let cron = cron.trim();
        if cron.is_empty() {
            return Err(HostError::new(
                "empty_cron",
                "a cron expression cannot be empty",
            ));
        }
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(HostError::new("empty_prompt", "a prompt cannot be empty"));
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(HostError::new(
                "prompt_too_long",
                format!("a prompt cannot be longer than {MAX_PROMPT_BYTES} bytes"),
            ));
        }
        let job = self
            .request(
                "cron.schedule",
                json!({ "cron": cron, "prompt": prompt, "recurring": recurring, "durable": durable }),
                CHAT_TIMEOUT,
            )
            .await?;
        self.refresh_after_change().await;
        Ok(job)
    }

    /// Forget a cron job. The id is one the runtime handed out, not a path.
    pub async fn cron_cancel(&self, id: &str) -> Result<Value, HostError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(HostError::new("empty_job", "a cron job id cannot be empty"));
        }
        let cancelled = self
            .request("cron.cancel", json!({ "id": id }), CHAT_TIMEOUT)
            .await?;
        self.refresh_after_change().await;
        Ok(cancelled)
    }

    /// Every skill the runtime found in the workspace, with its manifest path.
    ///
    /// Read-only by nature: a skill is a file in `skills/`, so the page lists
    /// what the loader scanned and can read one back. Nothing here can change
    /// them, which is why none of these re-reads the status.
    pub async fn skills_list(&self) -> Result<Value, HostError> {
        self.request("skills.list", json!({}), REQUEST_TIMEOUT).await
    }

    /// One full `SKILL.md`, the same text `load_skill` hands the model.
    pub async fn skills_read(&self, name: &str) -> Result<Value, HostError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(HostError::new("empty_skill", "a skill name cannot be empty"));
        }
        self.request("skills.read", json!({ "name": name }), REQUEST_TIMEOUT)
            .await
    }

    /// Copy skills into the workspace from a folder the host picked.
    ///
    /// The path comes from the picker, never from the page: the page can ask
    /// for the dialog, but it cannot name what gets copied where. The skill
    /// count is part of the status, so the snapshot is re-read afterwards.
    pub async fn skills_import(&self, path: &str) -> Result<Value, HostError> {
        let path = path.trim();
        if path.is_empty() {
            return Err(HostError::new("empty_skill_source", "no folder was chosen"));
        }
        let imported = self
            .request("skills.import", json!({ "path": path }), CHAT_TIMEOUT)
            .await?;
        self.refresh_after_change().await;
        Ok(imported)
    }

    /// What the runtime is holding for the conversation in hand.
    ///
    /// Read-only, and cheap: the size it reports is the one its own compaction
    /// pipeline measures before every model call.
    pub async fn chat_context(&self) -> Result<Value, HostError> {
        self.request("chat.context", json!({}), REQUEST_TIMEOUT)
            .await
    }

    /// Every plugin this build ships, with the tools of the ones connected.
    pub async fn plugins_list(&self) -> Result<Value, HostError> {
        self.request("plugins.list", json!({}), REQUEST_TIMEOUT).await
    }

    /// Connect a plugin this build ships, discovering its tools.
    ///
    /// The page names a server, never a transport: an unknown name is refused on
    /// the other side with a code. The status is re-read afterwards so the
    /// counts the rest of the UI shows agree with what the page just connected.
    pub async fn plugins_connect(&self, server: &str) -> Result<Value, HostError> {
        let server = server.trim();
        if server.is_empty() {
            return Err(HostError::new(
                "empty_plugin",
                "a plugin name cannot be empty",
            ));
        }
        let connected = self
            .request("plugins.connect", json!({ "server": server }), CHAT_TIMEOUT)
            .await?;
        self.refresh_after_change().await;
        Ok(connected)
    }

    /// Every task on the workspace's board, with the state its edges imply.
    ///
    /// Read-only: the board lives under `<workspace>/.tasks` and is the same
    /// graph the model's task tools work from. Nothing is cached here.
    pub async fn tasks_list(&self) -> Result<Value, HostError> {
        self.request("tasks.list", json!({}), REQUEST_TIMEOUT).await
    }

    /// Add a node to the task board.
    ///
    /// The subject and description are the page's words; the id is the
    /// runtime's to make. Dependencies are ids the page read from the board,
    /// and the runtime checks each one before it writes anything.
    pub async fn tasks_create(
        &self,
        subject: &str,
        description: &str,
        blocked_by: &[String],
    ) -> Result<Value, HostError> {
        let subject = subject.trim();
        if subject.is_empty() {
            return Err(HostError::new(
                "empty_task",
                "a task subject cannot be empty",
            ));
        }
        let created = self
            .request(
                "tasks.create",
                json!({
                    "subject": subject,
                    "description": description.trim(),
                    "blocked_by": blocked_by,
                }),
                CHAT_TIMEOUT,
            )
            .await?;
        self.refresh_after_change().await;
        Ok(created)
    }

    /// Claim a ready task for the person at the page.
    pub async fn tasks_claim(&self, id: &str) -> Result<Value, HostError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(HostError::new("empty_task", "a task id cannot be empty"));
        }
        let claimed = self
            .request("tasks.claim", json!({ "id": id }), CHAT_TIMEOUT)
            .await?;
        self.refresh_after_change().await;
        Ok(claimed)
    }

    /// Give a task the page claimed back to the board.
    pub async fn tasks_release(&self, id: &str) -> Result<Value, HostError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(HostError::new("empty_task", "a task id cannot be empty"));
        }
        let released = self
            .request("tasks.release", json!({ "id": id }), CHAT_TIMEOUT)
            .await?;
        self.refresh_after_change().await;
        Ok(released)
    }

    /// Close a task the page claimed, reporting what that unblocked.
    pub async fn tasks_complete(&self, id: &str) -> Result<Value, HostError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(HostError::new("empty_task", "a task id cannot be empty"));
        }
        let completed = self
            .request("tasks.complete", json!({ "id": id }), CHAT_TIMEOUT)
            .await?;
        self.refresh_after_change().await;
        Ok(completed)
    }

    /// Every department, with its members, group chat, and task counts.
    pub async fn departments_list(&self) -> Result<Value, HostError> {
        self.request("departments.list", json!({}), REQUEST_TIMEOUT)
            .await
    }

    /// Create a department or a sub-department under `parent_id`.
    pub async fn departments_create(
        &self,
        name: &str,
        description: &str,
        instructions: &str,
        parent_id: Option<&str>,
    ) -> Result<Value, HostError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(HostError::new(
                "empty_department",
                "a department name cannot be empty",
            ));
        }
        let created = self
            .request(
                "departments.create",
                json!({
                    "name": name,
                    "description": description.trim(),
                    "instructions": instructions.trim(),
                    "parent_id": parent_id,
                }),
                CHAT_TIMEOUT,
            )
            .await?;
        self.refresh_after_change().await;
        Ok(created)
    }

    /// Remove a department and the sub-departments below it.
    pub async fn departments_remove(&self, id: &str) -> Result<Value, HostError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(HostError::new(
                "empty_department",
                "a department id cannot be empty",
            ));
        }
        let removed = self
            .request("departments.remove", json!({ "id": id }), CHAT_TIMEOUT)
            .await?;
        self.refresh_after_change().await;
        Ok(removed)
    }

    /// Add one agent to a department.
    pub async fn agents_add(
        &self,
        department_id: &str,
        name: &str,
        role: &str,
        instructions: &str,
    ) -> Result<Value, HostError> {
        let department_id = department_id.trim();
        let name = name.trim();
        if department_id.is_empty() {
            return Err(HostError::new(
                "empty_department",
                "a department id cannot be empty",
            ));
        }
        if name.is_empty() {
            return Err(HostError::new("empty_agent", "an agent name cannot be empty"));
        }
        let added = self
            .request(
                "agents.add",
                json!({
                    "department_id": department_id,
                    "name": name,
                    "role": role.trim(),
                    "instructions": instructions.trim(),
                }),
                CHAT_TIMEOUT,
            )
            .await?;
        self.refresh_after_change().await;
        Ok(added)
    }

    /// Remove one agent from a department.
    pub async fn agents_remove(
        &self,
        department_id: &str,
        agent_id: &str,
    ) -> Result<Value, HostError> {
        let department_id = department_id.trim();
        let agent_id = agent_id.trim();
        if department_id.is_empty() || agent_id.is_empty() {
            return Err(HostError::new(
                "empty_agent",
                "a department id and agent id are required",
            ));
        }
        let removed = self
            .request(
                "agents.remove",
                json!({ "department_id": department_id, "agent_id": agent_id }),
                CHAT_TIMEOUT,
            )
            .await?;
        self.refresh_after_change().await;
        Ok(removed)
    }

    /// Start one department shift. The answer arrives before the agents work;
    /// progress comes back as `department.*` events.
    pub async fn departments_run(
        &self,
        id: &str,
        instruction: &str,
    ) -> Result<Value, HostError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(HostError::new(
                "empty_department",
                "a department id cannot be empty",
            ));
        }
        let started = self
            .request(
                "departments.run",
                json!({ "id": id, "instruction": instruction.trim() }),
                CHAT_TIMEOUT,
            )
            .await?;
        self.refresh_after_change().await;
        Ok(started)
    }

    /// Ask the department's shift to stop after the current model call.
    pub async fn departments_stop(&self, id: &str) -> Result<Value, HostError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(HostError::new(
                "empty_department",
                "a department id cannot be empty",
            ));
        }
        let stopped = self
            .request("departments.stop", json!({ "id": id }), CHAT_TIMEOUT)
            .await?;
        self.refresh_after_change().await;
        Ok(stopped)
    }

    /// Re-read `runtime.status` and push it to the UI.
    ///
    /// The job table lives in the runtime, so a change the page made is re-read
    /// rather than patched into the shell's idea of the counts.
    async fn refresh_snapshot(&self) -> Result<(), HostError> {
        self.request("runtime.status", json!({}), REQUEST_TIMEOUT)
            .await?;
        self.emit_status().await;
        Ok(())
    }

    /// A change already happened; the snapshot is refreshed so the rest of the
    /// UI agrees, and a refresh that fails is a warning rather than a failure --
    /// the change itself is stored and valid.
    async fn refresh_after_change(&self) {
        if let Err(error) = self.refresh_snapshot().await {
            self.log(
                LogLevel::Warn,
                format!(
                    "the schedule changed, but the status could not be refreshed: {}",
                    error.message
                ),
            )
            .await;
        }
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

    /// Apply a change the runtime can only see in a new process.
    ///
    /// The workspace and the environment are both decided at spawn, so a live
    /// runtime is restarted to pick them up; a stopped one is left stopped,
    /// because choosing a project or saving a setting is not a request to start
    /// a runtime. Returns the new status when a restart happened.
    pub async fn apply_change(&self) -> Result<Option<Status>, HostError> {
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
        let mut chats: Vec<Value> = Vec::new();

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
                    // A turn's progress is not runtime lifecycle: it goes on its
                    // own channel so a streaming answer cannot push the status
                    // panel's short event ring out from under it.
                    let is_chat = event.starts_with("chat.");
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
                    let payload = json!({ "event": event, "ts": ts, "data": data });
                    if is_chat {
                        chats.push(payload);
                    } else {
                        messages.push(payload);
                    }
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
        for message in chats {
            let _ = self.app.emit(EVENT_CHAT, &message);
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
    fn the_longest_reply_the_runtime_keeps_still_frames() {
        // `chat.history` is answered with the runtime's whole log on one line,
        // and that log is bounded at 4 MiB of turns plus the envelope around
        // them. A limit that only covered what a request may carry would drop
        // the conversation instead of handing it back, so the frame this reader
        // takes has to reach past the runtime's own budget.
        let mut reply = vec![b'x'; (4 << 20) + (256 << 10)];
        reply.push(b'\n');
        let (kind, text) = frame(&reply, MAX_LINE_BYTES);
        assert!(matches!(kind, Frame::Line));
        assert_eq!(text.len(), (4 << 20) + (256 << 10));

        // The guard is still a guard: a line past it is dropped as before.
        let mut relentless = vec![b'x'; MAX_LINE_BYTES + 1];
        relentless.push(b'\n');
        let (kind, text) = frame(&relentless, MAX_LINE_BYTES);
        assert!(matches!(kind, Frame::Oversized));
        assert!(text.is_empty());
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
