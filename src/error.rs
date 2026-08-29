//! Unified error types for the agent.
//!
//! - `AgentError`: the app-level error returned across nearly every module (LLM API,
//!   tool, file-system, validation). Replaces `Box<dyn Error>` and ad-hoc strings.
//! - `LlmError`: the provider-layer domain error (docs §2.3). The anti-corruption layer
//!   maps `async_openai::error::OpenAIError` → `LlmError` (status-classified: 429→RateLimit,
//!   401/403→AuthError, prompt-too-long body→PromptTooLong). `LlmError` bridges into
//!   `AgentError` via `From<LlmError> for AgentError`, so upper layers returning
//!   `Result<_, AgentError>` are unchanged.
//!
//! Prompt-too-long detection is O(1) on the `LlmError::PromptTooLong` variant (classified
//! once in `LlmError::from_api_error`), consumed via `providers::CallResult::is_prompt_too_long`
//! → `agent::run_loop`'s reactive-compaction retry. `AgentError::is_prompt_too_long` is a
//! legacy body-substring matcher retained for API/tests; production detection uses the variant.
//!
//! See `docs/modules/error.md`.

/// Unified error type for the agent.
///
/// Variants:
/// - `Api`: HTTP-level error from the LLM provider (non-2xx status).
/// - `Network`: Transport / connection error (DNS, TLS, timeout at TCP level).
/// - `Stream`: SSE stream-level error (protocol violation, server-sent error event).
/// - `Timeout`: Operation exceeded its deadline.
/// - `InvalidResponse`: Malformed response (bad JSON, missing fields).
/// - `ToolNotFound`: Tool was not found in the registry.
/// - `ToolRejected`: Tool was rejected (e.g., in subagent context).
/// - `ToolDenied`: Tool execution was denied by a pre-tool hook.
/// - `ToolExecution`: Tool execution failed with a specific error message.
/// - `PathTraversal`: Path attempt to escape the workspace.
/// - `FileSystem`: File system related error.
/// - `Validation`: Input validation failed.
/// - `Other`: Catch-all for errors not yet classified (io, config, etc.).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AgentError {
    /// HTTP error from the LLM API (non-2xx response).
    #[error("API error (HTTP {status}): {body}")]
    Api { status: u16, body: String },

    /// Network / transport error.
    #[error("Network error: {0}")]
    Network(String),

    /// SSE stream-level error.
    #[error("Stream error: {0}")]
    Stream(String),

    /// Operation timed out.
    #[error("Operation timed out after {seconds}s")]
    Timeout { seconds: u64 },

    /// Invalid / malformed response.
    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    /// Tool was not found in the registry.
    #[error("Tool '{name}' not found. Available tools: {available}")]
    ToolNotFound { name: String, available: String },

    /// Tool was rejected (e.g., in subagent context).
    #[error("Tool '{name}' rejected: {reason}")]
    ToolRejected { name: String, reason: String },

    /// Tool execution was denied by a pre-tool hook.
    #[error("Tool '{name}' denied: {reason}")]
    ToolDenied { name: String, reason: String },

    /// Tool execution failed with a specific error message.
    #[error("Tool '{name}' execution failed: {reason}")]
    ToolExecution { name: String, reason: String },

    /// Path attempt to escape the workspace.
    #[error("Path '{path}' escapes workspace")]
    PathTraversal { path: String },

    /// File system related error.
    #[error("File system error: {0}")]
    FileSystem(String),

    /// Input validation failed.
    #[error("Validation error: {0}")]
    Validation(String),

    /// Catch-all.
    #[error("{0}")]
    Other(String),
}

impl AgentError {
    /// Check if this error indicates the prompt was too long for the model's context.
    ///
    /// Used by the agent loop to trigger reactive compaction: when the API rejects
    /// a request due to context length, we can summarize history and retry once.
    pub fn is_prompt_too_long(&self) -> bool {
        let msg = match self {
            Self::Api { body, .. } => body.to_lowercase(),
            Self::Stream(msg) => msg.to_lowercase(),
            Self::Other(msg) => msg.to_lowercase(),
            _ => return false,
        };
        // Anthropic wording + OpenAI wording; case-insensitive.
        msg.contains("prompt_too_long")
            || msg.contains("too many tokens")
            || msg.contains("request_too_large")
            || msg.contains("context_length_exceeded")
            || msg.contains("maximum context length")
            || msg.contains("reduce the length of the messages")
    }
}


impl From<std::io::Error> for AgentError {
    fn from(e: std::io::Error) -> Self {
        Self::FileSystem(e.to_string())
    }
}

/// 领域错误：LLM 供应商层（docs §2.3）。防腐层把 SDK 错误映射到这里；
/// 上层经 `From<LlmError> for AgentError` 桥接到 `AgentError`。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LlmError {
    #[error("Auth error: {0}")]
    AuthError(String),
    #[error("Rate limited: {0}")]
    RateLimit(String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("Timeout after {seconds}s")]
    Timeout { seconds: u64 },
    #[error("Stream error: {0}")]
    Stream(String),
    #[error("Invalid response: {0}")]
    InvalidResponse(String),
    #[error("Prompt too long: {0}")]
    PromptTooLong(String),
    #[error("{0}")]
    Other(String),
}

impl LlmError {
    /// 把 HTTP ApiError（status+body）分类为 `LlmError`。prompt-too-long 子串匹配集中在此。
    pub fn from_api_error(status: u16, body: &str) -> Self {
        let lower = body.to_lowercase();
        let too_long = lower.contains("prompt_too_long")
            || lower.contains("too many tokens")
            || lower.contains("request_too_large")
            || lower.contains("context_length_exceeded")
            || lower.contains("maximum context length")
            || lower.contains("reduce the length of the messages");
        if status == 429 {
            LlmError::RateLimit(body.to_string())
        } else if status == 401 || status == 403 {
            LlmError::AuthError(body.to_string())
        } else if (status == 400 || status == 413) && too_long {
            LlmError::PromptTooLong(body.to_string())
        } else {
            LlmError::Other(format!("HTTP {}: {}", status, body))
        }
    }

    /// O(1) 变体判定（分类已在 `from_api_error` 完成）。
    pub fn is_prompt_too_long(&self) -> bool {
        matches!(self, Self::PromptTooLong(_))
    }
}

impl From<async_openai::error::OpenAIError> for LlmError {
    /// 与既有 `From<OpenAIError> for AgentError` 同构，但落点为 `LlmError`；
    /// ApiError 走 `from_api_error` 按 status 细分。
    fn from(e: async_openai::error::OpenAIError) -> Self {
        use async_openai::error::OpenAIError;
        match e {
            OpenAIError::ApiError(resp) => {
                let status = resp.status_code.as_u16();
                let body = resp.api_error.to_string();
                LlmError::from_api_error(status, &body)
            }
            OpenAIError::Reqwest(e) => LlmError::Network(e.to_string()),
            OpenAIError::StreamError(e) => LlmError::Stream(e.to_string()),
            OpenAIError::JSONDeserialize(err, raw) => {
                LlmError::InvalidResponse(format!("JSON error: {} (raw: {})", err, raw))
            }
            OpenAIError::InvalidArgument(msg) => LlmError::Other(msg),
            other => LlmError::Other(other.to_string()),
        }
    }
}

impl From<LlmError> for AgentError {
    /// 桥接：使上层 `Result<_, AgentError>` 的 `?` 与 `CallResult::into_response()` 不变。
    fn from(e: LlmError) -> Self {
        match e {
            LlmError::AuthError(msg) => AgentError::Api { status: 401, body: msg },
            LlmError::RateLimit(msg) => AgentError::Api { status: 429, body: msg },
            LlmError::Network(s) => AgentError::Network(s),
            LlmError::Stream(s) => AgentError::Stream(s),
            LlmError::Timeout { seconds } => AgentError::Timeout { seconds },
            LlmError::InvalidResponse(s) => AgentError::InvalidResponse(s),
            LlmError::PromptTooLong(s) => AgentError::Api { status: 400, body: s },
            LlmError::Other(s) => AgentError::Other(s),
        }
    }
}

impl From<std::env::VarError> for AgentError {
    fn from(e: std::env::VarError) -> Self {
        Self::Validation(format!("Environment variable error: {}", e))
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(e: serde_json::Error) -> Self {
        Self::Validation(format!("JSON error: {}", e))
    }
}

impl From<Box<dyn std::error::Error>> for AgentError {
    fn from(e: Box<dyn std::error::Error>) -> Self {
        Self::Other(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_api_error() {
        let err = AgentError::Api {
            status: 429,
            body: "rate limited".into(),
        };
        assert_eq!(err.to_string(), "API error (HTTP 429): rate limited");
    }

    #[test]
    fn display_timeout() {
        let err = AgentError::Timeout { seconds: 30 };
        assert_eq!(err.to_string(), "Operation timed out after 30s");
    }

    #[test]
    fn display_stream_error() {
        let err = AgentError::Stream("bad JSON".into());
        assert_eq!(err.to_string(), "Stream error: bad JSON");
    }

    #[test]
    fn display_other() {
        let err = AgentError::Other("something went wrong".into());
        assert_eq!(err.to_string(), "something went wrong");
    }

    #[test]
    fn from_io_error() {
        // From<io::Error> maps io errors to FileSystem; the test asserts on that.
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err: AgentError = io_err.into();
        assert!(matches!(err, AgentError::FileSystem(_)));
        assert!(err.to_string().contains("file missing"));
    }

    #[test]
    fn from_boxed_error() {
        let boxed: Box<dyn std::error::Error> = "generic failure".into();
        let err: AgentError = boxed.into();
        assert!(matches!(err, AgentError::Other(_)));
        assert!(err.to_string().contains("generic failure"));
    }

    #[test]
    fn is_prompt_too_long_api_variant() {
        let err = AgentError::Api {
            status: 400,
            body: "prompt_too_long: request exceeds maximum context".into(),
        };
        assert!(err.is_prompt_too_long());

        let err2 = AgentError::Api {
            status: 400,
            body: "too many tokens in request".into(),
        };
        assert!(err2.is_prompt_too_long());

        let err3 = AgentError::Api {
            status: 400,
            body: "request_too_large".into(),
        };
        assert!(err3.is_prompt_too_long());

        // Case insensitive
        let err4 = AgentError::Api {
            status: 400,
            body: "PROMPT_TOO_LONG".into(),
        };
        assert!(err4.is_prompt_too_long());
    }

    #[test]
    fn is_prompt_too_long_openai_wording() {
        // OpenAI context-length error wording.
        let openai_variants = [
            "This model's maximum context length is 8192 tokens",
            "context_length_exceeded",
            "Please reduce the length of the messages.",
        ];
        for body in openai_variants {
            let err = AgentError::Api {
                status: 400,
                body: body.into(),
            };
            assert!(err.is_prompt_too_long(), "expected prompt-too-long for: {body}");
        }
    }

    #[test]
    fn is_prompt_too_long_negative() {
        let err = AgentError::Api {
            status: 401,
            body: "unauthorized".into(),
        };
        assert!(!err.is_prompt_too_long());

        let err2 = AgentError::Stream("connection closed".into());
        assert!(!err2.is_prompt_too_long());

        let err3 = AgentError::Timeout { seconds: 30 };
        assert!(!err3.is_prompt_too_long());

        let err4 = AgentError::Other("some other error".into());
        assert!(!err4.is_prompt_too_long());
    }

    #[test]
    fn llm_error_classifies_429_as_rate_limit() {
        assert!(matches!(
            LlmError::from_api_error(429, "rate limited"),
            LlmError::RateLimit(_)
        ));
    }

    #[test]
    fn llm_error_classifies_401_403_as_auth() {
        assert!(matches!(LlmError::from_api_error(401, "no key"), LlmError::AuthError(_)));
        assert!(matches!(LlmError::from_api_error(403, "forbidden"), LlmError::AuthError(_)));
    }

    #[test]
    fn llm_error_classifies_prompt_too_long_body() {
        for body in ["prompt_too_long", "too many tokens", "context_length_exceeded"] {
            assert!(
                matches!(LlmError::from_api_error(400, body), LlmError::PromptTooLong(_)),
                "expected PromptTooLong for: {body}"
            );
        }
    }

    #[test]
    fn llm_error_classifies_other_status_as_other() {
        assert!(matches!(LlmError::from_api_error(500, "boom"), LlmError::Other(_)));
    }

    #[test]
    fn llm_error_is_prompt_too_long_variant_check() {
        assert!(LlmError::PromptTooLong("x".into()).is_prompt_too_long());
        assert!(!LlmError::RateLimit("x".into()).is_prompt_too_long());
        assert!(!LlmError::Other("x".into()).is_prompt_too_long());
    }

    #[test]
    fn from_llm_error_to_agent_error_bridge() {
        // Bridge maps each LlmError variant to the corresponding AgentError shape.
        // Note: prompt-too-long *detection* is NOT preserved across the bridge —
        // AgentError::is_prompt_too_long() is a body-substring matcher, and the bridge
        // carries an arbitrary body. After the refactor, prompt-too-long is detected via
        // the CallResult/LlmError *variant* (checked before bridging), so the bridge only
        // needs to preserve the error shape for reporting/propagation.
        assert!(matches!(
            AgentError::from(LlmError::RateLimit("rl".into())),
            AgentError::Api { status: 429, .. }
        ));
        assert!(matches!(
            AgentError::from(LlmError::AuthError("au".into())),
            AgentError::Api { status: 401, .. }
        ));
        assert!(matches!(
            AgentError::from(LlmError::PromptTooLong("pl".into())),
            AgentError::Api { status: 400, .. }
        ));
        assert!(matches!(
            AgentError::from(LlmError::Network("n".into())),
            AgentError::Network(_)
        ));
    }

}
