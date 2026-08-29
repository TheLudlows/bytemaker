//! 供应商抽象层：`LlmProvider` trait + `CallResult` + `MockProvider`（docs §3.2）。
//!
//! 业务代码（agent loop、compactor、memory、goal evaluator）依赖 `LlmProvider`，
//! 永不直接依赖 `async-openai` 类型。具体实现见 `providers::openai`；测试用
//! `MockProvider`。这是 docs §3.2 的防腐边界。

pub mod openai;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::domain::message::{CompletionResponse, ContentBlock, Message, MessagesResponse, Usage};
use crate::error::LlmError;
use crate::tools::trait_def::ToolDefinition;

/// 流式补全的供应商中立结果。`PromptTooLong`/`Failure` 携 `LlmError`；
/// 无需区分 prompt-too-long 的调用方用 `.into_response()`（桥接到 `AgentError`）。
#[derive(Debug)]
pub enum CallResult {
    Success(MessagesResponse),
    PromptTooLong(LlmError),
    Failure(LlmError),
    Cancelled,
}

impl CallResult {
    /// 转为 `Result<MessagesResponse, AgentError>`（经 `From<LlmError> for AgentError`）。
    pub fn into_response(self) -> Result<MessagesResponse, crate::error::AgentError> {
        match self {
            Self::Success(r) => Ok(r),
            Self::PromptTooLong(e) | Self::Failure(e) => Err(e.into()),
            Self::Cancelled => Err(crate::error::AgentError::Stream("Cancelled".to_string())),
        }
    }

    /// O(1) 变体判定（分类已在 `LlmError::from_api_error` 完成）。
    pub fn is_prompt_too_long(&self) -> bool {
        matches!(self, Self::PromptTooLong(_))
    }
}

/// LLM 供应商抽象（依赖倒置）。`OpenAiProvider`、`MockProvider` 实现之；
/// Agent 持 `Arc<dyn LlmProvider>`，与 SDK 解耦。
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// 流式补全 + 工具 + 取消，返回累积后的 `CallResult`。
    async fn stream_messages(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        max_tokens: u32,
        cancel: CancellationToken,
    ) -> CallResult;

    /// 单次无状态便捷方法（docs §3.2 `generate_completion`）。默认实现包一层
    /// `stream_messages`，供 `MockProvider` 与「Hello, Agent」验收使用；具体供应商可覆盖。
    async fn generate_completion(
        &self,
        messages: &[Message],
    ) -> Result<CompletionResponse, LlmError> {
        let cancel = CancellationToken::new();
        match self.stream_messages("", messages, &[], 1024, cancel).await {
            CallResult::Success(r) => {
                let content = r
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let (prompt_tokens, completion_tokens) = r
                    .usage
                    .map(|u| (u.input_tokens as u32, u.output_tokens as u32))
                    .unwrap_or((0, 0));
                Ok(CompletionResponse {
                    content,
                    prompt_tokens,
                    completion_tokens,
                })
            }
            CallResult::PromptTooLong(e) | CallResult::Failure(e) => Err(e),
            CallResult::Cancelled => Err(LlmError::Stream("Cancelled".to_string())),
        }
    }
}

/// 返回写死字符串的 Mock 供应商（测试/演示用）。证明 Agent 能不改核心逻辑切供应商
/// （docs §6 验收 #3）。
pub struct MockProvider {
    content: String,
}

impl MockProvider {
    pub fn new(content: impl Into<String>) -> Self {
        Self { content: content.into() }
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn stream_messages(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _max_tokens: u32,
        _cancel: CancellationToken,
    ) -> CallResult {
        CallResult::Success(MessagesResponse {
            content: vec![ContentBlock::Text { text: self.content.clone() }],
            finish_reason: "stop".to_string(),
            usage: Some(Usage::default()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// 验收 #3：MockProvider 经与真实供应商相同的 `LlmProvider` 接口返回写死字符串，
    /// 核心逻辑（generate_completion）对二者不变，且不涉及任何 SDK。
    #[tokio::test]
    async fn mock_provider_generate_completion_returns_hardcoded() {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new("hello from mock"));
        let resp = provider
            .generate_completion(&[Message::user_text("anything")])
            .await
            .expect("mock generate_completion");
        assert_eq!(resp.content, "hello from mock");
    }

    #[tokio::test]
    async fn mock_provider_stream_returns_success() {
        let provider = MockProvider::new("hi");
        let cancel = CancellationToken::new();
        let r = provider.stream_messages("", &[], &[], 100, cancel).await;
        assert!(matches!(r, CallResult::Success(_)));
    }

    #[test]
    fn call_result_is_prompt_too_long_variant() {
        let e = LlmError::PromptTooLong("x".into());
        assert!(CallResult::PromptTooLong(e).is_prompt_too_long());
        let r = MessagesResponse {
            content: vec![],
            finish_reason: "stop".into(),
            usage: None,
        };
        assert!(!CallResult::Success(r).is_prompt_too_long());
    }

    #[test]
    fn call_result_into_response_bridges_to_agent_error() {
        let err = CallResult::Failure(LlmError::RateLimit("rl".into()))
            .into_response()
            .unwrap_err();
        assert!(matches!(err, crate::error::AgentError::Api { status: 429, .. }));
    }
}
