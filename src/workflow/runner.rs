//! Workflow agent runners. `agent()` is a single-shot LLM call (no tool loop),
//! mirroring s16's OpenAIAgentRunner / MockAgentRunner.

use crate::domain::message::{ContentBlock, Message, MessagesResponse};
use crate::providers::{CallResult, LlmProvider};
use crate::error::AgentError;
use crate::workflow::schema::fill_schema;
use crate::workflow::WorkflowError;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct RunnerOutput {
    pub value: Value,
    pub tokens: u64,
}

#[async_trait]
pub trait WorkflowRunner: Send + Sync {
    async fn run(
        &self,
        prompt: &str,
        schema: Option<&Value>,
        label: Option<&str>,
    ) -> Result<RunnerOutput, WorkflowError>;
}

/// Deterministic runner for demos and tests (no API key).
pub struct MockRunner;

#[async_trait]
impl WorkflowRunner for MockRunner {
    async fn run(
        &self,
        prompt: &str,
        schema: Option<&Value>,
        label: Option<&str>,
    ) -> Result<RunnerOutput, WorkflowError> {
        let label = label.unwrap_or("audit");
        let value = match schema.and_then(|s| s.get("properties")) {
            Some(props) if props.get("findings").is_some() => {
                let n = 1 + (crate::workflow::ids::stable_hash(prompt) % 2);
                let sev = ["high", "medium", "low"];
                let findings: Vec<Value> = (0..n)
                    .map(|i| {
                        serde_json::json!({
                            "title": format!("{label} #{}", i + 1),
                            "severity": sev[(crate::workflow::ids::stable_hash(&format!("{prompt}{i}")) % 3) as usize],
                        })
                    })
                    .collect();
                serde_json::json!({ "findings": findings })
            }
            Some(props) if props.get("isReal").is_some() => {
                let real = crate::workflow::ids::stable_hash(prompt) % 4 != 0;
                serde_json::json!({
                    "isReal": real,
                    "reason": if real { "reproduced" } else { "could not reproduce" },
                })
            }
            _ => match schema {
                Some(s) => fill_schema(s, prompt),
                None => {
                    let truncated: String = prompt.chars().take(60).collect();
                    Value::String(format!("[mock] {truncated}"))
                }
            },
        };
        let tokens = (prompt.len() / 4) as u64
            + (serde_json::to_string(&value).unwrap().len() / 4) as u64;
        Ok(RunnerOutput { value, tokens })
    }
}

/// Real runner: single-shot LLM call through the host's API client.
pub struct OpenAIRunner {
    client: Arc<dyn LlmProvider>,
}

impl OpenAIRunner {
    pub fn new(client: Arc<dyn LlmProvider>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl WorkflowRunner for OpenAIRunner {
    async fn run(
        &self,
        prompt: &str,
        schema: Option<&Value>,
        _label: Option<&str>,
    ) -> Result<RunnerOutput, WorkflowError> {
        let mut request = prompt.to_string();
        if let Some(s) = schema {
            request.push_str("\n\nReturn only one JSON object matching this schema:\n");
            request.push_str(&serde_json::to_string(s)?);
        }
        let messages = vec![Message::user_text(&request)];
        let cancel = CancellationToken::new();
        let result = self
            .client
            .stream_messages(
                "You are a focused workflow agent. Complete only the supplied step. Do not \
                 claim access to files or results not included in the prompt.",
                &messages,
                &[],
                2000,
                cancel,
            )
            .await;
        let resp: MessagesResponse = match result {
            CallResult::Success(r) => r,
            CallResult::PromptTooLong(e) | CallResult::Failure(e) => {
                return Err(WorkflowError::Agent(e.into()));
            }
            CallResult::Cancelled => {
                return Err(WorkflowError::Agent(AgentError::Stream("cancelled".into())));
            }
        };
        let tokens = resp
            .usage
            .as_ref()
            .map(|u| u.input_tokens + u.output_tokens)
            .unwrap_or(0);
        let text: String = resp
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let value = if schema.is_some() {
            // On parse failure, return the raw text so agent()'s schema
            // validation catches it and triggers the one-time retry
            // (mirrors s16's OpenAIAgentRunner — do NOT propagate here).
            parse_json(&text).unwrap_or_else(|_| Value::String(text.clone()))
        } else {
            Value::String(text)
        };
        Ok(RunnerOutput { value, tokens })
    }
}

/// Parse JSON from a model text response: strips ``` fences, then tries the whole
/// string, then scans for the first `{` that parses as a JSON value.
fn parse_json(text: &str) -> Result<Value, String> {
    let s = text.trim();
    let s = if s.starts_with("```") {
        let mut lines: Vec<&str> = s.lines().collect();
        if !lines.is_empty() {
            lines.remove(0);
        }
        if lines.last().map(|l| l.trim() == "```").unwrap_or(false) {
            lines.pop();
        }
        lines.join("\n").trim().to_string()
    } else {
        s.to_string()
    };
    if let Ok(v) = serde_json::from_str::<Value>(&s) {
        return Ok(v);
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            // Streaming deserialize: parses one value, tolerating trailing text.
            let mut iter = serde_json::Deserializer::from_str(&s[i..]).into_iter::<Value>();
            if let Some(Ok(v)) = iter.next() {
                return Ok(v);
            }
        }
        i += 1;
    }
    Err("invalid JSON".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn mock_findings_shape() {
        let schema = json!({
            "type":"object","required":["findings"],
            "properties":{"findings":{"type":"array","items":{
                "type":"object","required":["title","severity"],
                "properties":{"title":{"type":"string"},
                    "severity":{"type":"string","enum":["high","medium","low"]}}}}}
        });
        let out = MockRunner
            .run("p", Some(&schema), Some("audit:security"))
            .await
            .unwrap();
        assert!(out.value["findings"].is_array());
        assert!(out.tokens > 0);
    }

    #[tokio::test]
    async fn mock_verdict_shape() {
        let schema = json!({
            "type":"object","required":["isReal","reason"],
            "properties":{"isReal":{"type":"boolean"},"reason":{"type":"string"}}
        });
        let out = MockRunner
            .run("p", Some(&schema), Some("verify"))
            .await
            .unwrap();
        assert!(out.value["isReal"].is_boolean());
    }

    #[tokio::test]
    async fn mock_no_schema_returns_text() {
        let out = MockRunner.run("hello", None, None).await.unwrap();
        assert!(out.value.is_string());
    }

    #[test]
    fn parse_json_strips_fences() {
        assert_eq!(parse_json("```json\n{\"a\":1}\n```").unwrap(), json!({"a":1}));
        assert_eq!(parse_json("{\"a\":1}").unwrap(), json!({"a":1}));
        assert_eq!(parse_json("noise {\"a\":2} tail").unwrap(), json!({"a":2}));
        assert!(parse_json("not json at all").is_err());
    }
}
