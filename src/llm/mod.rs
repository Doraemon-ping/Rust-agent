//! The model boundary: one small trait plus the wire types the loop needs.

mod anthropic;

pub use anthropic::AnthropicClient;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::message::{ContentBlock, Message, ToolCall};

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
/// tests. The loop only ever needs `complete`.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Run one model completion.
    async fn complete(&self, request: CompletionRequest<'_>) -> Result<CompletionResponse>;
}
