//! Error types shared across the runtime.

use thiserror::Error;

/// Convenience alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while driving the agent.
#[derive(Debug, Error)]
pub enum Error {
    /// Transport-level failure while talking to the model provider.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// The provider returned a body we could not parse.
    #[error("failed to parse model response: {0}")]
    Json(#[from] serde_json::Error),

    /// Local filesystem failure inside a tool.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// An invalid glob pattern.
    #[error("invalid glob pattern: {0}")]
    Pattern(#[from] glob::PatternError),

    /// The provider answered with a non-success status.
    #[error("model api error ({status}): {message}")]
    Api { status: u16, message: String },

    /// Missing or malformed configuration.
    #[error("configuration error: {0}")]
    Config(String),

    /// A tool failed; reported back to the model as a failed tool result.
    #[error("tool `{tool}` failed: {message}")]
    Tool { tool: String, message: String },

    /// The model asked for a tool that is not registered.
    #[error("unknown tool: {0}")]
    UnknownTool(String),

    /// The model asked for a skill that is not in the catalog.
    #[error("unknown skill `{name}`; available: {available}")]
    UnknownSkill { name: String, available: String },

    /// A path argument tried to escape the workspace.
    #[error("path escapes the workspace: {0}")]
    PathEscape(String),

    /// A memory store rejected an invalid record or filename.
    #[error("memory error: {0}")]
    Memory(String),

    /// The loop hit its safety bound without the model stopping.
    #[error("agent exceeded the maximum number of iterations ({0})")]
    MaxIterations(usize),
}
