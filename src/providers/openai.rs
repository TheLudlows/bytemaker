//! OpenAI Chat Completions client (via `async-openai`): request translation + stream
//! accumulation.
//!
//! The agent speaks a provider-neutral vocabulary internally (`Message`/`ContentBlock`
//! with `tool_call`/`tool_output`, `finish_reason == "tool_call"`), consumed by ~12 modules.
//! To keep that surface stable, this client is a **boundary translator**: it converts
//! the in-house request into an OpenAI chat-completions request, streams the response
//! via `async-openai`, and accumulates OpenAI stream deltas back into the same
//! `ContentBlock`/`finish_reason`/`Usage` vocabulary. The wire format is OpenAI; the
//! in-memory model is unchanged.
//!
//! Provides `Message`/`ContentBlock` (text/tool_call/tool_output, tagged by `type`),
//! `MessageBuilder`, `MessagesResponse` + `Usage`, and `Client::stream_messages`
//! returning `CallResult` (`Success`/`PromptTooLong`/`Failure`/`Cancelled`).
//!
//! Key invariants (preserved across the Anthropic→OpenAI migration):
//! - `finish_reason` defaults to `"unknown"`; a stream that ends without a terminal
//!   chunk (finish_reason/usage) returns `Failure` via the `got_terminal` sentinel
//!   (avoids next-turn API 400 from partial content).
//! - Two timeouts: `request_timeout` (create_stream headers, 120s) and
//!   `stream_idle_timeout` (inter-chunk gap, 60s) — prevents hung keep-alive proxies.
//! - 429/5xx retry: `max_retries=4`, exponential backoff capped 30s, `Retry-After`
//!   preferred; 400/401/403/404 not retried; sleep honors `CancellationToken`.
//! - Trailing `/` on `base_url` trimmed.
//!
//! Details: `docs/modules/client.md`.

use async_trait::async_trait;
use crate::domain::message::{ContentBlock, Message, MessagesResponse, Usage};
use crate::error::LlmError;
use crate::providers::{CallResult, LlmProvider};
use crate::tools::trait_def::ToolDefinition;
use async_openai::config::OpenAIConfig;
use async_openai::types::chat::ChatCompletionMessageToolCall;
use async_openai::types::chat::ChatCompletionMessageToolCalls;
use async_openai::types::chat::ChatCompletionRequestAssistantMessage;
use async_openai::types::chat::ChatCompletionRequestAssistantMessageContent;
use async_openai::types::chat::ChatCompletionRequestMessage;
use async_openai::types::chat::ChatCompletionRequestSystemMessage;
use async_openai::types::chat::ChatCompletionRequestSystemMessageContent;
use async_openai::types::chat::ChatCompletionRequestToolMessage;
use async_openai::types::chat::ChatCompletionRequestToolMessageContent;
use async_openai::types::chat::ChatCompletionRequestUserMessage;
use async_openai::types::chat::ChatCompletionRequestUserMessageContent;
use async_openai::types::chat::ChatCompletionStreamOptions;
use async_openai::types::chat::ChatCompletionTool;
use async_openai::types::chat::ChatCompletionTools;
use async_openai::types::chat::CreateChatCompletionRequest;
use async_openai::types::chat::FinishReason;
use async_openai::types::chat::FunctionCall;
use async_openai::types::chat::FunctionObject;
use async_openai::Client as OpenAIClient;
use futures_util::StreamExt;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Wraps OpenAI Chat Completions API interaction (boundary translator).
///
/// Holds an `async_openai::Client` built from `OpenAIConfig` (api_key + api_base).
/// The Anthropic-shaped in-memory types are translated to/from OpenAI wire shapes
/// inside `stream_messages`.
pub struct OpenAiProvider {
    openai: OpenAIClient<OpenAIConfig>,
    base_url: String,
    model: String,
    /// Hard cap on `create_stream` headers (connect + send + response headers). async-openai's
    /// internal retry backoff sleeps run within this budget.
    request_timeout: Duration,
    /// Max idle gap between stream chunks; on timeout the stream is judged failed,
    /// preventing keep-alive proxies that never return data from hanging the loop.
    stream_idle_timeout: Duration,
}

/// Default timeouts: connect 15s (slow corp-proxy handshake), request headers 120s, stream idle 60s.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

impl OpenAiProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self::with_timeouts(
            api_key,
            base_url,
            model,
            DEFAULT_CONNECT_TIMEOUT,
            DEFAULT_REQUEST_TIMEOUT,
            DEFAULT_STREAM_IDLE_TIMEOUT,
        )
    }

    /// Build `Client` with the given timeouts; tests pass small values to verify timeout
    /// paths fast. `connect_timeout` is accepted for API continuity but async-openai owns
    /// its HTTP client's connect timeout. 429/5xx retry is delegated to async-openai's
    /// built-in `OpenAIRetry` layer (max 3 retries, Retry-After preferred, backoff capped 8s).
    fn with_timeouts(
        api_key: String,
        base_url: String,
        model: String,
        _connect_timeout: Duration,
        request_timeout: Duration,
        stream_idle_timeout: Duration,
    ) -> Self {
        Self::with_config(
            api_key,
            base_url,
            model,
            request_timeout,
            stream_idle_timeout,
        )
    }

    /// Full-config build for tests (inject small timeouts to verify paths fast).
    fn with_config(
        api_key: String,
        base_url: String,
        model: String,
        request_timeout: Duration,
        stream_idle_timeout: Duration,
    ) -> Self {
        // Trailing '/' on base_url trimmed here: async-openai appends "/chat/completions".
        let api_base = base_url.trim_end_matches('/').to_string();
        let config = OpenAIConfig::new()
            .with_api_key(api_key)
            .with_api_base(api_base);
        let openai = OpenAIClient::with_config(config);
        Self {
            openai,
            base_url,
            model,
            request_timeout,
            stream_idle_timeout,
        }
    }

    /// Classify an AgentError into a CallResult (prompt_too_long → PromptTooLong, else Failure).
    fn classify_error(&self, err: LlmError) -> CallResult {
        if err.is_prompt_too_long() {
            CallResult::PromptTooLong(err)
        } else {
            CallResult::Failure(err)
        }
    }

    /// Format this turn's content blocks into a readable multi-line string for [resp] logs.
    /// Text blocks printed in full (the response body is what needs diagnosing, bounded by
    /// max_tokens); tool_call prints id/name/input. Unlike [req] truncation, responses want "full".
    fn format_response_for_log(content: &[ContentBlock]) -> String {
        let mut out = String::new();
        for (i, b) in content.iter().enumerate() {
            match b {
                ContentBlock::Text { text } => {
                    out.push_str(&format!("[resp] block[{}] text:\n", i));
                    out.push_str(text);
                    if !text.ends_with('\n') {
                        out.push('\n');
                    }
                }
                ContentBlock::ToolCall { id, name, input } => {
                    out.push_str(&format!(
                        "[resp] block[{}] tool_call({}: {}): {}\n",
                        i, id, name, input
                    ));
                }
                ContentBlock::ToolOutput { call_id, content } => {
                    out.push_str(&format!(
                        "[resp] block[{}] tool_output({}): {}\n",
                        i, call_id, content
                    ));
                }
            }
        }
        out.trim_end().to_string()
    }

    /// Stream /v1/messages.
    ///
    /// Accumulates text and tool_call input_json deltas, returning a `CallResult`.
    /// Does not print directly — returns `CallResult` after collecting all SSE events;
    /// the caller (run_loop) renders. `agent_loop` then checks finish_reason as usual.
    async fn stream_messages_impl(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        max_tokens: u32,
        cancel: CancellationToken,
    ) -> CallResult {
        // Build the OpenAI chat-completions request (boundary translation from the
        // provider-neutral in-memory types). `system` becomes a leading role:"system"
        // message; ContentBlock Text/ToolCall/ToolOutput map to OpenAI content/tool_calls/
        // role:"tool" messages; ToolDefinition maps to ChatCompletionTools::Function.
        let openai_messages = to_openai_messages(system, messages);
        let openai_tools = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .map(|t| ChatCompletionTools::Function(ChatCompletionTool {
                        function: FunctionObject {
                            name: t.name.clone(),
                            description: Some(t.description.clone()),
                            parameters: Some(t.input_schema.clone()),
                            strict: None,
                        },
                    }))
                    .collect::<Vec<_>>(),
            )
        };
        let request = CreateChatCompletionRequest {
            messages: openai_messages,
            model: self.model.clone(),
            max_completion_tokens: Some(max_tokens),
            stream: Some(true),
            stream_options: Some(ChatCompletionStreamOptions {
                include_usage: Some(true),
                include_obfuscation: None,
            }),
            tools: openai_tools,
            ..Default::default()
        };

        // Rough input size estimate for diagnosing context growth: system length +
        // text/tool_output body chars in history (ToolCall input is skipped — costly JSON,
        // not the main content).
        let system_chars = system.chars().count();
        let msg_chars: usize = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .map(|b| match b {
                ContentBlock::Text { text } => text.chars().count(),
                ContentBlock::ToolOutput { content, .. } => content.chars().count(),
                ContentBlock::ToolCall { .. } => 0,
            })
            .sum();
        tracing::info!(
            "[req] base_url={}, model={}, messages={}, tools={}, max_tokens={}, system_chars={}, input_chars={}",
            self.base_url,
            self.model,
            messages.len(),
            tools.len(),
            max_tokens,
            system_chars,
            system_chars + msg_chars
        );
        // Open the stream. Retries (429/5xx + connect errors, Retry-After preferred,
        // exponential backoff capped at 8s, max 3 retries) are handled by async-openai's
        // built-in `OpenAIRetry` layer — we do NOT layer a second retry loop on top (that
        // would multiply attempt counts). We still enforce two of our own invariants:
        //   - `request_timeout` caps the connect+headers phase (async-openai's retry sleeps
        //     run within this budget);
        //   - `cancel` aborts the whole open.
        // A stream opened here that ends before a terminal chunk is judged truncated below.
        let mut stream = {
            let chat = self.openai.chat();
            let create = chat.create_stream(request);
            match tokio::select! {
                biased;
                _ = cancel.cancelled() => return CallResult::Cancelled,
                r = tokio::time::timeout(self.request_timeout, create) => r,
            } {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => return self.classify_error(e.into()),
                Err(_) => {
                    return self.classify_error(LlmError::Stream(format!(
                        "request timed out after {:?} (connect+headers)",
                        self.request_timeout
                    )))
                }
            }
        };

        // Accumulate OpenAI stream chunks into the provider-neutral ContentBlock vocabulary.
        let mut content: Vec<ContentBlock> = Vec::new();
        // Init to "unknown" rather than empty: if the stream ends abnormally with no
        // terminal finish_reason, an empty string would silently exit the loop via
        // `"" != "tool_call"`, masking a protocol error. "unknown" likewise skips the tool
        // branch but leaves a recognizable sentinel for upstream diagnosis.
        let mut finish_reason = String::from("unknown");
        // A stream must reach a terminal chunk (one carrying finish_reason, or the final
        // usage chunk) to count as complete. If it ends before that (network truncation /
        // proxy early-close / server crash), returning Success with "unknown" or partial
        // content would force an error next turn. Use a sentinel and judge Failure on break.
        let mut got_terminal = false;

        // OpenAI streams text as `delta.content` and tool calls as `delta.tool_calls[]`
        // (indexed, to support parallel calls). Accumulate each separately and flush into
        // `content` (text first, then tool_call blocks) after the stream ends.
        let mut tool_by_index: std::collections::BTreeMap<u32, (String, String, String)> =
            std::collections::BTreeMap::new();
        let mut text_buf = String::new();
        // Token usage: with stream_options.include_usage, the final chunk carries the
        // total usage (prompt_tokens=input, completion_tokens=output). Earlier chunks
        // carry a null usage. Some gateways omit usage entirely; stays None, logged as `?`.
        let mut input_tokens: Option<u64> = None;
        let mut output_tokens: Option<u64> = None;

        loop {
            // Idle cap between chunks: a keep-alive-but-no-data stream must be judged
            // Failure within the deadline, else stream.next() hangs forever. Rebuilding
            // the timeout each turn gives "chunk-idle" semantics — any chunk resets the timer.
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => return CallResult::Cancelled,
                ev = tokio::time::timeout(self.stream_idle_timeout, stream.next()) => match ev {
                    Ok(Some(Ok(c))) => c,
                    Ok(Some(Err(e))) => return self.classify_error(e.into()),
                    Ok(None) => break,
                    Err(_) => return self.classify_error(LlmError::Stream(format!(
                        "stream idle timeout: no chunk within {:?}",
                        self.stream_idle_timeout
                    ))),
                },
            };

            // Final usage chunk (choices empty) — the OpenAI analog of the closing event.
            if let Some(usage) = chunk.usage {
                input_tokens = Some(usage.prompt_tokens as u64);
                output_tokens = Some(usage.completion_tokens as u64);
            }

            for choice in &chunk.choices {
                let delta = &choice.delta;

                // Text deltas → accumulate into the text buffer.
                if let Some(t) = delta.content.as_deref() {
                    if !t.is_empty() {
                        text_buf.push_str(t);
                    }
                }

                // Tool-call deltas → accumulate by index (id/name arrive on the first
                // chunk for that index; arguments stream in across subsequent chunks).
                if let Some(calls) = delta.tool_calls.as_ref() {
                    for call in calls {
                        let entry = tool_by_index
                            .entry(call.index)
                            .or_insert_with(|| (String::new(), String::new(), String::new()));
                        if let Some(id) = call.id.as_deref() {
                            if !id.is_empty() {
                                entry.0 = id.to_string();
                            }
                        }
                        if let Some(func) = call.function.as_ref() {
                            if let Some(name) = func.name.as_deref() {
                                if !name.is_empty() {
                                    entry.1 = name.to_string();
                                }
                            }
                            if let Some(args) = func.arguments.as_deref() {
                                entry.2.push_str(args);
                            }
                        }
                    }
                }

                // finish_reason marks the terminal chunk for this choice.
                if let Some(fr) = choice.finish_reason {
                    got_terminal = true;
                    finish_reason = match fr {
                        FinishReason::ToolCalls | FinishReason::FunctionCall => "tool_call".into(),
                        FinishReason::Stop => "stop".into(),
                        FinishReason::Length => "max_tokens".into(),
                        FinishReason::ContentFilter => "content_filter".into(),
                    };
                }
            }
        }

        // Flush accumulated text (if any) as a Text block, then tool_call blocks by index.
        if !text_buf.is_empty() {
            content.push(ContentBlock::Text { text: text_buf });
        }
        for (_idx, (id, name, partial_json)) in tool_by_index {
            let input: serde_json::Value = if partial_json.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_str(&partial_json).unwrap_or(serde_json::Value::Null)
            };
            content.push(ContentBlock::ToolCall { id, name, input });
        }

        // Tally block types and collect this turn's tool names for a clear view of what the model did.
        let text_blocks = content
            .iter()
            .filter(|b| matches!(b, ContentBlock::Text { .. }))
            .count();
        let tool_names: Vec<&str> = content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        tracing::info!(
            "[resp] finish_reason={}, blocks={}, text={}, tool_call={}, tools=[{}], input_tokens={}, output_tokens={}",
            finish_reason,
            content.len(),
            text_blocks,
            tool_names.len(),
            tool_names.join(","),
            input_tokens.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
            output_tokens.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
        );
        tracing::info!("[resp] content detail:\n{}", Self::format_response_for_log(&content));

        // Stream ended without a terminal chunk → truncated; judge Failure, not Success
        // (partial content already logged above via [resp] for diagnosis).
        if !got_terminal {
            return self.classify_error(LlmError::Stream(format!(
                "stream ended without a terminal chunk (finish_reason={}, blocks={}) — possible truncation",
                finish_reason,
                content.len()
            )));
        }

        let usage = match (input_tokens, output_tokens) {
            (Some(i), Some(o)) => Some(Usage { input_tokens: i, output_tokens: o }),
            (Some(i), None) => Some(Usage { input_tokens: i, output_tokens: 0 }),
            (None, Some(o)) => Some(Usage { input_tokens: 0, output_tokens: o }),
            (None, None) => None,
        };
        CallResult::Success(MessagesResponse { content, finish_reason, usage })
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn stream_messages(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        max_tokens: u32,
        cancel: CancellationToken,
    ) -> CallResult {
        self.stream_messages_impl(system, messages, tools, max_tokens, cancel).await
    }
}

/// Translate the in-house provider-neutral request into OpenAI chat-completion messages.
///
/// - `system: &str` → a leading `role:"system"` message.
/// - `Message { role:"user", [Text] }` → `role:"user"` with text content.
/// - `Message { role:"user", [ToolOutput] }` → one `role:"tool"` message per ToolOutput.
/// - `Message { role:"assistant", [Text, ToolCall] }` → `role:"assistant"` with joined
///   text content + `tool_calls` (function name + JSON-stringified input).
///
/// Mixed assistant blocks (text + tool_call) map to OpenAI's `content` + `tool_calls`.
fn to_openai_messages(system: &str, messages: &[Message]) -> Vec<ChatCompletionRequestMessage> {
    let mut out: Vec<ChatCompletionRequestMessage> = Vec::with_capacity(messages.len() + 1);

    // System prompt as a leading system message (OpenAI has no top-level system field).
    if !system.is_empty() {
        out.push(ChatCompletionRequestMessage::System(
            ChatCompletionRequestSystemMessage {
                content: ChatCompletionRequestSystemMessageContent::Text(system.to_string()),
                name: None,
            },
        ));
    }

    for msg in messages {
        match msg.role.as_str() {
            "user" => {
                // Split user blocks: Text blocks fold into one user message; each ToolOutput
                // becomes its own role:"tool" message (OpenAI requires one tool message
                // per tool_call_id).
                let mut text_parts: Vec<String> = Vec::new();
                for b in &msg.content {
                    match b {
                        ContentBlock::Text { text } => text_parts.push(text.clone()),
                        ContentBlock::ToolOutput {
                            call_id,
                            content,
                        } => {
                            out.push(ChatCompletionRequestMessage::Tool(
                                ChatCompletionRequestToolMessage {
                                    content: ChatCompletionRequestToolMessageContent::Text(
                                        content.clone(),
                                    ),
                                    tool_call_id: call_id.clone(),
                                },
                            ));
                        }
                        ContentBlock::ToolCall { .. } => {}
                    }
                }
                if !text_parts.is_empty() {
                    out.push(ChatCompletionRequestMessage::User(
                        ChatCompletionRequestUserMessage {
                            content: ChatCompletionRequestUserMessageContent::Text(
                                text_parts.join("\n"),
                            ),
                            name: None,
                        },
                    ));
                }
            }
            "assistant" => {
                let mut text_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<ChatCompletionMessageToolCalls> = Vec::new();
                for b in &msg.content {
                    match b {
                        ContentBlock::Text { text } => text_parts.push(text.clone()),
                        ContentBlock::ToolCall { id, name, input } => {
                            tool_calls.push(ChatCompletionMessageToolCalls::Function(
                                ChatCompletionMessageToolCall {
                                    id: id.clone(),
                                    function: FunctionCall {
                                        name: name.clone(),
                                        arguments: input.to_string(),
                                    },
                                },
                            ));
                        }
                        ContentBlock::ToolOutput { .. } => {}
                    }
                }
                let content = if text_parts.is_empty() {
                    None
                } else {
                    Some(ChatCompletionRequestAssistantMessageContent::Text(
                        text_parts.join("\n"),
                    ))
                };
                let tool_calls = if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                };
                out.push(ChatCompletionRequestMessage::Assistant(
                    ChatCompletionRequestAssistantMessage {
                        content,
                        tool_calls,
                        ..Default::default()
                    },
                ));
            }
            _ => {
                // Unknown role: best-effort pass-through as a user message of joined text.
                let text: String = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    out.push(ChatCompletionRequestMessage::User(
                        ChatCompletionRequestUserMessage {
                            content: ChatCompletionRequestUserMessageContent::Text(text),
                            name: None,
                        },
                    ));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid OpenAI streaming chunk the mock server can emit. Parsed by
    /// async-openai without error; carries no finish_reason so the loop keeps waiting.
    fn openai_chunk(delta_content: Option<&str>) -> String {
        let content = match delta_content {
            Some(c) => format!(",\"content\":{:?}", c),
            None => String::new(),
        };
        format!(
            "{{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\"{}}},\"finish_reason\":null}}]}}\n\n",
            content
        )
    }

    /// Server accepts TCP but never returns headers: `create_stream()` must fail within
    /// `request_timeout` instead of hanging the agent loop.
    #[tokio::test]
    async fn stream_messages_fails_on_request_timeout_when_server_never_responds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            // Accept the connection but never reply with a byte: create_stream waits for headers forever.
            let (_sock, _peer) = listener.accept().await.expect("accept");
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let base_url = format!("http://{}", addr);
        let client = OpenAiProvider::with_timeouts(
            "key".into(),
            base_url,
            "model".into(),
            Duration::from_secs(15),
            Duration::from_millis(200),
            Duration::from_secs(60),
        );
        let cancel = CancellationToken::new();
        // Outer sentinel: stream_messages must return within 5s; the 200ms request
        // timeout should yield Failure quickly.
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.stream_messages("sys", &[], &[], 100, cancel),
        )
        .await;
        let inner = result.expect("stream_messages hung — request timeout not applied (P0-1)");
        assert!(
            matches!(inner, CallResult::Failure(_)),
            "expected Failure, got {:?}",
            inner
        );
    }

    /// Server returns headers + one valid chunk then stalls (keep-alive, no data):
    /// `stream.next()` must fail within `stream_idle_timeout`, not hang forever.
    #[tokio::test]
    async fn stream_messages_fails_on_idle_timeout_when_stream_stalls() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let (mut sock, _peer) = listener.accept().await.expect("accept");
            // Read the request line/headers, reply 200 + one valid chunk, then stall:
            // the stream parses one event but never returns the next chunk.
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {}",
                openai_chunk(Some("hi"))
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let base_url = format!("http://{}", addr);
        let client = OpenAiProvider::with_timeouts(
            "key".into(),
            base_url,
            "model".into(),
            Duration::from_secs(15),
            Duration::from_secs(5),
            Duration::from_millis(200),
        );
        let cancel = CancellationToken::new();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.stream_messages("sys", &[], &[], 100, cancel),
        )
        .await;
        let inner = result.expect("stream_messages hung — idle timeout not applied (P0-1)");
        assert!(
            matches!(inner, CallResult::Failure(_)),
            "expected Failure, got {:?}",
            inner
        );
    }

    /// External `cancel.cancel()` mid-stream: `stream_messages` must return
    /// `CallResult::Cancelled`.
    #[tokio::test]
    async fn stream_messages_returns_cancelled_when_token_cancelled_mid_stream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let (mut sock, _peer) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            // Reply 200 + one valid chunk then stall: the stream blocks on next() until cancel fires.
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {}",
                openai_chunk(Some("hi"))
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let base_url = format!("http://{}", addr);
        let client = OpenAiProvider::with_timeouts(
            "key".into(),
            base_url,
            "model".into(),
            Duration::from_secs(15),
            Duration::from_secs(5),
            Duration::from_secs(60),
        );
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel_for_task.cancel();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.stream_messages("sys", &[], &[], 100, cancel),
        )
        .await;
        let inner = result.expect("stream_messages hung — cancel not honoured (P0-2)");
        assert!(
            matches!(inner, CallResult::Cancelled),
            "expected Cancelled, got {:?}",
            inner
        );
    }

    /// Truncated stream (ends before a terminal finish_reason chunk) must return
    /// Failure, not Success with "unknown"/partial content (else next turn errors).
    #[tokio::test]
    async fn stream_messages_returns_failure_when_stream_ends_without_terminal_chunk() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let (mut sock, _peer) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            // One text chunk but no terminal finish_reason before close → truncated.
            let body = format!("data: {}", openai_chunk(Some("hi")));
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        let base_url = format!("http://{}", addr);
        let client = OpenAiProvider::with_config(
            "key".into(),
            base_url,
            "model".into(),
            Duration::from_secs(5),
            Duration::from_secs(60),
        );
        let cancel = CancellationToken::new();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.stream_messages("sys", &[], &[], 100, cancel),
        )
        .await;
        let inner = result.expect("stream_messages hung (P0-3)");
        assert!(
            matches!(inner, CallResult::Failure(_)),
            "expected Failure on truncated stream, got {:?}",
            inner
        );
    }

    /// 429 rate-limited: async-openai's built-in retry layer backs off and retries;
    /// the attempt that returns 200 + a terminal OpenAI stream succeeds. The 429 bodies
    /// must be valid `{"error":{...}}` JSON so async-openai maps them to ApiError(429) (retryable).
    #[tokio::test]
    async fn stream_messages_retries_429_then_succeeds() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let attempts = std::sync::Arc::new(AtomicU32::new(0));
        let attempts_for_task = attempts.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _peer) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let n = attempts_for_task.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                if n < 2 {
                    // First two: 429 with a valid error body (so it maps to ApiError(429)).
                    let body = "{\"error\":{\"message\":\"rate limited\",\"type\":\"rate_limit\",\"param\":null,\"code\":null}}";
                    let resp = format!(
                        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                } else {
                    // 3rd attempt: 200 + full OpenAI stream (text chunk + terminal finish_reason
                    // chunk + usage chunk + [DONE]).
                    let chunks = format!(
                        "data: {c1}data: {c2}data: {c3}data: [DONE]\n\n",
                        c1 = openai_chunk(Some("ok")),
                        c2 = "{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        c3 = "{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                    );
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        chunks.len(),
                        chunks
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                }
            }
        });
        let base_url = format!("http://{}", addr);
        let client = OpenAiProvider::with_config(
            "key".into(),
            base_url,
            "model".into(),
            Duration::from_secs(5),
            Duration::from_secs(60),
        );
        let cancel = CancellationToken::new();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.stream_messages("sys", &[], &[], 100, cancel),
        )
        .await;
        let inner = result.expect("stream_messages hung (P0-4 succeed)");
        match inner {
            CallResult::Success(r) => {
                assert_eq!(r.finish_reason.as_str(), "stop", "finish_reason after retry");
                assert_eq!(r.content.len(), 1, "one text block expected");
            }
            other => panic!("expected Success after retry, got {:?}", other),
        }
        // async-openai retries the 429s internally; the attempt that returns 200 (n>=2)
        // succeeds. So at least 3 connections were made (2 rate-limits + the success).
        assert!(
            attempts.load(Ordering::SeqCst) >= 3,
            "expected >=3 attempts (retries then success), got {}",
            attempts.load(Ordering::SeqCst)
        );
    }

    /// Persistent 429: after async-openai's retries exhaust (1 initial + 3 retries = 4
    /// attempts), returns Failure(Api 429) — no infinite retry.
    #[tokio::test]
    async fn stream_messages_returns_failure_after_retries_exhausted_on_429() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let attempts = std::sync::Arc::new(AtomicU32::new(0));
        let attempts_for_task = attempts.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _peer) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                attempts_for_task.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                // No Retry-After → exponential backoff (base=1ms); valid error body → ApiError(429).
                let body = "{\"error\":{\"message\":\"rate limited\",\"type\":\"rate_limit\",\"param\":null,\"code\":null}}";
                let resp = format!(
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        let base_url = format!("http://{}", addr);
        let client = OpenAiProvider::with_config(
            "key".into(),
            base_url,
            "model".into(),
            Duration::from_secs(5),
            Duration::from_secs(60),
        );
        let cancel = CancellationToken::new();
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            client.stream_messages("sys", &[], &[], 100, cancel),
        )
        .await;
        let inner = result.expect("stream_messages hung (P0-4 exhaust)");
        match inner {
            CallResult::Failure(e) => {
                assert!(
                    matches!(e, LlmError::RateLimit(_)),
                    "expected LlmError::RateLimit after retries exhausted, got {:?}",
                    e
                );
            }
            other => panic!("expected Failure after retries exhausted, got {:?}", other),
        }
        // async-openai default OpenAIRetry = 3 retries → 1 + 3 = 4 attempts (no infinite retry).
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            4,
            "expected 4 total attempts (1 + 3 retries), got {}",
            attempts.load(Ordering::SeqCst)
        );
    }
}
