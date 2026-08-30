//! Provider-neutral 对话领域模型（docs §3.1）。从 `client.rs` 迁入；签名不变。
//!
//! 无论底层用哪个 SDK，业务层只见这里的 `Message`/`ContentBlock`/`MessagesResponse`；
//! 供应商在 `providers::*` 边界处做数据转换。

use serde::{Deserialize, Serialize};

/// Conversation role (docs §3.1).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    /// Wire string for this role (`"system"` / `"user"` / `"assistant"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Default for Role {
    fn default() -> Self {
        Role::User
    }
}

/// A message.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

/// A content block.
///
/// Shared by requests and responses: serialized tagged by `type`
/// (text/tool_call/tool_output). Callers build Text/ToolOutput, read ToolCall/Text.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
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
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Single user tool-output message (role="user" + one ToolOutput block).
    pub fn user_tool_output(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::ToolOutput {
                call_id: call_id.into(),
                content: content.into(),
            }],
        }
    }

    /// assistant message wrapping existing content blocks (e.g. replay API response).
    pub fn assistant_content(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content,
        }
    }

    /// user message wrapping existing content blocks (tool results, reminders).
    pub fn user_blocks(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
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
    role: Role,
    content: Vec<ContentBlock>,
}

impl MessageBuilder {
    /// Empty builder (role and content both empty).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set role to user (same as `.role(Role::User)`).
    pub fn user(mut self) -> Self {
        self.role = Role::User;
        self
    }

    /// Set role to assistant (same as `.role(Role::Assistant)`).
    pub fn assistant(mut self) -> Self {
        self.role = Role::Assistant;
        self
    }

    /// Set the role.
    pub fn role(mut self, role: Role) -> Self {
        self.role = role;
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
/// s18（ch5 eval）：额外 derive Serialize/Deserialize，供盒带 JSONL 落盘。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
        assert_eq!(m.role, Role::User);
        assert_eq!(m.content.len(), 1);
        assert!(matches!(m.content[0], ContentBlock::Text { .. }));
    }

    #[test]
    fn role_serializes_to_lowercase_wire_string() {
        // Lock the provider wire contract: a Role must serialize to a bare
        // lowercase string, NOT an externally-tagged {"User": ...}. Removing
        // #[serde(rename_all = "lowercase")] must turn this red.
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(serde_json::to_string(&Role::Assistant).unwrap(), "\"assistant\"");
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
    }

    #[test]
    fn message_role_roundtrips_wire_format() {
        // Round-trip must preserve role AND content shape (tagged ContentBlock).
        let m = Message::user_text("hi");
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(
            json,
            r#"{"role":"user","content":[{"type":"text","text":"hi"}]}"#
        );
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.role, Role::User);
        assert_eq!(back.content.len(), 1);
    }
}
