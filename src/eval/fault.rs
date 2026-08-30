//! 故障注入与脚本 Provider（docs/5.evals.md §2.5 / §3.1）。
//!
//! 手写响应不适合模拟正常流（与真实供应商结构漂移大），却是模拟**错误路径**的最佳
//! 工具——没有供应商会为你稳定地复现 429。`FaultyProvider` 用声明式故障脚本验证
//! `LlmError` 分类与重试逻辑（`RetryProvider`），全程零网络（验收 #4）。
//!
//! 设计说明：重试逻辑在 `OpenAiProvider` 内部由 async-openai 的 `OpenAIRetry`
//! 层承担，`LlmProvider` trait 层没有可测的重试路径。`RetryProvider` 把重试
//! 提升到防腐层（eval live 模式直接受益），`FaultyProvider` 因此能**零网络**
//! 验证「429×2 后成功，总调用 = 3」（验收 #4）。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::domain::message::{ContentBlock, Message, MessagesResponse, Usage};
use crate::error::LlmError;
use crate::providers::{CallResult, LlmProvider};
use crate::tools::trait_def::ToolDefinition;

// ---- 响应构造 helper（测试与盒带生成共用） ----

/// 纯文本响应（finish_reason="stop"）。
pub fn text_response(text: &str) -> MessagesResponse {
    MessagesResponse {
        content: vec![ContentBlock::text(text)],
        finish_reason: "stop".to_string(),
        usage: Some(Usage::default()),
    }
}

/// 带用量的纯文本响应（给 MeteredProvider 测试/断言 token 用）。
pub fn resp_with_usage(text: &str, input_tokens: u64, output_tokens: u64) -> MessagesResponse {
    MessagesResponse {
        content: vec![ContentBlock::text(text)],
        finish_reason: "stop".to_string(),
        usage: Some(Usage { input_tokens, output_tokens }),
    }
}

/// 单个工具调用响应（finish_reason="tool_call"）。
pub fn tool_call_response(id: &str, name: &str, input: serde_json::Value) -> MessagesResponse {
    MessagesResponse {
        content: vec![ContentBlock::ToolCall { id: id.to_string(), name: name.to_string(), input }],
        finish_reason: "tool_call".to_string(),
        usage: Some(Usage::default()),
    }
}

// ---- ScriptedProvider：按序吐出预置响应 ----

/// 脚本 Provider：声明式响应序列，用于盒带录制（Task 11 生成提交进 git 的盒带）
/// 与端到端测试。耗尽时显式报错，绝不静默循环。
pub struct ScriptedProvider {
    responses: Mutex<VecDeque<MessagesResponse>>,
    calls: AtomicU64,
}

impl ScriptedProvider {
    pub fn new(responses: Vec<MessagesResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            calls: AtomicU64::new(0),
        }
    }

    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    async fn stream_messages(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _max_tokens: u32,
        _cancel: CancellationToken,
    ) -> CallResult {
        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        match self.responses.lock().unwrap().pop_front() {
            Some(r) => CallResult::Success(r),
            None => CallResult::Failure(LlmError::Other(format!(
                "scripted provider exhausted at call #{n} (add more responses)"
            ))),
        }
    }
}

// ---- FaultyProvider：声明式故障注入 ----

/// 注入的故障种类（映射到 `LlmError` 分类，docs ch0 §2.3）。
#[derive(Clone)]
pub enum Fault {
    RateLimit,
    Timeout,
    MalformedJson,
    AuthError,
}

/// 故障脚本的一步：失败（注入错误）或成功（预置响应）。
#[derive(Clone)]
pub enum FaultStep {
    Fail(Fault),
    Succeed(MessagesResponse),
}

fn fault_error(f: &Fault) -> LlmError {
    match f {
        Fault::RateLimit => LlmError::RateLimit("injected 429".into()),
        Fault::Timeout => LlmError::Timeout { seconds: 30 },
        Fault::MalformedJson => LlmError::InvalidResponse("injected malformed JSON".into()),
        Fault::AuthError => LlmError::AuthError("injected 401".into()),
    }
}

/// 故障注入 Provider：「第 2 次调用返回 429，第 3 次返回畸形 JSON」这类声明式脚本。
/// `call_counter()` 供断言「总调用次数 = 3」（验收 #4）。
pub struct FaultyProvider {
    script: Vec<FaultStep>,
    calls: Arc<AtomicU64>,
}

impl FaultyProvider {
    pub fn new(script: Vec<FaultStep>) -> Self {
        Self { script, calls: Arc::new(AtomicU64::new(0)) }
    }

    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// 共享计数器（配合 `RetryProvider` 断言 inner 的真实调用次数）。
    pub fn call_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.calls)
    }
}

#[async_trait]
impl LlmProvider for FaultyProvider {
    async fn stream_messages(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _max_tokens: u32,
        _cancel: CancellationToken,
    ) -> CallResult {
        let n = self.calls.fetch_add(1, Ordering::Relaxed);
        match self.script.get(n as usize) {
            Some(FaultStep::Fail(f)) => CallResult::Failure(fault_error(f)),
            Some(FaultStep::Succeed(r)) => CallResult::Success(r.clone()),
            None => CallResult::Failure(LlmError::Other(format!(
                "fault script exhausted at call #{} (script len {})",
                n + 1,
                self.script.len()
            ))),
        }
    }
}

// ---- RetryProvider：防腐层重试（可被 FaultyProvider 零网络验证） ----

/// trait 层重试：`RateLimit | Network | Timeout` 指数退避重试（base * 2^attempt）。
/// `OpenAiProvider` 内部另有 async-openai 的 `OpenAIRetry`；`RetryProvider` 把重试
/// 提升到防腐层，使 eval live 模式与故障注入测试共用同一套可验证逻辑。
pub struct RetryProvider {
    inner: Arc<dyn LlmProvider>,
    max_retries: u32,
    base_delay: Duration,
}

impl RetryProvider {
    pub fn new(inner: Arc<dyn LlmProvider>, max_retries: u32, base_delay: Duration) -> Self {
        Self { inner, max_retries, base_delay }
    }
}

#[async_trait]
impl LlmProvider for RetryProvider {
    async fn stream_messages(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        max_tokens: u32,
        cancel: CancellationToken,
    ) -> CallResult {
        let mut attempt: u32 = 0;
        loop {
            let result = self
                .inner
                .stream_messages(system, messages, tools, max_tokens, cancel.clone())
                .await;
            let retryable = match &result {
                CallResult::Failure(e) => matches!(
                    e,
                    LlmError::RateLimit(_) | LlmError::Network(_) | LlmError::Timeout { .. }
                ),
                _ => false,
            };
            if !retryable || attempt >= self.max_retries {
                return result;
            }
            let delay = self.base_delay * 2u32.pow(attempt);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            attempt += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::{ContentBlock, Message};
    use crate::providers::{CallResult, LlmProvider};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    #[tokio::test]
    async fn scripted_provider_returns_sequence_and_counts() {
        let p = ScriptedProvider::new(vec![text_response("a"), text_response("b")]);
        let msgs = vec![Message::user_text("x")];
        let r1 = p.stream_messages("", &msgs, &[], 100, cancel()).await;
        let r2 = p.stream_messages("", &msgs, &[], 100, cancel()).await;
        assert!(matches!(r1, CallResult::Success(ref s) if s.content[0] == ContentBlock::text("a")));
        assert!(matches!(r2, CallResult::Success(ref s) if s.content[0] == ContentBlock::text("b")));
        assert_eq!(p.calls(), 2);
        // 耗尽 → 显式失败。
        let r3 = p.stream_messages("", &msgs, &[], 100, cancel()).await;
        assert!(matches!(r3, CallResult::Failure(ref e) if e.to_string().contains("exhausted")));
    }

    #[test]
    fn tool_call_response_has_tool_call_finish_reason() {
        let r = tool_call_response("call_1", "read_file", serde_json::json!({"path": "README.md"}));
        assert_eq!(r.finish_reason, "tool_call");
        assert!(matches!(r.content[0], ContentBlock::ToolCall { .. }));
        let r2 = text_response("done");
        assert_eq!(r2.finish_reason, "stop");
    }

    #[tokio::test]
    async fn faulty_provider_classifies_injected_errors() {
        let p = FaultyProvider::new(vec![FaultStep::Fail(Fault::RateLimit)]);
        let r = p
            .stream_messages("", &[Message::user_text("x")], &[], 100, cancel())
            .await;
        assert!(matches!(r, CallResult::Failure(crate::error::LlmError::RateLimit(_))));
        assert_eq!(p.calls(), 1);
    }

    /// 验收 #4：注入「429 两次后成功」，断言重试正确工作且总调用次数 = 3，全程零网络。
    #[tokio::test]
    async fn retry_provider_survives_two_429s_with_three_total_calls() {
        let faulty = Arc::new(FaultyProvider::new(vec![
            FaultStep::Fail(Fault::RateLimit),
            FaultStep::Fail(Fault::RateLimit),
            FaultStep::Succeed(resp_with_usage("ok", 10, 5)),
        ]));
        let calls = faulty.call_counter();
        let retry = RetryProvider::new(faulty as Arc<dyn LlmProvider>, 3, Duration::ZERO);
        let r = retry
            .stream_messages("", &[Message::user_text("x")], &[], 100, cancel())
            .await;
        assert!(matches!(r, CallResult::Success(_)), "got: {r:?}");
        assert_eq!(calls.load(Ordering::Relaxed), 3, "1 initial + 2 retries = 3 total calls");
    }

    #[tokio::test]
    async fn retry_provider_does_not_retry_auth_errors() {
        let faulty = Arc::new(FaultyProvider::new(vec![
            FaultStep::Fail(Fault::AuthError),
            FaultStep::Succeed(text_response("never reached")),
        ]));
        let calls = faulty.call_counter();
        let retry = RetryProvider::new(faulty as Arc<dyn LlmProvider>, 3, Duration::ZERO);
        let r = retry
            .stream_messages("", &[Message::user_text("x")], &[], 100, cancel())
            .await;
        assert!(matches!(r, CallResult::Failure(crate::error::LlmError::AuthError(_))));
        assert_eq!(calls.load(Ordering::Relaxed), 1, "auth errors are not retryable");
    }

    #[tokio::test]
    async fn retry_provider_gives_up_after_budget() {
        let faulty = Arc::new(FaultyProvider::new(vec![FaultStep::Fail(Fault::RateLimit); 10]));
        let calls = faulty.call_counter();
        let retry = RetryProvider::new(faulty as Arc<dyn LlmProvider>, 2, Duration::ZERO);
        let r = retry
            .stream_messages("", &[Message::user_text("x")], &[], 100, cancel())
            .await;
        assert!(matches!(r, CallResult::Failure(crate::error::LlmError::RateLimit(_))));
        assert_eq!(calls.load(Ordering::Relaxed), 3, "1 initial + max_retries(2)");
    }
}
