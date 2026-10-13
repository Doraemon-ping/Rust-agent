//! A three-gate permission pipeline.
//!
//! Gate 1 -- deny list: commands that are never allowed.
//! Gate 2 -- rules: contextual checks (destructive shell, escaping paths).
//! Gate 3 -- approval: pause and ask the user before running.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::hooks::Hook;
use crate::message::ToolCall;
use crate::tools::normalize;

/// Outcome of evaluating a tool call against the policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run the tool.
    Allow,
    /// Refuse outright; reported to the model as a failed tool result.
    Deny(String),
    /// Pause and ask the user; the reason is shown in the prompt.
    Ask(String),
}

/// Commands that are never allowed, anywhere.
const DENY_PATTERNS: &[&str] = &[
    "rm -rf /",
    "sudo ",
    "shutdown",
    "reboot",
    "mkfs",
    "dd if=",
    ":(){:|:&};:",
];

/// Substrings that mark a shell command as destructive enough to confirm.
const DESTRUCTIVE_HINTS: &[&str] = &[
    "rm -rf",
    "rm -r",
    "rm -f",
    "chmod 777",
    "> /etc/",
    "del /f",
    "del /s",
    "format ",
    "diskpart",
];

/// Tools whose `path` argument must stay inside the workspace.
const FILE_TOOLS: &[&str] = &["read_file", "write_file", "edit_file"];

/// Evaluates tool calls before they are dispatched.
#[derive(Debug, Clone)]
pub struct PermissionPolicy {
    workspace: PathBuf,
    deny_patterns: Vec<String>,
    destructive_hints: Vec<String>,
}

impl PermissionPolicy {
    /// A policy rooted at `workspace`.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        PermissionPolicy {
            workspace: normalize(&workspace.into()),
            deny_patterns: DENY_PATTERNS.iter().map(|s| s.to_string()).collect(),
            destructive_hints: DESTRUCTIVE_HINTS.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Replace the deny list.
    pub fn with_deny_patterns(mut self, patterns: impl IntoIterator<Item = String>) -> Self {
        self.deny_patterns = patterns.into_iter().collect();
        self
    }

    /// Run every gate and return a decision.
    pub fn evaluate(&self, call: &ToolCall) -> Decision {
        if call.name == "bash" {
            if let Some(command) = call.input.get("command").and_then(Value::as_str) {
                if let Some(pattern) = self
                    .deny_patterns
                    .iter()
                    .find(|p| command.contains(p.as_str()))
                {
                    return Decision::Deny(format!("`{pattern}` is on the deny list"));
                }
                if is_destructive(command, &self.destructive_hints) {
                    return Decision::Ask("potentially destructive command".into());
                }
            }
        }

        if FILE_TOOLS.contains(&call.name.as_str()) {
            if let Some(path) = call.input.get("path").and_then(Value::as_str) {
                if !self.path_is_inside(path) {
                    return Decision::Ask(format!("path outside the workspace: {path}"));
                }
            }
        }

        Decision::Allow
    }

    /// Whether a model-supplied path stays inside the workspace.
    pub fn path_is_inside(&self, path: &str) -> bool {
        normalize(&self.workspace.join(path)).starts_with(&self.workspace)
    }
}

/// Answer to gate 3.
pub trait Approver: Send + Sync {
    /// Return `true` to allow the flagged call.
    fn approve(&self, call: &ToolCall, reason: &str) -> bool;
}

/// Approves everything. For trusted or non-interactive runs.
pub struct AutoApprover;

impl Approver for AutoApprover {
    fn approve(&self, _call: &ToolCall, _reason: &str) -> bool {
        true
    }
}

/// Refuses everything. For tests and dry runs.
pub struct DenyApprover;

impl Approver for DenyApprover {
    fn approve(&self, _call: &ToolCall, _reason: &str) -> bool {
        false
    }
}

/// A deliberately simple heuristic (no regex dependency): flag delete/format
/// verbs at the start of a shell segment, plus a handful of high-risk
/// substrings.
fn is_destructive(command: &str, hints: &[String]) -> bool {
    let lower = command.to_lowercase();

    if hints.iter().any(|hint| lower.contains(hint.as_str())) {
        return true;
    }

    for segment in lower.split([';', '&', '|', '\n', '(', ')']) {
        let segment = segment.trim_start();
        for verb in [
            "rm ", "rm\t", "del ", "rmdir ", "rd /s", "format ", "diskpart",
        ] {
            if segment.starts_with(verb) {
                return true;
            }
        }
    }

    false
}

/// s03's three-gate pipeline, expressed as a `PreToolUse` hook.
///
/// Installing this is what frees the loop of hard-coded permission checks:
/// the loop just asks its hooks.
pub struct PermissionHook {
    policy: PermissionPolicy,
    approver: Arc<dyn Approver>,
    interactive: bool,
}

impl PermissionHook {
    /// Wrap a policy and an approver.
    pub fn new(policy: PermissionPolicy, approver: Arc<dyn Approver>) -> Self {
        PermissionHook {
            policy,
            approver,
            interactive: true,
        }
    }

    /// Whether this turn may pause and ask the user (s15).
    ///
    /// An asynchronous turn -- a teammate, a background worker -- has no
    /// terminal to prompt on. With `interactive(false)` anything the policy
    /// would have asked about is denied outright, instead of blocking on an
    /// input nobody can answer.
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }
}

impl Hook for PermissionHook {
    fn pre_tool_use(&self, call: &ToolCall) -> Option<String> {
        match self.policy.evaluate(call) {
            Decision::Allow => None,
            Decision::Deny(reason) => Some(format!("Permission denied: {reason}")),
            Decision::Ask(reason) if !self.interactive => Some(format!(
                "Permission denied: {reason} (interactive approval is unavailable \
                 during an asynchronous turn)"
            )),
            Decision::Ask(reason) => {
                if self.approver.approve(call, &reason) {
                    None
                } else {
                    Some(format!("Permission denied by user: {reason}"))
                }
            }
        }
    }
}
