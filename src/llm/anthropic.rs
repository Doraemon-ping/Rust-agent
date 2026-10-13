//! Anthropic Messages API client.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

use super::{CompletionRequest, CompletionResponse, LlmClient, StreamEvent, ToolDefinition, Usage};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::message::{ContentBlock, Message, ToolCall};

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
    /// Set on the streaming request only; the non-streaming body is unchanged.
    #[serde(skip_serializing_if = "is_false")]
    stream: bool,
}

fn slice_is_empty<T>(slice: &[T]) -> bool {
    slice.is_empty()
}

fn is_false(value: &bool) -> bool {
    !*value
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
            stream: false,
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

    async fn stream(
        &self,
        request: CompletionRequest<'_>,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<CompletionResponse> {
        let body = MessagesBody {
            model: request.model.unwrap_or(&self.model),
            max_tokens: request.max_tokens,
            system: request.system,
            messages: request.messages,
            tools: request.tools,
            stream: true,
        };

        // Retry a stream that failed before it produced anything (a refused
        // connection, a 429, an overloaded error event). Once a delta has been
        // shown, replaying would duplicate it, so the failure is reported.
        let mut attempt = 1;
        loop {
            let mut emitted = false;
            let mut counted = |event: StreamEvent| {
                emitted = true;
                on_event(event);
            };
            match self.send_streaming(&body, &mut counted).await {
                Ok(response) => return Ok(response),
                Err(error) if attempt < MAX_ATTEMPTS && !emitted && is_transient(&error) => {
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

    /// One streaming round trip: read the SSE body a chunk at a time, hand
    /// text and finished tool calls to `on_event` as they land, and assemble
    /// the same response `send_once` would have parsed.
    async fn send_streaming(
        &self,
        body: &MessagesBody<'_>,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<CompletionResponse> {
        let mut response = self
            .http
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
            .header("accept", "text/event-stream")
            .json(body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                status: status.as_u16(),
                message: truncate(&text, 2_000),
            });
        }

        let mut assembler = StreamAssembler::default();
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            buffer.extend_from_slice(&chunk);
            // Decode only whole lines: a chunk can split a multi-byte character.
            while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = buffer.drain(..=newline).collect();
                let line = String::from_utf8_lossy(&line);
                let Some(data) = line.trim_end().strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let event: Value = serde_json::from_str(data)?;
                assembler.apply(&event, on_event)?;
            }
        }
        Ok(assembler.finish())
    }
}

/// Whether the same call is worth another attempt. A dropped or stalled
/// connection and an overloaded provider are transient; a rejected request is
/// not (context overflow must reach the caller so it can compact).
fn is_transient(error: &Error) -> bool {
    match error {
        Error::Http(error) => error.is_timeout() || error.is_connect(),
        Error::Api { status, .. } => *status == 408 || *status == 429 || *status >= 500,
        // A stream that reported an error before any delta is worth one more try.
        Error::Stream(_) => true,
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

/// Assembles Anthropic SSE events into the response `complete` would have
/// parsed, reporting text and finished tool calls as they arrive.
#[derive(Default)]
struct StreamAssembler {
    blocks: Vec<Option<ContentBlock>>,
    /// `content_block` index -> accumulated `input_json_delta` payload.
    tool_json: BTreeMap<usize, String>,
    stop_reason: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

impl StreamAssembler {
    /// Fold one SSE `data:` payload into the response.
    fn apply(
        &mut self,
        event: &Value,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<()> {
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "message_start" => {
                let message = event.get("message");
                self.model = optional_field(message, "model");
                self.usage = parse_usage(message.and_then(|message| message.get("usage")));
            }
            "content_block_start" => {
                let index = block_index(event);
                let block = event.get("content_block");
                let started = match block
                    .and_then(|block| block.get("type"))
                    .and_then(Value::as_str)
                {
                    Some("tool_use") => {
                        self.tool_json.insert(index, String::new());
                        ContentBlock::ToolUse {
                            id: field(block, "id"),
                            name: field(block, "name"),
                            input: Value::Null,
                        }
                    }
                    Some("thinking") => ContentBlock::Thinking {
                        thinking: field(block, "thinking"),
                        signature: optional_field(block, "signature"),
                    },
                    Some("redacted_thinking") => ContentBlock::RedactedThinking {
                        data: field(block, "data"),
                    },
                    _ => ContentBlock::text(field(block, "text")),
                };
                self.set(index, started);
            }
            "content_block_delta" => {
                let index = block_index(event);
                let delta = event.get("delta");
                match delta
                    .and_then(|delta| delta.get("type"))
                    .and_then(Value::as_str)
                {
                    Some("text_delta") => {
                        let text = field(delta, "text");
                        if !text.is_empty() {
                            on_event(StreamEvent::Text(text.clone()));
                        }
                        self.append_text(index, &text);
                    }
                    Some("thinking_delta") => {
                        self.append_thinking(index, &field(delta, "thinking"))
                    }
                    Some("signature_delta") => {
                        self.set_signature(index, optional_field(delta, "signature"))
                    }
                    Some("input_json_delta") => {
                        if let Some(json) = self.tool_json.get_mut(&index) {
                            json.push_str(&field(delta, "partial_json"));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = block_index(event);
                if let Some(json) = self.tool_json.remove(&index) {
                    let input = serde_json::from_str(&json).unwrap_or(Value::Null);
                    if let Some(Some(ContentBlock::ToolUse { input: slot, .. })) =
                        self.blocks.get_mut(index)
                    {
                        *slot = input.clone();
                    }
                    if let Some(Some(ContentBlock::ToolUse { id, name, .. })) =
                        self.blocks.get(index)
                    {
                        on_event(StreamEvent::ToolCall(ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            input,
                        }));
                    }
                }
            }
            "message_delta" => {
                if let Some(reason) = event
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(delta) = parse_usage(event.get("usage")) {
                    let mut usage = self.usage.take().unwrap_or_default();
                    usage.output_tokens = delta.output_tokens;
                    if delta.input_tokens > 0 {
                        usage.input_tokens = delta.input_tokens;
                    }
                    self.usage = Some(usage);
                }
            }
            "error" => {
                let message = event
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown stream error");
                return Err(Error::Stream(message.to_string()));
            }
            _ => {}
        }
        Ok(())
    }

    /// Build the response the loop consumes.
    fn finish(self) -> CompletionResponse {
        let content: Vec<ContentBlock> = self.blocks.into_iter().flatten().collect();
        let stop_reason = self.stop_reason.unwrap_or_else(|| {
            let reason = if content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
            {
                "tool_use"
            } else {
                "end_turn"
            };
            reason.to_string()
        });
        CompletionResponse {
            content,
            stop_reason: Some(stop_reason),
            model: self.model,
            usage: self.usage,
        }
    }

    fn set(&mut self, index: usize, block: ContentBlock) {
        if self.blocks.len() <= index {
            self.blocks.resize(index + 1, None);
        }
        self.blocks[index] = Some(block);
    }

    fn append_text(&mut self, index: usize, text: &str) {
        if let Some(Some(ContentBlock::Text { text: existing })) = self.blocks.get_mut(index) {
            existing.push_str(text);
        }
    }

    fn append_thinking(&mut self, index: usize, thinking: &str) {
        if let Some(Some(ContentBlock::Thinking {
            thinking: existing, ..
        })) = self.blocks.get_mut(index)
        {
            existing.push_str(thinking);
        }
    }

    fn set_signature(&mut self, index: usize, signature: Option<String>) {
        if let Some(signature) = signature {
            if let Some(Some(ContentBlock::Thinking {
                signature: slot, ..
            })) = self.blocks.get_mut(index)
            {
                *slot = Some(signature);
            }
        }
    }
}

/// The `index` an SSE event applies to; Anthropic always sends one.
fn block_index(event: &Value) -> usize {
    event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize
}

/// A required string field of an SSE object, empty when absent.
fn field(object: Option<&Value>, key: &str) -> String {
    object
        .and_then(|object| object.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// An optional string field of an SSE object.
fn optional_field(object: Option<&Value>, key: &str) -> Option<String> {
    object
        .and_then(|object| object.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Token counts from a `usage` object.
fn parse_usage(value: Option<&Value>) -> Option<Usage> {
    let value = value?;
    Some(Usage {
        input_tokens: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    })
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

    #[tokio::test]
    async fn a_streamed_response_is_assembled_from_its_events() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 8192];
            let _ = socket.read(&mut buffer).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n")
                .await
                .unwrap();
            for frame in SSE_FRAMES {
                socket.write_all(frame.as_bytes()).await.unwrap();
                socket.write_all(b"\n\n").await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        let client = client_against(format!("http://{addr}/v1/messages"), Duration::from_secs(5));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&seen);
        let response = client
            .stream(
                CompletionRequest {
                    model: None,
                    system: "",
                    messages: &[],
                    tools: &[],
                    max_tokens: 64,
                },
                &mut |event| recorder.lock().unwrap().push(event),
            )
            .await
            .unwrap();

        assert_eq!(response.text(), "Hello world");
        assert_eq!(response.stop_reason(), crate::llm::StopReason::ToolUse);
        assert_eq!(response.model.as_deref(), Some("claude-x"));
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 10,
                output_tokens: 12,
            })
        );
        let calls = response.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].input, serde_json::json!({ "command": "ls" }));

        // The text arrived in two deltas and the tool call arrived whole.
        let events = seen.lock().unwrap();
        let texts: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hello ", "world"]);
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::ToolCall(call) if call.id == "tool_1")));
    }

    /// A text block and a tool_use block, with a `ping` mixed in for good measure.
    const SSE_FRAMES: [&str; 13] = [
        r#"event: ping"#,
        r#"data: {"type":"ping"}"#,
        r#"data: {"type":"message_start","message":{"model":"claude-x","usage":{"input_tokens":10,"output_tokens":1}}}"#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello "}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tool_1","name":"bash"}}"#,
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
        r#"data: {"type":"content_block_stop","index":1}"#,
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}"#,
        r#"data: {"type":"message_stop"}"#,
    ];
}
