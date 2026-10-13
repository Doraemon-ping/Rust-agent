//! Anthropic Messages API client.

use async_trait::async_trait;
use serde::Serialize;
use std::time::Duration;

use super::{CompletionRequest, CompletionResponse, LlmClient, ToolDefinition};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::message::Message;

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Budget for reaching the endpoint at all.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Default budget for one read on the response stream. A connection the peer
/// silently drops would otherwise hang the session forever: there is no
/// timeout by default, so a stalled turn never returns. It has to clear a
/// whole non-streaming completion, so it is generous; raise it with
/// `MODEL_READ_TIMEOUT_SECS` for a slower provider.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(180);
/// Attempts per logical call: the first try plus retries.
const MAX_ATTEMPTS: u32 = 3;

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
            http: http_client(),
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

/// A client that cannot hang: a stalled connection trips the read timeout and
/// the deadline is reported to the caller instead of freezing the session.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(read_timeout())
        .build()
        .expect("build the HTTP client")
}

/// Per-read budget, overridable with `MODEL_READ_TIMEOUT_SECS`.
fn read_timeout() -> Duration {
    std::env::var("MODEL_READ_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_READ_TIMEOUT)
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
            model: request.model.unwrap_or(&self.model),
            max_tokens: request.max_tokens,
            system: request.system,
            messages: request.messages,
            tools: request.tools,
        };

        let mut attempt = 1;
        loop {
            match self.send_once(&body).await {
                Ok(response) => return Ok(response),
                Err(error) if attempt < MAX_ATTEMPTS && is_transient(&error) => {
                    tokio::time::sleep(retry_backoff(attempt)).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl AnthropicClient {
    /// One round trip to the messages endpoint.
    async fn send_once(&self, body: &MessagesBody<'_>) -> Result<CompletionResponse> {
        let response = self
            .http
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
            .json(body)
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

/// Whether the same call is worth another attempt. A dropped or stalled
/// connection and an overloaded provider are transient; a rejected request is
/// not (context overflow must reach the caller so it can compact).
fn is_transient(error: &Error) -> bool {
    match error {
        Error::Http(error) => error.is_timeout() || error.is_connect(),
        Error::Api { status, .. } => *status == 408 || *status == 429 || *status >= 500,
        _ => false,
    }
}

/// One second, then two, between attempts.
fn retry_backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << (attempt - 1).min(4))
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_against(endpoint: String, read_timeout: Duration) -> AnthropicClient {
        AnthropicClient {
            http: reqwest::Client::builder()
                .read_timeout(read_timeout)
                .build()
                .expect("build test client"),
            endpoint,
            api_key: "test-key".to_string(),
            model: "test-model".to_string(),
            version: ANTHROPIC_VERSION.to_string(),
        }
    }

    #[test]
    fn overload_and_transport_failures_are_retried_rejections_are_not() {
        let api = |status| Error::Api {
            status,
            message: String::new(),
        };
        assert!(is_transient(&api(408)));
        assert!(is_transient(&api(429)));
        assert!(is_transient(&api(500)));
        assert!(is_transient(&api(529)));
        assert!(!is_transient(&api(400)));
        assert!(!is_transient(&api(401)));
        assert!(!is_transient(&Error::Config("nope".to_string())));
        assert_eq!(retry_backoff(1), Duration::from_secs(1));
        assert_eq!(retry_backoff(2), Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_stalled_endpoint_fails_instead_of_hanging_forever() {
        // A server that accepts the connection and then never answers. Without
        // a read timeout this call would never return, freezing the session.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _connection = listener.accept().await;
            std::future::pending::<()>().await;
        });

        let client = client_against(
            format!("http://{addr}/v1/messages"),
            Duration::from_millis(200),
        );
        let error = client
            .complete(CompletionRequest {
                model: None,
                system: "",
                messages: &[],
                tools: &[],
                max_tokens: 16,
            })
            .await
            .unwrap_err();

        assert!(matches!(error, Error::Http(_)), "{error}");
    }
}
