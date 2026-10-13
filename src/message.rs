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
    /// Extended-thinking reasoning emitted alongside the visible answer.
    ///
    /// Preserved verbatim (including `signature`) because the Messages API
    /// requires thinking blocks to be echoed back unmodified on the next turn.
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Encrypted reasoning the model chose not to expose; echoed back as-is.
    RedactedThinking {
        #[serde(default)]
        data: String,
    },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_blocks_round_trip_verbatim() {
        let raw = r#"{
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "let me plan", "signature": "abc123"},
                {"type": "text", "text": "here is the plan"}
            ]
        }"#;
        let message: Message = serde_json::from_str(raw).unwrap();
        assert_eq!(message.text(), "here is the plan");

        // The signature must survive the round trip; the API rejects a
        // replayed thinking block whose signature was dropped.
        let encoded = serde_json::to_value(&message).unwrap();
        assert_eq!(encoded["content"][0]["type"], "thinking");
        assert_eq!(encoded["content"][0]["signature"], "abc123");
    }

    #[test]
    fn thinking_and_redacted_blocks_do_not_masquerade_as_text_or_tools() {
        let raw = r#"[
            {"type": "thinking", "thinking": "hmm"},
            {"type": "redacted_thinking", "data": "encrypted"},
            {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}}
        ]"#;
        let blocks: Vec<ContentBlock> = serde_json::from_str(raw).unwrap();
        assert!(blocks.iter().all(|block| block.as_text().is_none()));
        assert_eq!(
            blocks.iter().filter_map(ContentBlock::as_tool_call).count(),
            1
        );
        // A missing signature must not be serialized as `null`.
        let encoded = serde_json::to_value(&blocks[0]).unwrap();
        assert!(encoded.get("signature").is_none());
    }
}
