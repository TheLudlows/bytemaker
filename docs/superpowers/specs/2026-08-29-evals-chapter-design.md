# 设计：新增第 5 章「评测与确定性测试基建」

日期：2026-08-29
状态：已与用户逐节确认

## 背景与动机

对 `docs/curriculum.md` 的 gap 分析指出：大纲在单体 Agent 内核上完整，但缺评测与确定性测试基建——Agent 行为非确定，改一行 prompt 可能悄悄弄坏行为而无从发现。本设计将「评测与确定性测试基建」插入课程序列，作为新的第 5 章。

## 已确认的决策

| 决策点 | 结论 |
| --- | --- |
| 章节位置 | 插为新的第 5 章（原 5-9 章顺延为 6-10，共 11 章 0-10） |
| 模块范围 | Record/Replay 盒带 + 评测框架（任务集 + judge）+ 轨迹指标与回归对比；**CI 集成与门禁排除在外**（留给生产化章） |
| 回放匹配策略 | **方案 A：顺序流回放**——有序 `(请求指纹, 响应)` 列表，指纹分级校验（归一化匹配为主）；方案 C 降级为故障注入子模块 |
| 章节文档 | `docs/5.evals.md`，格式对齐 `docs/0.hello_agent.md` |

## 章节定位与叙事

> ch4 之后，系统已具备后台任务、Cron 唤醒、跨会话状态——行为空间开始爆炸，「肉眼看了能跑」不构成验收。且 ch6 将引入 Workflow DAG 与死循环风险——在制造更多复杂性之前，必须先有守门机制。本章把前四章的人工验收 prompt 升级为 CI 可守的回归测试。

## curriculum.md 集成改动

1. 目录表插入（介于 4 与原 5 之间）：

   | 5. 评测与确定性测试 | **Record/Replay 盒带**、任务集评测框架（LLM-as-judge）、轨迹指标与回归对比 | **建立守门机制**：把非确定性的 Agent 行为纳入确定性测试；为后续所有章节提供回归防线。 |

2. 原 5-9 章顺延为 6-10；全文交叉引用同步更新。
3. 「十个章节（0-9）」→「十一个章节（0-10）」；阶段标签 `s01-s17` → `s01-s19`（新章占 2 个）。
4. 产出物约定追加：ch6 起每章验收 = 新增 eval 任务集条目 + 离线回放全绿。
5. `docs/0.hello_agent.md` 中的前瞻引用顺延更新（第 4→第 5 章等）。

## 模块架构

### 目录结构

```text
bytemaker/
├── src/eval/
│   ├── mod.rs           # 章节门面：EvalRunner、eval CLI 子命令
│   ├── cassette.rs      # CassetteProvider：录制/回放双模 Provider
│   ├── fingerprint.rs   # 请求指纹：归一化 + 易变字段掩码
│   ├── fault.rs         # FaultyProvider：故障注入（429/超时/畸形 JSON）
│   ├── suite.rs         # 任务集定义（YAML）与运行器
│   ├── judge.rs         # LLM-as-judge（裁判调用本身也可走盒带）
│   ├── trajectory.rs    # 轨迹采集中间件
│   └── report.rs        # 报告生成与回归对比
└── evals/
    ├── suites/*.yaml    # 任务集（种子：README 5 条示例 + 各章验收 prompt）
    ├── cassettes/*.json # 录制盒带（提交进 git，CI 离线复放）
    └── runs/            # 历次运行报告（gitignore）
```

### CassetteProvider（顺序流回放）

挂在 ch0 的 `LlmProvider` trait 上（ch0 验收 `MockProvider` 的正式演进）：

```rust
pub enum CassetteMode {
    Record { inner: Box<dyn LlmProvider>, sink: CassetteWriter },
    Replay { entries: VecDeque<CassetteEntry> },
}

pub struct CassetteProvider { mode: CassetteMode }
```

- 录制：包装真实 Provider，把每个 `(请求指纹, 响应)` 追加写入盒带 JSON。
- 回放：按序弹出响应；指纹不匹配时报错必须可行动（「第 N 次调用指纹漂移 + 易变字段 diff + 建议重录」），不允许静默假绿。
- 切换方式：环境变量 `BYTEMAKER_CASSETTE=evals/cassettes/core.json`（存在即回放；`+record` 后缀即录制）；单测可直接构造。

### 指纹归一化（fingerprint.rs）

三级处理后哈希：

1. 结构化剥离：时间戳、请求 ID、会话 ID 等已知易变字段；
2. 掩码规则：可配置正则（如 ISO 时间 → `<TS>`），规则文件与盒带同目录；
3. 剩余全文 SHA-256——真实语义变化必须重录（刻意保守）。

### 故障注入（fault.rs）

声明式故障脚本（「第 2 次调用返回 429，第 3 次返回畸形 JSON」），验证 `LlmError` 分类与重试逻辑，全程零网络。

### 评测框架（suite.rs + judge.rs）

任务集 YAML 示例：

```yaml
suite: core
tasks:
  - name: read-project-summary
    prompt: "Read the file README.md and tell me what this project is about"
    workspace: fixtures/readme/
    assertions:
      - kind: judge
        rubric: "回答准确概括了 bytemaker 是什么"
      - kind: trajectory
        max_steps: 6
        max_tokens: 4000
        forbidden_tools: [write_file]
```

- 双模式：`bytemaker eval run --suite core --replay`（离线 CI）/ `--live`（真实 LLM）；共用运行器，仅 Provider 不同。
- 裁判：judge 走 `LlmProvider` 调用，输出结构化 `(pass, score, rationale)`；裁判盒带可录制，离线评测全链路无网络。
- 报告：JSON 落盘 + 终端表格（每任务 pass/fail、步数、token、耗时）。

### 轨迹指标与回归对比（trajectory.rs + report.rs）

- 采集：轨迹采集器实现为 **ch2 的 Hooks 中间件**——旁路观测，不侵入 Agent Loop。
- 指标：步数、工具调用分布、token/成本、墙钟时间、死循环检测（相邻 N 次工具调用签名重复）。
- 对比：`bytemaker eval compare runs/A.json runs/B.json` ——成功率差、token 涨幅阈值告警、新增死循环模式。

## 验收标准

1. 盒带往返一致性：录制真实任务，断网回放重跑，轨迹完全一致。
2. 指纹漂移显式失败：改 prompt 一词后回放旧盒带，得到可行动报错，非静默通过/panic。
3. CI 离线全绿：无 API key 环境 `cargo test` 通过，含 ≥3 个盒带端到端测试。
4. 故障注入验证韧性：注入「429×2 后成功」，断言重试正确且总调用 = 3，零网络。
5. 评测闭环：`eval run --suite core --replay` 输出结构化报告（≥5 种子任务）；judge 离线打分可复现。
6. 回归捕获演示：故意弄坏一个工具，`eval compare` 明确标出成功率下降与失败任务。

## 演进预留

- 多 Agent 协作断言（ch7/ch8 之后扩展 suite schema，不动核心）。
- MCP 外部进程响应不在盒带管辖内，工具层录制留待演进。
- CI 门禁（阈值拦截、盒带陈旧检测）→ ch9 生产化。
- 对比报告升级为趋势面板 → ch9 可观测性。

## 交付物

1. `docs/curriculum.md`：插入新章行 + 全文重编号 + 标签约定更新。
2. `docs/5.evals.md`：完整章节文档（对齐 ch0 格式）。
3. `docs/0.hello_agent.md`：前瞻引用顺延更新。
