//! The `bash` tool: run a shell command inside the workspace.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::process::Command;

use super::{required_str, truncate_chars, Tool, ToolContext};
use crate::error::{Error, Result};

/// Runs a command through the platform shell (`cmd.exe` on Windows,
/// `/bin/sh` elsewhere) with a hard timeout.
pub struct BashTool {
    timeout: Duration,
    max_output: usize,
    advertise_background: bool,
}

impl Default for BashTool {
    fn default() -> Self {
        BashTool {
            timeout: Duration::from_secs(120),
            max_output: 50_000,
            advertise_background: false,
        }
    }
}

impl BashTool {
    /// Override the execution timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Advertise the `run_in_background` parameter (s11).
    ///
    /// The agent loop reads it and dispatches to the background manager; this
    /// tool never sees it. Only advertise it when a manager is configured.
    pub fn advertising_background(mut self) -> Self {
        self.advertise_background = true;
        self
    }
}

/// Combined stdout+stderr of one shell run, plus its exit code.
pub(crate) struct ShellOutput {
    pub text: String,
    pub exit_code: Option<i32>,
}

/// Build the platform shell invocation (`cmd.exe /C`, or `sh -c`).
pub(crate) fn shell_command(command: &str) -> Command {
    if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

/// Run a shell command to completion, killing it if it outlives `timeout`.
pub(crate) async fn run_shell(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    max_output: usize,
) -> Result<ShellOutput> {
    let mut process = shell_command(command);
    process
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(timeout, process.output())
        .await
        .map_err(|_| Error::Tool {
            tool: "bash".into(),
            message: format!("command timed out after {timeout:?}"),
        })??;

    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(ShellOutput {
        text: truncate_chars(combined.trim(), max_output),
        exit_code: output.status.code(),
    })
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        if cfg!(windows) {
            "Run a shell command through cmd.exe. Use cmd.exe syntax, not PowerShell."
        } else {
            "Run a shell command."
        }
    }

    fn input_schema(&self) -> Value {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "command".into(),
            json!({ "type": "string", "description": "The shell command to run." }),
        );
        if self.advertise_background {
            properties.insert(
                "run_in_background".into(),
                json!({
                    "type": "boolean",
                    "description": "Set true only for independent, slow commands."
                }),
            );
        }
        json!({
            "type": "object",
            "properties": Value::Object(properties),
            "required": ["command"]
        })
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<String> {
        let command = required_str(&input, "command")?;
        let result = run_shell(command, ctx.workspace(), self.timeout, self.max_output).await?;
        if result.text.is_empty() {
            return Ok(format!(
                "(no output; exit code {})",
                result.exit_code.unwrap_or(-1)
            ));
        }
        Ok(result.text)
    }
}
