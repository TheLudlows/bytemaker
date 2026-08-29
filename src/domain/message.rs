//! Provider-neutral 对话领域模型（docs §3.1）。从 `client.rs` 迁入；签名不变。
//!
//! 无论底层用哪个 SDK，业务层只见这里的 `Message`/`ContentBlock`/`MessagesResponse`；
//! 供应商在 `providers::*` 边界处做数据转换。

use serde::{Deserialize, Serialize};

/// A message.
#[derive(Serialize, Deserialize, Clone)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

/// A content block.
///
/// Shared by requests and responses: serialized tagged by `type`
/// (text/tool_call/tool_output). Callers build Text/ToolOutput, read ToolCall/Text.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_output")]
    ToolOutput {
        call_id: String,
        content: String,
    },
}

impl ContentBlock {
    /// Build a Text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Build a ToolOutput block.
    pub fn tool_output(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::ToolOutput {
            call_id: call_id.into(),
            content: content.into(),
        }
    }
}

impl Message {
    /// Single user text message (role="user" + one Text block).
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Single user tool-output message (role="user" + one ToolOutput block).
    pub fn user_tool_output(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: vec![ContentBlock::ToolOutput {
                call_id: call_id.into(),
                content: content.into(),
            }],
        }
    }

    /// assistant message wrapping existing content blocks (e.g. replay API response).
    pub fn assistant_content(content: Vec<ContentBlock>) -> Self {
        Self {
            role: "assistant".to_string(),
            content,
        }
    }

    /// user message wrapping existing content blocks (tool results, reminders).
    pub fn user_blocks(content: Vec<ContentBlock>) -> Self {
        Self {
            role: "user".to_string(),
            content,
        }
    }

    /// Return a builder to mix Text/ToolCall/ToolOutput blocks in one message.
    pub fn builder() -> MessageBuilder {
        MessageBuilder::new()
    }
}

/// Builder to assemble a `Message` block by block.
///
/// Prefer the named constructors (`Message::user_text`/`assistant_content`/
/// `user_blocks`) for common shapes; use this builder to chain multiple
/// Text/ToolCall/ToolOutput blocks, then `.build()`.
#[derive(Default)]
pub struct MessageBuilder {
    role: String,
    content: Vec<ContentBlock>,
}

impl MessageBuilder {
    /// Empty builder (role and content both empty).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set role to user (same as `.role("user")`).
    pub fn user(mut self) -> Self {
        self.role = "user".to_string();
        self
    }

    /// Set role to assistant (same as `.role("assistant")`).
    pub fn assistant(mut self) -> Self {
        self.role = "assistant".to_string();
        self
    }

    /// Set the role.
    pub fn role(mut self, role: impl Into<String>) -> Self {
        self.role = role.into();
        self
    }

    /// Append a Text block.
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.content.push(ContentBlock::Text { text: text.into() });
        self
    }

    /// Append a ToolCall block.
    pub fn tool_call(
        mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Self {
        self.content.push(ContentBlock::ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        });
        self
    }

    /// Append a ToolOutput block.
    pub fn tool_output(
        mut self,
        call_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        self.content.push(ContentBlock::ToolOutput {
            call_id: call_id.into(),
            content: content.into(),
        });
        self
    }

    /// Append any content block.
    pub fn block(mut self, block: ContentBlock) -> Self {
        self.content.push(block);
        self
    }

    /// Replace content with the given blocks.
    pub fn content(mut self, blocks: Vec<ContentBlock>) -> Self {
        self.content = blocks;
        self
    }

    /// Build the `Message`.
    pub fn build(self) -> Message {
        Message {
            role: self.role,
            content: self.content,
        }
    }
}

/// Token usage captured from the SSE stream (s16 workflow plumbing).
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Model API response (accumulated stream result).
#[derive(Debug)]
pub struct MessagesResponse {
    pub content: Vec<ContentBlock>,
    pub finish_reason: String,
    pub usage: Option<Usage>,
}

/// 单次无状态补全结果（docs §3.1 字面类型）。供 `LlmProvider::generate_completion`。
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub content: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_response_construct() {
        let r = CompletionResponse {
            content: "hi".into(),
            prompt_tokens: 3,
            completion_tokens: 2,
        };
        assert_eq!(r.content, "hi");
        assert_eq!((r.prompt_tokens, r.completion_tokens), (3, 2));
    }

    #[test]
    fn message_user_text_roundtrip() {
        let m = Message::user_text("hello");
        assert_eq!(m.role, "user");
        assert_eq!(m.content.len(), 1);
        assert!(matches!(m.content[0], ContentBlock::Text { .. }));
    }
}
