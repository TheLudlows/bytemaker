//! 轨迹采集与指标聚合（docs/5.evals.md §2.4 / §3.5）。
//!
//! 采集器实现为 **ch2 的 Hooks 中间件**——旁路观测，不侵入 Agent Loop 一行代码，
//! 因此死循环检测天然对 Workflow（ch6）与 Subagent（ch8）生效。LLM 调用计数与
//! token 累计无法从 Hooks 看到，由 `MeteredProvider`（Provider 装饰器，同样零侵入）
//! 承担；`compose` 把两路数据拼成 `TrajectoryMetrics`。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::domain::message::Message;
use crate::hooks::PostToolHook;
use crate::providers::{CallResult, LlmProvider};
use crate::tools::trait_def::ToolDefinition;

/// 死循环判定窗口：相邻 N 次工具调用签名完全重复。
pub const LOOP_WINDOW: usize = 3;

/// 单任务轨迹指标（docs §3.5）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrajectoryMetrics {
    /// LLM 调用次数（来自 MeteredProvider）。
    pub steps: u32,
    /// 工具名 → 调用次数（来自 PostToolHook，仅统计真实执行）。
    pub tool_calls: BTreeMap<String, u32>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub wall_ms: u64,
    /// 相邻 LOOP_WINDOW 次工具调用签名重复。
    pub loop_detected: bool,
}

/// 采集器某一时刻的快照（不含 meter 数据）。
pub struct CollectorSnapshot {
    pub tool_calls: BTreeMap<String, u32>,
    pub loop_detected: bool,
    pub wall_ms: u64,
}

struct CollectorState {
    tool_calls: BTreeMap<String, u32>,
    signatures: Vec<String>,
    started: Instant,
}

/// 轨迹采集中间件：ch2 Hooks 的旁路观测器。`Clone` 是 Arc 浅拷贝——
/// 一份注册进 Hooks，一份留在 runner 手里做 snapshot。
#[derive(Clone)]
pub struct TrajectoryCollector {
    state: Arc<Mutex<CollectorState>>,
}

impl TrajectoryCollector {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(CollectorState {
                tool_calls: BTreeMap::new(),
                signatures: Vec::new(),
                started: Instant::now(),
            })),
        }
    }

    pub fn snapshot(&self) -> CollectorSnapshot {
        let s = self.state.lock().unwrap();
        CollectorSnapshot {
            tool_calls: s.tool_calls.clone(),
            loop_detected: detect_loop(&s.signatures),
            wall_ms: s.started.elapsed().as_millis() as u64,
        }
    }
}

impl Default for TrajectoryCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PostToolHook for TrajectoryCollector {
    /// 永远返回 None：只旁路记录，不注入提醒、不短路其它 hook。
    async fn on_post_tool(
        &self,
        name: &str,
        input: &serde_json::Value,
        _output: &str,
    ) -> Option<String> {
        let mut s = self.state.lock().unwrap();
        *s.tool_calls.entry(name.to_string()).or_insert(0) += 1;
        s.signatures.push(format!("{name}:{input}"));
        None
    }
}

/// 相邻 LOOP_WINDOW 次签名重复 → 判定死循环。
pub fn detect_loop(signatures: &[String]) -> bool {
    if signatures.len() < LOOP_WINDOW {
        return false;
    }
    signatures
        .windows(LOOP_WINDOW)
        .any(|w| w.iter().all(|s| s == &w[0]))
}

#[derive(Default)]
struct MeterState {
    calls: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
}

/// Provider 装饰器：计数 LLM 调用并累计 token 用量（Hooks 看不到 LLM 调用，
/// 这是采集 steps/tokens 的零侵入通道；record 与 replay 共用，故指标可对比）。
pub struct MeteredProvider {
    inner: Arc<dyn LlmProvider>,
    state: Mutex<MeterState>,
}

impl MeteredProvider {
    pub fn new(inner: Arc<dyn LlmProvider>) -> Self {
        Self { inner, state: Mutex::new(MeterState::default()) }
    }

    /// (calls, prompt_tokens, completion_tokens)。
    pub fn totals(&self) -> (u64, u64, u64) {
        let s = self.state.lock().unwrap();
        (s.calls, s.prompt_tokens, s.completion_tokens)
    }
}

#[async_trait]
impl LlmProvider for MeteredProvider {
    async fn stream_messages(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        max_tokens: u32,
        cancel: CancellationToken,
    ) -> CallResult {
        let result = self
            .inner
            .stream_messages(system, messages, tools, max_tokens, cancel)
            .await;
        if let CallResult::Success(r) = &result {
            let mut s = self.state.lock().unwrap();
            s.calls += 1;
            if let Some(u) = &r.usage {
                s.prompt_tokens += u.input_tokens;
                s.completion_tokens += u.output_tokens;
            }
        }
        result
    }
}

/// 两路采集数据 → `TrajectoryMetrics`。
pub fn compose(snapshot: CollectorSnapshot, meter: (u64, u64, u64)) -> TrajectoryMetrics {
    TrajectoryMetrics {
        steps: meter.0 as u32,
        tool_calls: snapshot.tool_calls,
        prompt_tokens: meter.1,
        completion_tokens: meter.2,
        wall_ms: snapshot.wall_ms,
        loop_detected: snapshot.loop_detected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::Message;
    use crate::providers::LlmProvider;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn collector_counts_tool_calls_and_wall_time() {
        let c = TrajectoryCollector::new();
        c.on_post_tool("read_file", &serde_json::json!({"path": "a"}), "out").await;
        c.on_post_tool("read_file", &serde_json::json!({"path": "b"}), "out").await;
        c.on_post_tool("glob", &serde_json::json!({"pattern": "**"}), "out").await;
        let snap = c.snapshot();
        assert_eq!(snap.tool_calls.get("read_file"), Some(&2));
        assert_eq!(snap.tool_calls.get("glob"), Some(&1));
        assert!(!snap.loop_detected);
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(c.snapshot().wall_ms >= 2, "wall clock must advance");
    }

    #[tokio::test]
    async fn collector_detects_repeated_adjacent_signatures() {
        let c = TrajectoryCollector::new();
        let input = serde_json::json!({"path": "same"});
        for _ in 0..3 {
            c.on_post_tool("read_file", &input, "out").await;
        }
        assert!(c.snapshot().loop_detected, "3 identical adjacent calls = loop");
        // 不同输入不算死循环。
        let c2 = TrajectoryCollector::new();
        c2.on_post_tool("read_file", &serde_json::json!({"path": "a"}), "out").await;
        c2.on_post_tool("read_file", &serde_json::json!({"path": "b"}), "out").await;
        c2.on_post_tool("read_file", &serde_json::json!({"path": "c"}), "out").await;
        assert!(!c2.snapshot().loop_detected);
    }

    #[test]
    fn detect_loop_edge_cases() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(!detect_loop(&s(&[])));
        assert!(!detect_loop(&s(&["a", "a"])));
        assert!(detect_loop(&s(&["a", "a", "a"])));
        assert!(detect_loop(&s(&["a", "b", "b", "b"])));
        assert!(!detect_loop(&s(&["a", "a", "b", "a", "a"])));
    }

    #[tokio::test]
    async fn metered_provider_accumulates_usage() {
        let scripted = Arc::new(crate::eval::fault::ScriptedProvider::new(vec![
            crate::eval::fault::resp_with_usage("a", 10, 5),
            crate::eval::fault::resp_with_usage("b", 7, 3),
        ]));
        let metered = MeteredProvider::new(scripted as Arc<dyn LlmProvider>);
        let msgs = vec![Message::user_text("x")];
        for _ in 0..2 {
            metered
                .stream_messages("", &msgs, &[], 100, CancellationToken::new())
                .await;
        }
        assert_eq!(metered.totals(), (2, 17, 8));
    }

    #[test]
    fn compose_merges_collector_and_meter() {
        let snap = CollectorSnapshot {
            tool_calls: [("read_file".to_string(), 2u32)].into_iter().collect(),
            loop_detected: true,
            wall_ms: 42,
        };
        let m = compose(snap, (3, 100, 50));
        assert_eq!(m.steps, 3);
        assert_eq!(m.tool_calls.get("read_file"), Some(&2));
        assert_eq!((m.prompt_tokens, m.completion_tokens), (100, 50));
        assert_eq!(m.wall_ms, 42);
        assert!(m.loop_detected);
    }
}
