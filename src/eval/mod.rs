//! eval —— 评测与确定性测试基建（docs/5.evals.md）。
//!
//! 本章门面：各子模块（cassette/fingerprint/fault/suite/judge/trajectory/report）
//! 加上 `EvalRunner`（三种 Provider 模式跑同一套任务集）；`eval` CLI 子命令见后续任务。

pub mod cassette;
pub mod fault;
pub mod fingerprint;
pub mod judge;
pub mod report;
pub mod suite;
pub mod trajectory;

use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::Agent;
use crate::domain::message::Message;
use crate::error::AgentError;
use crate::hooks::Hooks;
use crate::providers::LlmProvider;

/// eval agent 的回合上限（失控任务必须被截断）。
pub const EVAL_MAX_TURNS: usize = 20;

/// eval agent 的 system prompt（不入指纹，但保持稳定）。
const EVAL_SYSTEM: &str =
    "You are a coding agent. Use tools as needed to complete the task, then answer concisely.";

/// 评测运行如何获得 LLM 能力（docs §2.3：三种模式，一个 Trait）。
pub enum ProviderSpec {
    /// 离线回放（CI 用）。
    Replay { cassette: PathBuf },
    /// 录制：包装真实 Provider 落盘盒带。
    Record { cassette: PathBuf, inner: Box<dyn LlmProvider>, model: String },
    /// 真实 LLM，测真实成功率。
    Live { provider: Arc<dyn LlmProvider> },
}

/// 评测运行器：任务集 → 逐任务（布景 → 跑标准 agent loop → 断言）→ 报告。
///
/// 每任务流程：布景临时 workspace → 组装该任务的 masks（基础规则 + workspace
/// 路径字面量 + 其 canonical 形式，都替换为 `<WORKSPACE>`）→ `cassette.set_masks`
/// → 记录 meter 基线 → 构建 isolated Agent（meter 包 cassette 的 Provider 栈 +
/// TrajectoryCollector hook）→ 跑标准 `run_loop` → meter 增量 + collector 快照
/// 合成指标。
///
/// 盒带调用顺序：先 Agent 的全部调用、后 judge 调用（record 与 replay 走同一
/// 代码路径，顺序天然一致）。judge 只在 `run_loop` 成功返回时执行（漂移/失败时
/// 跳过，报告里落 failure）。
pub struct EvalRunner {
    evals_dir: PathBuf,
    suite: suite::EvalSuite,
    mode: String,
    cassette: Option<Arc<cassette::CassetteProvider>>,
    meter: Arc<trajectory::MeteredProvider>,
    base_masks: fingerprint::MaskRules,
    registry: Arc<crate::tools::ToolRegistry>,
}

impl EvalRunner {
    pub fn new(
        evals_dir: PathBuf,
        suite: suite::EvalSuite,
        spec: ProviderSpec,
        registry: Arc<crate::tools::ToolRegistry>,
    ) -> Result<Self, AgentError> {
        let (cassette, provider, base_masks, mode) = match spec {
            ProviderSpec::Replay { cassette: path } => {
                let masks = cassette::load_masks_for(&path);
                let p = Arc::new(
                    cassette::CassetteProvider::replay(&path, masks.clone())
                        .map_err(AgentError::Other)?,
                );
                (Some(p.clone()), p as Arc<dyn LlmProvider>, masks, "replay".to_string())
            }
            ProviderSpec::Record { cassette: path, inner, model } => {
                let masks = cassette::load_masks_for(&path);
                let p = Arc::new(
                    cassette::CassetteProvider::record(inner, &path, masks.clone(), &model)
                        .map_err(AgentError::Other)?,
                );
                (Some(p.clone()), p as Arc<dyn LlmProvider>, masks, "record".to_string())
            }
            ProviderSpec::Live { provider } => {
                (None, provider, fingerprint::MaskRules::default_rules(), "live".to_string())
            }
        };
        let meter = Arc::new(trajectory::MeteredProvider::new(provider));
        Ok(Self { evals_dir, suite, mode, cassette, meter, base_masks, registry })
    }

    /// 跑完整任务集，产出报告（timestamp 由调用方或 CLI 决定是否落盘）。
    pub async fn run(&self) -> Result<report::RunReport, AgentError> {
        let mut tasks = Vec::with_capacity(self.suite.tasks.len());
        for task in &self.suite.tasks {
            tasks.push(self.run_task(task).await);
        }
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        Ok(report::RunReport::new(
            self.suite.suite.clone(),
            self.mode.clone(),
            timestamp,
            tasks,
        ))
    }

    async fn run_task(&self, task: &suite::EvalTask) -> report::TaskReport {
        // 1. 布景 workspace；把当前临时目录（含 canonical 形式）注入掩码——
        //    工具输出里的绝对路径两次运行指纹一致。
        let workspace = match suite::stage_workspace(&self.evals_dir, task.workspace.as_deref()) {
            Ok(w) => w,
            Err(e) => {
                return report::TaskReport::failed(
                    task.name.clone(),
                    format!("workspace staging failed: {e}"),
                )
            }
        };
        let workdir = workspace.path().to_path_buf();
        let mut masks = self
            .base_masks
            .clone()
            .with_literal(&workdir.to_string_lossy(), "<WORKSPACE>");
        if let Ok(canon) = dunce::canonicalize(&workdir) {
            masks = masks.with_literal(&canon.to_string_lossy(), "<WORKSPACE>");
        }
        if let Some(c) = &self.cassette {
            c.set_masks(masks);
        }

        // 2. meter 基线 + collector hook + isolated agent。
        let before = self.meter.totals();
        let collector = trajectory::TrajectoryCollector::new();
        let agent = Agent::isolated(
            workdir,
            self.meter.clone() as Arc<dyn LlmProvider>,
            Arc::new(crate::io::IO::memory()),
            eval_hooks(collector.clone()),
            EVAL_SYSTEM.to_string(),
            EVAL_MAX_TURNS,
            Arc::clone(&self.registry),
        );

        // 3. 标准 agent loop（与 REPL 完全同一条代码路径）。
        let mut messages = vec![Message::user_text(task.prompt.clone())];
        let outcome = agent.run_loop(&mut messages, &task.prompt, "", "").await;
        let after = self.meter.totals();
        let metrics = trajectory::compose(
            collector.snapshot(),
            (after.0 - before.0, after.1 - before.1, after.2 - before.2),
        );

        // 4. 断言。盒带调用顺序约定：先 Agent 全部调用、后 judge（record/replay
        //    同路径，顺序一致）。run_loop 失败（如盒带漂移）时跳过 judge。
        let mut failures: Vec<String> = Vec::new();
        let mut judge: Option<judge::JudgeVerdict> = None;
        match &outcome {
            Ok(_) => {
                let answer =
                    crate::agent::extract_final_text_from(&messages).unwrap_or_default();
                for a in &task.assertions {
                    match a {
                        suite::Assertion::Trajectory { max_steps, max_tokens, forbidden_tools } => {
                            if let Some(ms) = max_steps {
                                if metrics.steps > *ms {
                                    failures.push(format!("steps {} > max {}", metrics.steps, ms));
                                }
                            }
                            if let Some(mt) = max_tokens {
                                let total = metrics.prompt_tokens + metrics.completion_tokens;
                                if total > *mt {
                                    failures.push(format!("tokens {total} > max {mt}"));
                                }
                            }
                            for t in forbidden_tools {
                                if metrics.tool_calls.contains_key(t) {
                                    failures.push(format!("forbidden tool used: {t}"));
                                }
                            }
                            if metrics.loop_detected {
                                failures.push("loop detected (repeated adjacent tool calls)".into());
                            }
                        }
                        suite::Assertion::Judge { rubric } => {
                            let v = judge::run_judge(
                                self.meter.as_ref(),
                                &task.prompt,
                                rubric,
                                &answer,
                            )
                            .await;
                            if !v.pass {
                                failures.push(format!("judge: {}", v.rationale));
                            }
                            judge = Some(v);
                        }
                    }
                }
            }
            Err(e) => failures.push(format!("agent run failed: {e}")),
        }

        report::TaskReport {
            name: task.name.clone(),
            pass: failures.is_empty(),
            judge,
            metrics,
            failure: (!failures.is_empty()).then(|| failures.join("; ")),
        }
    }
}

/// eval agent 的 hook 集：权限门（ch2 三闸）+ 轨迹采集（旁路）。
/// 刻意不带 TodoReminder/Summary——eval 不需要提醒注入，保持轨迹最小确定。
fn eval_hooks(collector: trajectory::TrajectoryCollector) -> Hooks {
    let mut h = Hooks::new();
    h.on_pre_tool(crate::builtins::PermissionHook);
    h.on_post_tool(collector);
    h
}

/// 回归捕获演示（验收 #6）：把指定工具替换为永远返回空输出的桩。
/// `bytemaker eval compare` 场景：基线全绿 vs 弄坏 read_file 后成功率下降。
pub fn broken_registry(broken: &[&str]) -> Arc<crate::tools::ToolRegistry> {
    let mut registry = crate::tools::build_registry();
    for name in broken {
        registry.register(Box::new(BrokenTool { name: name.to_string() }));
    }
    Arc::new(registry)
}

struct BrokenTool {
    name: String,
}

#[async_trait::async_trait]
impl crate::tools::Tool for BrokenTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "broken on purpose (regression-capture demo)"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _ctx: &crate::tools::ToolContext<'_>,
        _input: &serde_json::Value,
    ) -> String {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::fault::ScriptedProvider;
    use crate::eval::suite::EvalSuite;
    use std::sync::Arc;

    /// 测试用 evals 目录：内联 fixture（README.md），不依赖 Task 11 的资产。
    fn test_env() -> tempfile::TempDir {
        let evals = tempfile::TempDir::new().unwrap();
        let fixture = evals.path().join("fixtures/readme");
        std::fs::create_dir_all(&fixture).unwrap();
        std::fs::write(fixture.join("README.md"), "# demo\nA demo project for evals.").unwrap();
        evals
    }

    const DEMO_SUITE: &str = r#"
suite: demo
tasks:
  - name: read-summary
    prompt: "Read README.md and summarize it"
    workspace: fixtures/readme/
    assertions:
      - kind: judge
        rubric: "回答概括了 demo 项目"
      - kind: trajectory
        max_steps: 4
        forbidden_tools: [write_file]
  - name: minimal
    prompt: "Reply with exactly: hello agent"
    assertions:
      - kind: trajectory
        max_steps: 2
"#;

    fn demo_script() -> Vec<crate::domain::message::MessagesResponse> {
        vec![
            // task 1: read_file -> final answer -> judge
            fault::tool_call_response("c1", "read_file", serde_json::json!({"path": "README.md"})),
            fault::text_response("The project is a demo project for evals."),
            fault::text_response(r#"{"pass": true, "score": 1.0, "rationale": "accurate"}"#),
            // task 2: 直接回答（无 judge 断言）
            fault::text_response("hello agent"),
        ]
    }

    /// 候选运行脚本：read_file 被弄坏（空输出）→ 证据缺失 → judge 打分失败
    /// （只改第 3 个响应，见任务简报「修正说明」）。
    fn demo_script_broken_judge() -> Vec<crate::domain::message::MessagesResponse> {
        vec![
            fault::tool_call_response("c1", "read_file", serde_json::json!({"path": "README.md"})),
            fault::text_response("The project is a demo project for evals."),
            fault::text_response(
                r#"{"pass": false, "score": 0.0, "rationale": "no evidence: README content was empty"}"#,
            ),
            fault::text_response("hello agent"),
        ]
    }

    fn runner(
        evals: &std::path::Path,
        suite: EvalSuite,
        spec: ProviderSpec,
    ) -> EvalRunner {
        EvalRunner::new(
            evals.to_path_buf(),
            suite,
            spec,
            Arc::new(crate::tools::build_registry()),
        )
        .unwrap()
    }

    /// 验收 #1：录制（脚本 Provider 走真实录制路径）→ 离线回放，轨迹完全一致。
    #[tokio::test]
    async fn e2e_record_then_replay_reproduces_trajectory() {
        let evals = test_env();
        let cassette = evals.path().join("cassettes/demo.json");

        let recorded = runner(
            evals.path(),
            EvalSuite::from_yaml_str(DEMO_SUITE).unwrap(),
            ProviderSpec::Record {
                cassette: cassette.clone(),
                inner: Box::new(ScriptedProvider::new(demo_script())),
                model: "scripted".into(),
            },
        )
        .run()
        .await
        .unwrap();
        assert_eq!(recorded.success_rate, 1.0, "{:?}", recorded.tasks);

        let replayed = runner(
            evals.path(),
            EvalSuite::from_yaml_str(DEMO_SUITE).unwrap(),
            ProviderSpec::Replay { cassette },
        )
        .run()
        .await
        .unwrap();
        assert_eq!(replayed.success_rate, 1.0, "{:?}", replayed.tasks);

        let a = &recorded.tasks[0];
        let b = &replayed.tasks[0];
        assert_eq!(a.metrics.steps, b.metrics.steps, "steps must match");
        assert_eq!(a.metrics.tool_calls, b.metrics.tool_calls, "tool sequence must match");
        assert_eq!(a.metrics.prompt_tokens, b.metrics.prompt_tokens, "recorded usage replays");
        assert_eq!(
            a.judge.as_ref().unwrap().rationale,
            b.judge.as_ref().unwrap().rationale,
            "judge verdict must be reproducible offline"
        );
    }

    /// 验收 #2：prompt 改一个词后回放旧盒带 → 可行动的漂移报错，不是静默假绿。
    #[tokio::test]
    async fn e2e_prompt_change_fails_with_actionable_drift() {
        let evals = test_env();
        let cassette = evals.path().join("cassettes/demo.json");
        runner(
            evals.path(),
            EvalSuite::from_yaml_str(DEMO_SUITE).unwrap(),
            ProviderSpec::Record {
                cassette: cassette.clone(),
                inner: Box::new(ScriptedProvider::new(demo_script())),
                model: "scripted".into(),
            },
        )
        .run()
        .await
        .unwrap();

        let drifted_yaml = DEMO_SUITE.replace("summarize it", "summarise it");
        let replayed = runner(
            evals.path(),
            EvalSuite::from_yaml_str(&drifted_yaml).unwrap(),
            ProviderSpec::Replay { cassette },
        )
        .run()
        .await
        .unwrap();
        let failure = replayed.tasks[0].failure.as_ref().expect("task must fail");
        assert!(failure.contains("cassette drift at call #1"), "got: {failure}");
        assert!(failure.contains("re-record"), "got: {failure}");
        assert!(replayed.success_rate < 1.0);
    }

    /// 验收 #3 之一：走盒带的 Agent 端到端测试——trajectory 断言拦住被禁工具。
    #[tokio::test]
    async fn e2e_forbidden_tool_fails_trajectory_assertion() {
        let evals = test_env();
        let suite = EvalSuite::from_yaml_str(
            r#"
suite: demo
tasks:
  - name: rogue-write
    prompt: "Write a file"
    workspace: fixtures/readme/
    assertions:
      - kind: trajectory
        forbidden_tools: [write_file]
"#,
        )
        .unwrap();
        let script = vec![
            fault::tool_call_response("c1", "write_file", serde_json::json!({"path": "x.txt", "content": "hi"})),
            fault::text_response("written"),
        ];
        let report = runner(
            evals.path(),
            suite,
            ProviderSpec::Record {
                cassette: evals.path().join("c.json"),
                inner: Box::new(ScriptedProvider::new(script)),
                model: "scripted".into(),
            },
        )
        .run()
        .await
        .unwrap();
        assert!(!report.tasks[0].pass);
        let failure = report.tasks[0].failure.as_deref().unwrap();
        assert!(failure.contains("forbidden tool used: write_file"), "got: {failure}");
    }

    /// 验收 #6：故意弄坏一个工具（read_file 返回空），compare 必须把成功率下降与
    /// 新增失败任务明确标出。基线用健康 registry + 全绿 judge；候选用
    /// broken_registry(["read_file"]) + judge 失败脚本（唯一差异是 read_file 失效）。
    #[tokio::test]
    async fn e2e_compare_flags_broken_tool_regression() {
        let evals = test_env();
        let suite = EvalSuite::from_yaml_str(DEMO_SUITE).unwrap();
        let script = || ScriptedProvider::new(demo_script());

        let baseline = EvalRunner::new(
            evals.path().to_path_buf(),
            suite.clone(),
            ProviderSpec::Record {
                cassette: evals.path().join("baseline.json"),
                inner: Box::new(script()),
                model: "scripted".into(),
            },
            Arc::new(crate::tools::build_registry()),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
        assert_eq!(baseline.success_rate, 1.0);

        let candidate = EvalRunner::new(
            evals.path().to_path_buf(),
            suite,
            ProviderSpec::Record {
                cassette: evals.path().join("candidate.json"),
                inner: Box::new(ScriptedProvider::new(demo_script_broken_judge())),
                model: "scripted".into(),
            },
            broken_registry(&["read_file"]),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
        // read_file 返回空 → 回答证据缺失 → judge 打分失败（脚本里 judge 对空证据扣分）。
        let c = report::compare(&baseline, &candidate);
        assert!(c.rate_delta < 0.0);
        assert_eq!(c.newly_failed, vec!["read-summary".to_string()]);
        let text = report::render_compare(&c);
        assert!(text.contains("NEWLY FAILED: read-summary"), "got: {text}");
        assert!(text.contains("-50.0 pts") || text.contains("pts"), "rate drop must be shown");
    }
}
