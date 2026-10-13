//! The model boundary: one small trait plus the wire types the loop needs.

mod anthropic;

pub use anthropic::AnthropicClient;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::message::{ContentBlock, Message, ToolCall};

/// A delta reported while a completion streams in.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A run of assistant text, in the order the model produced it.
    Text(String),
    /// The model finished naming one tool. The call is complete, so a caller
    /// need not wait for the response as a whole to finish arriving before
    /// acting on it.
    ToolCall(ToolCall),
}

/// A tool advertised to the model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Everything needed for one model call.
///
/// Borrowed on purpose: the loop hands over the live conversation instead of
/// cloning it on every iteration.
#[derive(Debug, Clone)]
pub struct CompletionRequest<'a> {
    /// Override the client's configured model for this call.
    ///
    /// The recovery layer uses it to fall back to a second model; `None` means
    /// "whatever the client was configured with".
    pub model: Option<&'a str>,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDefinition],
    pub max_tokens: u32,
}

/// A parsed model response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionResponse {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

impl CompletionResponse {
    /// Concatenated text of every text block.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("")
    }

    /// Every tool call the model asked for.
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(ContentBlock::as_tool_call)
            .collect()
    }

    /// Whether the model asked for at least one tool.
    pub fn wants_tools(&self) -> bool {
        self.content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
    }

    /// Normalised stop reason.
    pub fn stop_reason(&self) -> StopReason {
        StopReason::parse(self.stop_reason.as_deref())
    }
}

/// Token accounting reported by the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    PauseTurn,
    Refusal,
    Unknown,
}

impl StopReason {
    fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("end_turn") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("stop_sequence") => StopReason::StopSequence,
            Some("pause_turn") => StopReason::PauseTurn,
            Some("refusal") => StopReason::Refusal,
            _ => StopReason::Unknown,
        }
    }
}

/// The seam between the agent loop and any model provider.
///
/// Implement this to plug in another vendor, a local model, or a mock in
/// tests. The loop drives every call through `stream`, whose default falls
/// back to `complete`.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Run one model completion.
    async fn complete(&self, request: CompletionRequest<'_>) -> Result<CompletionResponse>;

    /// Stream one completion, reporting text and finished tool calls as they
    /// arrive, and return the assembled response.
    ///
    /// The loop always goes through this method, so a client that can only do
    /// whole responses is fine: the default replays `complete` as one event per
    /// block. Implement it to make the first token visible before the response
    /// ends.
    async fn stream(
        &self,
        request: CompletionRequest<'_>,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<CompletionResponse> {
        let response = self.complete(request).await?;
        for block in &response.content {
            if let Some(text) = block.as_text() {
                on_event(StreamEvent::Text(text.to_string()));
            } else if let Some(call) = block.as_tool_call() {
                on_event(StreamEvent::ToolCall(call));
            }
        }
        Ok(response)
    }
}
