//! Anthropic Messages API client.

use async_trait::async_trait;
use serde::Serialize;

use super::{CompletionRequest, CompletionResponse, LlmClient, ToolDefinition};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::message::Message;

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Talks to an Anthropic-compatible `/v1/messages` endpoint.
pub struct AnthropicClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
    version: String,
}

impl AnthropicClient {
    /// Build a client from configuration.
    pub fn new(config: &Config) -> Self {
        AnthropicClient {
            http: reqwest::Client::new(),
            endpoint: config.messages_endpoint(),
            api_key: config.api_key.clone(),
            model: config.model.clone(),
            version: ANTHROPIC_VERSION.to_string(),
        }
    }

    /// Build a client straight from the environment.
    pub fn from_env() -> Result<Self> {
        Ok(Self::new(&Config::from_env()?))
    }
}

#[derive(Serialize)]
struct MessagesBody<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "slice_is_empty")]
    tools: &'a [ToolDefinition],
}

fn slice_is_empty<T>(slice: &[T]) -> bool {
    slice.is_empty()
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn complete(&self, request: CompletionRequest<'_>) -> Result<CompletionResponse> {
        let body = MessagesBody {
            model: &self.model,
            max_tokens: request.max_tokens,
            system: request.system,
            messages: request.messages,
            tools: request.tools,
        };

        let response = self
            .http
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;

        if !status.is_success() {
            return Err(Error::Api {
                status: status.as_u16(),
                message: truncate(&text, 2_000),
            });
        }

        Ok(serde_json::from_str(&text)?)
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}...")
    }
}
