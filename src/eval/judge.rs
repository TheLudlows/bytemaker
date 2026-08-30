//! LLM-as-judge（docs/5.evals.md §3.4）。
//!
//! 裁判本身是一次 `LlmProvider::generate_completion` 调用——因此裁判的响应同样
//! 录制进盒带，离线模式下评测全链路无网络。

use serde::{Deserialize, Serialize};

use crate::domain::message::Message;
use crate::providers::LlmProvider;

/// 结构化裁决（docs §3.4）。`PartialEq` 供报告测试断言 `judge == None`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeVerdict {
    pub pass: bool,
    pub score: f32,
    pub rationale: String,
}

/// 裁判消息：任务 + 评分标准 + Agent 最终回答，要求只输出 JSON 裁决。
pub fn judge_messages(task_prompt: &str, rubric: &str, answer: &str) -> Vec<Message> {
    let content = format!(
        "You are an impartial judge evaluating an AI agent's final answer.\n\n\
Task given to the agent:\n{task_prompt}\n\n\
Grading rubric:\n{rubric}\n\n\
Agent's final answer:\n{answer}\n\n\
Respond with ONLY a JSON object, no other text:\n\
{{\"pass\": <true|false>, \"score\": <0.0-1.0>, \"rationale\": \"<one sentence>\"}}"
    );
    vec![Message::user_text(content)]
}

/// 跑一次裁判。任何 provider 错误都落为 pass=false 的确定性裁决（评测不因裁判
/// 失败而 panic；错误信息进 rationale 供报告展示）。
pub async fn run_judge(
    provider: &dyn LlmProvider,
    task_prompt: &str,
    rubric: &str,
    answer: &str,
) -> JudgeVerdict {
    match provider
        .generate_completion(&judge_messages(task_prompt, rubric, answer))
        .await
    {
        Ok(resp) => parse_verdict(&resp.content),
        Err(e) => JudgeVerdict {
            pass: false,
            score: 0.0,
            rationale: format!("judge call failed: {e}"),
        },
    }
}

/// 容错解析：剥 markdown 围栏，截取首个 `{` 到最后一个 `}`；解析失败返回
/// pass=false、rationale 说明「响应不是合法 JSON」。
pub fn parse_verdict(raw: &str) -> JudgeVerdict {
    let cleaned = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```");
    if let (Some(s), Some(e)) = (cleaned.find('{'), cleaned.rfind('}')) {
        if e > s {
            if let Ok(v) = serde_json::from_str::<JudgeVerdict>(&cleaned[s..=e]) {
                return v;
            }
        }
    }
    let preview: String = raw.chars().take(120).collect();
    JudgeVerdict {
        pass: false,
        score: 0.0,
        rationale: format!("judge response was not valid JSON: {preview}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::MockProvider;

    #[test]
    fn parse_verdict_plain_json() {
        let v = parse_verdict(r#"{"pass": true, "score": 0.9, "rationale": "good"}"#);
        assert!(v.pass);
        assert_eq!(v.score, 0.9);
        assert_eq!(v.rationale, "good");
    }

    #[test]
    fn parse_verdict_tolerates_fences_and_prose() {
        let v = parse_verdict("Here is my verdict:\n```json\n{\"pass\": false, \"score\": 0.2, \"rationale\": \"wrong\"}\n```\nthanks");
        assert!(!v.pass);
        assert_eq!(v.rationale, "wrong");
    }

    #[test]
    fn parse_verdict_garbage_fails_deterministically() {
        let v = parse_verdict("I cannot answer that.");
        assert!(!v.pass);
        assert_eq!(v.score, 0.0);
        assert!(v.rationale.contains("not valid JSON"), "got: {}", v.rationale);
    }

    #[tokio::test]
    async fn run_judge_uses_provider_generate_completion() {
        let provider = MockProvider::new(r#"{"pass": true, "score": 1.0, "rationale": "accurate"}"#);
        let v = run_judge(&provider, "summarize the project", "准确概括", "It is a demo.").await;
        assert!(v.pass);
        assert_eq!(v.score, 1.0);
    }

    #[tokio::test]
    async fn run_judge_maps_provider_error_to_fail() {
        // MockProvider 不会失败；用一个耗尽的 ScriptedProvider 驱动错误路径。
        let provider = crate::eval::fault::ScriptedProvider::new(vec![]);
        let v = run_judge(&provider, "p", "r", "a").await;
        assert!(!v.pass);
        assert!(v.rationale.contains("judge call failed"), "got: {}", v.rationale);
    }

    #[test]
    fn judge_messages_contains_all_inputs() {
        let msgs = judge_messages("do the task", "the rubric", "the answer");
        assert_eq!(msgs.len(), 1);
        let text = serde_json::to_string(&msgs[0]).unwrap();
        assert!(text.contains("do the task") && text.contains("the rubric") && text.contains("the answer"));
        // serde_json escapes the quotes inside the prompt, so the serialized form
        // carries `\"pass\"` — that proves the content demands the JSON verdict shape.
        assert!(text.contains("\\\"pass\\\""), "prompt must demand the JSON verdict shape");
    }
}
