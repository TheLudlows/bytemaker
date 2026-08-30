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

/// 手写 Debug（内部 Provider 是 trait 对象无法 derive）；CLI 测试的失败信息用。
impl std::fmt::Debug for ProviderSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderSpec::Replay { cassette } => {
                write!(f, "Replay {{ cassette: {} }}", cassette.display())
            }
            ProviderSpec::Record { cassette, .. } => {
                write!(f, "Record {{ cassette: {} }}", cassette.display())
            }
            ProviderSpec::Live { .. } => write!(f, "Live {{ provider: .. }}"),
        }
    }
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

/// eval 子命令用法。
pub const USAGE: &str = "usage:\n  bytemaker eval run --suite <name> [--replay|--live|--record] [--cassette <path>]\n  bytemaker eval compare <baseline.json> <candidate.json>";

/// `bytemaker eval ...` 入口（main.rs 在读取 OPENAI_API_KEY 之前分发，
/// 保证 `--replay` 离线可用，验收 #3）。
pub async fn run_cli(args: &[String]) -> Result<(), AgentError> {
    match args.first().map(String::as_str) {
        Some("run") => cli_run(&args[1..]).await,
        Some("compare") => cli_compare(&args[1..]),
        _ => Err(AgentError::Other(USAGE.to_string())),
    }
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .filter(|v| !v.starts_with("--"))
        .cloned()
}

async fn cli_run(args: &[String]) -> Result<(), AgentError> {
    let suite_name = flag_value(args, "--suite").unwrap_or_else(|| "core".to_string());
    let evals_dir = PathBuf::from("evals");
    let suite_path = evals_dir.join("suites").join(format!("{suite_name}.yaml"));
    let suite = suite::EvalSuite::from_yaml_file(&suite_path).map_err(AgentError::Other)?;
    let spec = resolve_spec(args, &suite_name, default_cassette)?;
    let runner = EvalRunner::new(
        evals_dir.clone(),
        suite,
        spec,
        Arc::new(crate::tools::build_registry()),
    )?;
    let report = runner.run().await?;

    let runs_dir = evals_dir.join("runs");
    let path = runs_dir.join(format!(
        "{}-{suite_name}.json",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    report.write_json(&path).map_err(AgentError::Other)?;
    println!("{}", report.render_table());
    println!("report written to {}", path.display());
    Ok(())
}

fn cli_compare(args: &[String]) -> Result<(), AgentError> {
    if args.len() != 2 {
        return Err(AgentError::Other(USAGE.to_string()));
    }
    let baseline = report::RunReport::from_json_file(&PathBuf::from(&args[0]))
        .map_err(AgentError::Other)?;
    let candidate = report::RunReport::from_json_file(&PathBuf::from(&args[1]))
        .map_err(AgentError::Other)?;
    println!("{}", report::render_compare(&report::compare(&baseline, &candidate)));
    Ok(())
}

/// 默认盒带查找：`--cassette` > `BYTEMAKER_CASSETTE` > `evals/cassettes/<suite>.json`。
fn default_cassette(suite_name: &str) -> Option<PathBuf> {
    Some(PathBuf::from("evals/cassettes").join(format!("{suite_name}.json")))
}

/// 模式解析（优先级：显式 flag > BYTEMAKER_CASSETTE > 报错）。
/// `cassette_default` 由测试注入（生产传 `default_cassette`）。
fn resolve_spec(
    args: &[String],
    suite_name: &str,
    // impl Fn 而非 fn 指针：测试注入的闭包捕获了局部 PathBuf（fn 指针不收捕获闭包）。
    cassette_default: impl Fn(&str) -> Option<PathBuf>,
) -> Result<ProviderSpec, AgentError> {
    let env_spec = cassette::spec_from_env().map_err(AgentError::Other)?;
    // 预先取出 env 路径：`CassetteSpec` 非 Clone，在闭包里直接 match 会把
    // `env_spec` 整体 move 走，与末尾的 match 冲突（borrowck）。
    let env_replay = match &env_spec {
        Some(cassette::CassetteSpec::Replay(p)) => Some(p.clone()),
        _ => None,
    };
    let env_record = match &env_spec {
        Some(cassette::CassetteSpec::Record(p)) => Some(p.clone()),
        _ => None,
    };
    let from_flag = || flag_value(args, "--cassette").map(PathBuf::from);
    let default_path = || cassette_default(suite_name);

    let live = || -> Result<ProviderSpec, AgentError> {
        let cfg = crate::config::Config::from_env()?;
        Ok(ProviderSpec::Live {
            provider: Arc::new(crate::providers::openai::OpenAiProvider::new(
                cfg.api_key,
                cfg.base_url,
                cfg.model.clone(),
            )),
        })
    };
    let record = |path: PathBuf| -> Result<ProviderSpec, AgentError> {
        let cfg = crate::config::Config::from_env()?;
        Ok(ProviderSpec::Record {
            cassette: path,
            inner: Box::new(crate::providers::openai::OpenAiProvider::new(
                cfg.api_key,
                cfg.base_url,
                cfg.model.clone(),
            )),
            model: cfg.model,
        })
    };

    if has_flag(args, "--live") {
        return live();
    }
    if has_flag(args, "--record") {
        let path = from_flag()
            .or(env_record)
            .or_else(default_path)
            .ok_or_else(|| AgentError::Other("no cassette path for --record".into()))?;
        return record(path);
    }
    if has_flag(args, "--replay") {
        let path = from_flag()
            .or(env_replay)
            .or_else(default_path)
            .ok_or_else(|| AgentError::Other(format!(
                "replay cassette for suite '{suite_name}' does not exist; \
                 record first with `bytemaker eval run --suite {suite_name} --record`"
            )))?;
        if !path.exists() {
            return Err(AgentError::Other(format!(
                "cassette {} does not exist; record first with `bytemaker eval run --suite {suite_name} --record`",
                path.display()
            )));
        }
        return Ok(ProviderSpec::Replay { cassette: path });
    }
    // 无显式模式：BYTEMAKER_CASSETTE 决定（docs §3.1）。
    match env_spec {
        Some(cassette::CassetteSpec::Replay(p)) => Ok(ProviderSpec::Replay { cassette: p }),
        Some(cassette::CassetteSpec::Record(p)) => record(p),
        None => Err(AgentError::Other(format!(
            "no provider mode: pass --replay/--live/--record or set BYTEMAKER_CASSETTE\n{USAGE}"
        ))),
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

    /// 给工具调用响应补上非零用量（`resp_with_usage` 只覆盖纯文本响应）。
    fn usage(
        mut r: crate::domain::message::MessagesResponse,
        input: u64,
        output: u64,
    ) -> crate::domain::message::MessagesResponse {
        r.usage = Some(crate::domain::message::Usage { input_tokens: input, output_tokens: output });
        r
    }

    /// 脚本里的用量全部非零且各不相同：token 证据的录制/回放对比不是 0==0 的空断言。
    fn demo_script() -> Vec<crate::domain::message::MessagesResponse> {
        vec![
            // task 1: read_file -> final answer -> judge
            usage(
                fault::tool_call_response("c1", "read_file", serde_json::json!({"path": "README.md"})),
                120,
                45,
            ),
            fault::resp_with_usage("The project is a demo project for evals.", 200, 30),
            fault::resp_with_usage(
                r#"{"pass": true, "score": 1.0, "rationale": "accurate"}"#,
                350,
                20,
            ),
            // task 2: 直接回答（无 judge 断言）
            fault::resp_with_usage("hello agent", 10, 5),
        ]
    }

    /// 候选运行脚本：与 `demo_script` 的差异是 (a) agent 最终答案退化为
    /// 「没读到 README」、(b) judge 裁决失败——judge 的失败至少经由候选的退化
    /// 答案传导（answer-mediated），而不是与 agent 行为脱钩的纯脚本裁决。
    fn demo_script_broken_judge() -> Vec<crate::domain::message::MessagesResponse> {
        vec![
            usage(
                fault::tool_call_response("c1", "read_file", serde_json::json!({"path": "README.md"})),
                120,
                45,
            ),
            fault::resp_with_usage("I could not read README.md.", 200, 30),
            fault::resp_with_usage(
                r#"{"pass": false, "score": 0.0, "rationale": "no evidence: README content was empty"}"#,
                350,
                20,
            ),
            fault::resp_with_usage("hello agent", 10, 5),
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
        assert!(
            a.metrics.prompt_tokens > 0 && b.metrics.prompt_tokens > 0,
            "token evidence must be non-zero (scripted usage is non-zero), got {} / {}",
            a.metrics.prompt_tokens,
            b.metrics.prompt_tokens
        );
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

    /// 验收 #6：compare 必须把成功率下降与新增失败任务明确标出。候选用
    /// broken_registry(["read_file"]) + 退化答案脚本（agent 最终回答声称没读到
    /// README，judge 据此判失败）——本测试确立的证据链是「候选答案退化 → judge
    /// 失败 → 成功率下降被标出」；broken registry 自身的效果（工具输出为空）由
    /// 脚本间接代表，不被断言直接观察。
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
        // 候选答案退化（脚本代表 read_file 空输出的后果）→ judge 判失败 → compare 标出。
        let c = report::compare(&baseline, &candidate);
        assert!(c.rate_delta < 0.0);
        assert_eq!(c.newly_failed, vec!["read-summary".to_string()]);
        let text = report::render_compare(&c);
        assert!(text.contains("NEWLY FAILED: read-summary"), "got: {text}");
        assert!(text.contains("-50.0 pts"), "rate drop must be shown with its value: {text}");
    }

    mod cli_tests {
        use super::super::*;
        use std::path::PathBuf;

        fn args(v: &[&str]) -> Vec<String> {
            v.iter().map(|s| s.to_string()).collect()
        }

        #[test]
        fn flag_helpers() {
            let a = args(&["run", "--suite", "core", "--replay"]);
            assert!(has_flag(&a, "--replay"));
            assert!(!has_flag(&a, "--live"));
            assert_eq!(flag_value(&a, "--suite").as_deref(), Some("core"));
            assert_eq!(flag_value(&a, "--cassette"), None);
        }

        #[test]
        fn resolve_spec_replay_defaults_to_suite_cassette() {
            // 环境变量会干扰该测试 —— 串行 + 清空。
            let _g = crate::eval::cassette::tests_env_lock();
            std::env::remove_var("BYTEMAKER_CASSETTE");
            let tmp = tempfile::TempDir::new().unwrap();
            let cassette = tmp.path().join("core.json");
            std::fs::write(&cassette, "").unwrap();
            let spec = resolve_spec(
                &args(&["--replay"]),
                "core",
                |_name| Some(cassette.clone()),
            )
            .unwrap();
            match spec {
                ProviderSpec::Replay { cassette: p } => assert_eq!(p, cassette),
                other => panic!("expected Replay, got {other:?}"),
            }
            // 盒带不存在 → 可行动错误。
            let err = resolve_spec(&args(&["--replay"]), "core", |_name| None).unwrap_err();
            assert!(err.to_string().contains("does not exist"), "got: {err}");
        }

        #[test]
        fn resolve_spec_requires_a_mode() {
            let _g = crate::eval::cassette::tests_env_lock();
            std::env::remove_var("BYTEMAKER_CASSETTE");
            let err = resolve_spec(&args(&[]), "core", |_name| None).unwrap_err();
            assert!(err.to_string().contains("--replay"), "got: {err}");
        }

        #[test]
        fn usage_message_documents_both_subcommands() {
            let _ = PathBuf::from("."); // PathBuf 已在作用域
            assert!(USAGE.contains("eval run") && USAGE.contains("eval compare"));
        }
    }

    mod core_suite {
        use super::*;
        use crate::eval::fault::{self, ScriptedProvider};
        use crate::eval::suite::EvalSuite;
        use std::path::PathBuf;
        use std::sync::Arc;

        fn repo_evals() -> PathBuf {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("evals")
        }

        fn core_script() -> Vec<crate::domain::message::MessagesResponse> {
            use fault::{tool_call_response as tc};
            // 用量全部非零且各不相同：提交版盒带携带真实形态的 token 证据，
            // 离线回放的 token 指标不再是全零占位。
            vec![
                // 1. read-project-summary: read_file -> 回答 -> judge
                usage(tc("c1", "read_file", serde_json::json!({"path": "README.md"})), 110, 40),
                fault::resp_with_usage(
                    "bytemaker is a hands-on tutorial that builds a Claude Code-style coding agent in Rust, step by step.",
                    240,
                    35,
                ),
                fault::resp_with_usage(
                    r#"{"pass": true, "score": 1.0, "rationale": "accurate summary"}"#,
                    310,
                    25,
                ),
                // 2. create-and-read-back: write_file -> read_file -> 回答 -> judge
                usage(
                    tc("c2", "write_file", serde_json::json!({"path": "test.py", "content": "print(\"hello\")\n"})),
                    150,
                    50,
                ),
                usage(tc("c3", "read_file", serde_json::json!({"path": "test.py"})), 260, 45),
                fault::resp_with_usage(
                    r#"Created test.py containing print("hello") and read it back — the file exists and prints hello."#,
                    380,
                    60,
                ),
                fault::resp_with_usage(
                    r#"{"pass": true, "score": 1.0, "rationale": "file created and read back"}"#,
                    410,
                    30,
                ),
                // 3. find-python-files: glob -> 回答 -> judge
                usage(tc("c4", "glob", serde_json::json!({"pattern": "**/*.py"})), 130, 25),
                fault::resp_with_usage("There are no Python files in this directory.", 220, 20),
                fault::resp_with_usage(
                    r#"{"pass": true, "score": 1.0, "rationale": "correctly reports none"}"#,
                    180,
                    15,
                ),
                // 4. minimal-agent-question: 直接回答 -> judge
                fault::resp_with_usage("hello agent", 90, 10),
                fault::resp_with_usage(
                    r#"{"pass": true, "score": 1.0, "rationale": "exact match"}"#,
                    120,
                    12,
                ),
                // 5. read-only-discipline: read_file -> 回答（无 judge 断言）
                usage(tc("c5", "read_file", serde_json::json!({"path": "README.md"})), 170, 30),
                fault::resp_with_usage(
                    "The first heading of README.md is: # bytemaker",
                    230,
                    28,
                ),
            ]
        }

        /// 重新生成提交版盒带（一次性维护操作，不进 CI）：
        /// `cargo test regenerate_core_cassette -- --ignored`
        #[tokio::test]
        #[ignore = "regenerates evals/cassettes/core.json; run: cargo test regenerate_core_cassette -- --ignored"]
        async fn regenerate_core_cassette() {
            let evals = repo_evals();
            let suite = EvalSuite::from_yaml_file(&evals.join("suites/core.yaml")).unwrap();
            let runner = EvalRunner::new(
                evals,
                suite,
                ProviderSpec::Record {
                    cassette: repo_evals().join("cassettes/core.json"),
                    inner: Box::new(ScriptedProvider::new(core_script())),
                    model: "scripted".into(),
                },
                Arc::new(crate::tools::build_registry()),
            )
            .unwrap();
            let report = runner.run().await.unwrap();
            assert_eq!(
                report.success_rate,
                1.0,
                "scripted record run must pass before committing the cassette:\n{}",
                report.render_table()
            );
        }

        /// 验收 #3/#5：CI 守卫——提交版盒带离线复放必须全绿（无 API key 环境）。
        #[tokio::test]
        async fn committed_core_cassette_replays_all_green() {
            let evals = repo_evals();
            let cassette = evals.join("cassettes/core.json");
            assert!(
                cassette.exists(),
                "missing committed cassette; run: cargo test regenerate_core_cassette -- --ignored"
            );
            let suite = EvalSuite::from_yaml_file(&evals.join("suites/core.yaml")).unwrap();
            let runner = EvalRunner::new(
                evals,
                suite,
                ProviderSpec::Replay { cassette },
                Arc::new(crate::tools::build_registry()),
            )
            .unwrap();
            let report = runner.run().await.unwrap();
            assert_eq!(
                report.success_rate,
                1.0,
                "committed cassette must replay green:\n{}",
                report.render_table()
            );
            // 验收 #5：≥5 个种子任务。
            assert!(report.tasks.len() >= 5, "got {} tasks", report.tasks.len());
            // 提交版盒带携带非零 token 用量（脚本生成时即非零），回放指标非全零。
            assert!(
                report.tasks.iter().all(|t| t.metrics.prompt_tokens > 0),
                "every task must replay non-zero prompt tokens: {:?}",
                report.tasks.iter().map(|t| t.metrics.prompt_tokens).collect::<Vec<_>>()
            );
        }
    }
}
