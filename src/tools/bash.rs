//! The `bash` tool: run a shell command inside the workspace.

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
}

impl Default for BashTool {
    fn default() -> Self {
        BashTool {
            timeout: Duration::from_secs(120),
            max_output: 50_000,
        }
    }
}

impl BashTool {
    /// Override the execution timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
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
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to run." }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<String> {
        let command = required_str(&input, "command")?;

        let mut process = if cfg!(windows) {
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(command);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(command);
            cmd
        };

        process
            .current_dir(ctx.workspace())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = match tokio::time::timeout(self.timeout, process.output()).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(Error::Tool {
                    tool: "bash".into(),
                    message: format!("command timed out after {:?}", self.timeout),
                })
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut combined = String::with_capacity(stdout.len() + stderr.len());
        combined.push_str(stdout.as_ref());
        combined.push_str(stderr.as_ref());
        let combined = combined.trim();

        if combined.is_empty() {
            return Ok(format!(
                "(no output; exit code {})",
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(truncate_chars(combined, self.max_output))
    }
}
