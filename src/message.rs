//! Conversation data model.
//!
//! Mirrors the Anthropic Messages wire format: a conversation is a list of
//! [`Message`]s and each message carries a list of typed [`ContentBlock`]s.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A single block inside a message.
///
/// `tool_use` blocks are produced by the assistant; `tool_result` blocks are
/// produced by us and fed back on a user message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text { text: String },
    /// The model asking us to run a tool.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// The result of a tool we ran, fed back to the model.
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ContentBlock {
    /// Build a text block.
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }

    /// Build a tool-result block.
    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error,
        }
    }

    /// The text, if this is a text block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        }
    }

    /// The tool call, if this is a tool-use block.
    pub fn as_tool_call(&self) -> Option<ToolCall> {
        match self {
            ContentBlock::ToolUse { id, name, input } => Some(ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            _ => None,
        }
    }
}

/// A resolved tool invocation: what the model asked us to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned id, echoed back on the matching tool result.
    pub id: String,
    /// Registered tool name.
    pub name: String,
    /// Parsed arguments.
    pub input: Value,
}

/// One turn in the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// A user message holding a single text block.
    pub fn user_text(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// A user message from explicit blocks (typically tool results).
    pub fn user_blocks(content: Vec<ContentBlock>) -> Self {
        Message {
            role: Role::User,
            content,
        }
    }

    /// An assistant message from the model's raw content blocks.
    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Message {
            role: Role::Assistant,
            content,
        }
    }

    /// Concatenated text of every text block in this message.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("")
    }

    /// Every tool call in this message.
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(ContentBlock::as_tool_call)
            .collect()
    }
}
