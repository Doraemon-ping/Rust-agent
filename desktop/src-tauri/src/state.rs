//! Types the host sends to the frontend.
//!
//! These are serialized straight into `invoke` results and Tauri events, so
//! their field names are the frontend's contract: camelCase everywhere except
//! `snapshot`, which is the bridge's own `runtime.status` result passed through
//! untouched.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol revision this host speaks. Must match the bridge's constant.
pub const PROTOCOL_VERSION: u32 = 1;

/// A failure with a stable code, so the UI can branch without matching prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostError {
    pub code: String,
    pub message: String,
}

impl HostError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        HostError {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

/// Lifecycle of the runtime child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Starting,
    Ready,
    Stopping,
    Stopped,
    Error,
}

/// Severity of one runtime log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
    /// A stderr line the bridge did not format as JSON.
    Raw,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub ts: String,
    pub level: LogLevel,
    pub message: String,
}

/// Result of a `runtime.ping` round trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ping {
    pub ok: bool,
    pub latency_ms: u64,
    pub at: String,
    pub uptime_ms: u64,
    /// Why the ping failed, when it did. ok is the summary, this is the detail.
    pub error: Option<HostError>,
}

/// Everything the shell knows about the runtime, in one payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub state: State,
    pub pid: Option<u32>,
    pub bridge_path: Option<String>,
    pub protocol_version: Option<u32>,
    pub started_at: Option<String>,
    pub changed_at: String,
    pub last_error: Option<HostError>,
    pub last_ping: Option<Ping>,
    /// `runtime.status`, verbatim from the bridge.
    pub snapshot: Option<Value>,
}

impl Status {
    /// The state the shell is in before anything has been launched.
    pub fn stopped() -> Self {
        Status {
            state: State::Stopped,
            pid: None,
            bridge_path: None,
            protocol_version: None,
            started_at: None,
            changed_at: now(),
            last_error: None,
            last_ping: None,
            snapshot: None,
        }
    }

    /// Record a state change, stamping the time it happened.
    pub fn set_state(&mut self, state: State) {
        if self.state != state {
            self.state = state;
            self.changed_at = now();
        }
    }
}

/// Facts about the shell itself. Shown before the runtime is up.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostInfo {
    pub app_version: String,
    pub tauri_version: String,
    pub os: String,
    pub arch: String,
    pub protocol_version: u32,
    pub workspace: String,
    pub bridge_candidates: Vec<String>,
}

/// RFC3339 timestamp in UTC.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_serialize_lowercase_for_the_frontend() {
        let value = serde_json::to_value(State::Stopping).unwrap();
        assert_eq!(value, serde_json::json!("stopping"));
        let value = serde_json::to_value(LogLevel::Raw).unwrap();
        assert_eq!(value, serde_json::json!("raw"));
    }

    #[test]
    fn status_carries_the_snapshot_through_untouched() {
        let mut status = Status::stopped();
        status.snapshot = Some(serde_json::json!({ "workspace": "/tmp/x", "skills": { "count": 2 } }));
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["state"], serde_json::json!("stopped"));
        assert_eq!(value["snapshot"]["skills"]["count"], serde_json::json!(2));
        assert_eq!(value["bridgePath"], serde_json::Value::Null);
    }

    #[test]
    fn set_state_only_stamps_a_real_change() {
        let mut status = Status::stopped();
        let first = status.changed_at.clone();
        status.set_state(State::Stopped);
        assert_eq!(status.changed_at, first);
        status.set_state(State::Ready);
        assert_eq!(status.state, State::Ready);
        assert_ne!(status.changed_at, first);
    }
}
