//! Runtime configuration, read from the environment (`.env` friendly).

use crate::error::{Error, Result};

/// Endpoint, credentials, and model defaults.
#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL, e.g. `https://api.anthropic.com`.
    pub base_url: String,
    /// API key / auth token.
    pub api_key: String,
    /// Model id sent with every request.
    pub model: String,
    /// Response token budget per model call.
    pub max_tokens: u32,
}

impl Config {
    /// Read configuration from the process environment.
    ///
    /// Recognised variables: `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`
    /// (falling back to `ANTHROPIC_AUTH_TOKEN`), `MODEL_ID` (or
    /// `ANTHROPIC_MODEL`), and `MAX_TOKENS`.
    pub fn from_env() -> Result<Self> {
        let base_url = non_empty_env("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());

        let api_key = non_empty_env("ANTHROPIC_API_KEY")
            .or_else(|| non_empty_env("ANTHROPIC_AUTH_TOKEN"))
            .ok_or_else(|| {
                Error::Config("set ANTHROPIC_API_KEY (or ANTHROPIC_AUTH_TOKEN)".into())
            })?;

        let model = non_empty_env("MODEL_ID")
            .or_else(|| non_empty_env("ANTHROPIC_MODEL"))
            .ok_or_else(|| Error::Config("set MODEL_ID to the model you want to use".into()))?;

        let max_tokens = non_empty_env("MAX_TOKENS")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(8192);

        Ok(Config {
            base_url,
            api_key,
            model,
            max_tokens,
        })
    }

    /// The fully-qualified Messages endpoint.
    pub fn messages_endpoint(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
