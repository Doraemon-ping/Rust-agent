//! Background tasks (s11): slow Bash commands leave the agent loop.
//!
//! A call to `bash` with `run_in_background: true` is *not* executed inline.
//! The loop answers it immediately with a placeholder `tool_result`, a worker
//! thread runs the command, and the finished result is injected into the
//! conversation on a later turn as a `<task_notification>` block.
//!
//! The notification deliberately does not reuse the original `tool_use_id`:
//! that call was already answered by the placeholder, and one `tool_use` must
//! map to exactly one `tool_result`.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::tools::bash::run_shell;

/// Wrapper element used when a finished result is fed back to the model.
pub const NOTIFICATION_TAG: &str = "task_notification";

/// System-prompt guidance for the background parameter.
pub const BACKGROUND_PROMPT: &str =
    "Set run_in_background to true only for independent Bash commands.";

/// Characters of the result kept in a notification summary.
const SUMMARY_LIMIT: usize = 500;

/// Whether this tool call asked to run in the background (s11).
pub fn should_run_background(tool_name: &str, input: &Value) -> bool {
    tool_name == "bash" && input.get("run_in_background").and_then(Value::as_bool) == Some(true)
}

/// The result block returned while a background task is still running.
pub fn placeholder(task_id: &str) -> String {
    format!("[Background task {task_id} started] Its result arrives on a later turn.")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Completed,
    Failed,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Completed => "completed",
            Status::Failed => "failed",
        }
    }
}

struct Task {
    command: String,
    status: Status,
    result: String,
}

#[derive(Default)]
struct State {
    tasks: BTreeMap<String, Task>,
    ready: VecDeque<String>,
}

struct Shared {
    state: Mutex<State>,
    counter: AtomicU64,
}

/// Owns background task state and the completion queue. Cheap to clone.
#[derive(Clone)]
pub struct BackgroundManager {
    shared: Arc<Shared>,
    timeout: Duration,
    max_output: usize,
}

impl Default for BackgroundManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundManager {
    /// A manager with the standard timeout and output cap.
    pub fn new() -> Self {
        BackgroundManager {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                counter: AtomicU64::new(0),
            }),
            timeout: Duration::from_secs(120),
            max_output: 50_000,
        }
    }

    /// Override the per-command timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the per-command output cap.
    pub fn with_max_output(mut self, max_output: usize) -> Self {
        self.max_output = max_output;
        self
    }

    /// Start a command on its own thread and return its `bg_NNNN` id.
    pub fn start(&self, workspace: &Path, command: &str) -> Result<String> {
        if command.trim().is_empty() {
            return Err(Error::Tool {
                tool: "bash".into(),
                message: "background command cannot be empty".into(),
            });
        }

        let id = format!(
            "bg_{:04}",
            self.shared.counter.fetch_add(1, Ordering::SeqCst) + 1
        );
        {
            let mut state = self.lock();
            state.tasks.insert(
                id.clone(),
                Task {
                    command: command.to_string(),
                    status: Status::Running,
                    result: String::new(),
                },
            );
        }

        let shared = Arc::clone(&self.shared);
        let command = command.to_string();
        let workspace: PathBuf = workspace.to_path_buf();
        let task_id = id.clone();
        let timeout = self.timeout;
        let max_output = self.max_output;
        std::thread::spawn(move || {
            let (status, result) = match block_on_command(&command, &workspace, timeout, max_output)
            {
                Ok(output) => (
                    if output.exit_code.unwrap_or(0) == 0 {
                        Status::Completed
                    } else {
                        Status::Failed
                    },
                    format_result(&output.text, output.exit_code),
                ),
                Err(error) => (Status::Failed, format!("Error: {error}")),
            };

            let mut state = shared.state.lock().expect("background state poisoned");
            if let Some(task) = state.tasks.get_mut(&task_id) {
                task.status = status;
                task.result = result;
                state.ready.push_back(task_id.clone());
            }
        });

        Ok(id)
    }

    /// Drain finished tasks as `<task_notification>` blocks.
    pub fn collect(&self) -> Vec<String> {
        let ready: Vec<(String, Task)> = {
            let mut state = self.lock();
            let ids: Vec<String> = state.ready.drain(..).collect();
            ids.into_iter()
                .filter_map(|id| state.tasks.remove(&id).map(|task| (id, task)))
                .collect()
        };
        ready
            .into_iter()
            .map(|(id, task)| notification(&id, task.status, &task.command, &task.result))
            .collect()
    }

    /// Tasks still running.
    pub fn pending(&self) -> usize {
        self.lock()
            .tasks
            .values()
            .filter(|task| task.status == Status::Running)
            .count()
    }

    /// Tasks whose result is waiting to be collected.
    pub fn ready(&self) -> usize {
        self.lock().ready.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state.lock().expect("background state poisoned")
    }
}

/// Run one command on a fresh current-thread runtime (this is a worker thread,
/// so a slow command never needs the agent's runtime to make progress).
fn block_on_command(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    max_output: usize,
) -> Result<crate::tools::bash::ShellOutput> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_shell(command, cwd, timeout, max_output))
}

fn format_result(text: &str, exit_code: Option<i32>) -> String {
    let text = if text.is_empty() { "(no output)" } else { text };
    match exit_code {
        Some(code) if code != 0 => format!("Error: command exited with status {code}\n{text}"),
        _ => text.to_string(),
    }
}

fn notification(id: &str, status: Status, command: &str, result: &str) -> String {
    let summary: String = result.chars().take(SUMMARY_LIMIT).collect();
    format!(
        "<{NOTIFICATION_TAG}>\n  <task_id>{id}</task_id>\n  <status>{}</status>\n  \
         <command>{command}</command>\n  <summary>{summary}</summary>\n</{NOTIFICATION_TAG}>",
        status.name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    /// A command that outlives a short timeout on either platform.
    #[cfg(windows)]
    fn slow_command() -> &'static str {
        "ping -n 6 127.0.0.1 >nul"
    }

    #[cfg(not(windows))]
    fn slow_command() -> &'static str {
        "sleep 5"
    }

    /// Spin until a result is queued, failing if it takes too long.
    fn wait_ready(manager: &BackgroundManager) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while manager.ready() == 0 {
            assert!(
                Instant::now() < deadline,
                "background task did not finish in time"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn should_run_background_only_for_explicit_bash_flag() {
        assert!(should_run_background(
            "bash",
            &json!({ "run_in_background": true })
        ));
        assert!(!should_run_background(
            "bash",
            &json!({ "run_in_background": false })
        ));
        assert!(!should_run_background("bash", &json!({ "command": "ls" })));
        assert!(!should_run_background(
            "read_file",
            &json!({ "run_in_background": true })
        ));
        assert!(!should_run_background(
            "bash",
            &json!({ "run_in_background": "true" })
        ));
    }

    #[test]
    fn start_returns_an_id_and_a_named_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let manager = BackgroundManager::new();
        let id = manager.start(dir.path(), "echo hi").unwrap();
        assert!(id.starts_with("bg_"), "{id}");
        assert!(placeholder(&id).contains(&id));
    }

    #[test]
    fn an_empty_command_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let manager = BackgroundManager::new();
        assert!(manager.start(dir.path(), "   ").is_err());
    }

    #[test]
    fn a_finished_command_is_collected_as_a_notification() {
        let dir = tempfile::tempdir().unwrap();
        let manager = BackgroundManager::new();
        let id = manager.start(dir.path(), "echo hello-bg").unwrap();
        wait_ready(&manager);

        let notifications = manager.collect();
        assert_eq!(notifications.len(), 1);
        let note = &notifications[0];
        assert!(note.contains(&format!("<task_id>{id}</task_id>")), "{note}");
        assert!(note.contains("<status>completed</status>"), "{note}");
        assert!(note.contains("hello-bg"), "{note}");
        // The queue is drained: a second collect sees nothing.
        assert!(manager.collect().is_empty());
    }

    #[test]
    fn a_nonzero_exit_is_reported_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let manager = BackgroundManager::new();
        manager.start(dir.path(), "exit 3").unwrap();
        wait_ready(&manager);
        let note = &manager.collect()[0];
        assert!(note.contains("<status>failed</status>"), "{note}");
    }

    #[test]
    fn a_timeout_marks_the_task_failed() {
        let dir = tempfile::tempdir().unwrap();
        let manager = BackgroundManager::new().with_timeout(Duration::from_millis(300));
        manager.start(dir.path(), slow_command()).unwrap();
        wait_ready(&manager);
        let note = &manager.collect()[0];
        assert!(note.contains("<status>failed</status>"), "{note}");
        assert!(note.contains("timed out"), "{note}");
    }

    #[test]
    fn pending_and_ready_track_the_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let manager = BackgroundManager::new();
        manager.start(dir.path(), "echo one").unwrap();
        assert_eq!(manager.pending(), 1);
        assert_eq!(manager.ready(), 0);

        wait_ready(&manager);
        assert_eq!(manager.pending(), 0);
        assert_eq!(manager.ready(), 1);

        let _ = manager.collect();
        assert_eq!(manager.pending(), 0);
        assert_eq!(manager.ready(), 0);
    }
}
