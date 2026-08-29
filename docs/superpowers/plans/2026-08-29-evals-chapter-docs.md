# 新增第 5 章「评测与确定性测试基建」文档 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把已确认的设计（`docs/superpowers/specs/2026-08-29-evals-chapter-design.md`）落地为文档：更新 `docs/curriculum.md`（插入新第 5 章 + 全文重编号）、创建 `docs/5.evals.md`、更新 `docs/0.hello_agent.md` 前瞻引用。

**Architecture:** 纯文档变更，无代码。三个任务按依赖排序：先改大纲（确立编号），再写章节详解，最后同步 ch0 引用。每个任务独立提交。

**Tech Stack:** Markdown（GFM 表格）、与既有文档一致的中文技术写作风格。

## Global Constraints

- 全部文档为中文，风格与 `docs/0.hello_agent.md` 对齐（编号小节、Why 叙事、代码块、验收标准）。
- 编号映射（必须全文一致，不得遗漏）：原 5→6（单体编排）、原 6→7（插件化）、原 7→8（多 Agent）、原 8→9（生产化）、原 9→10（进阶）；第 0-4 章不变；新第 5 章 = 评测与确定性测试。
- git 阶段标签约定：`s01 到 s17` → `s01 到 s19`（新章占 2 个）；`prod01..prod05` 不变。
- 章节文档命名对齐现有约定：`docs/5.evals.md`（仿 `docs/0.hello_agent.md`）。
- 不得改动 `docs/superpowers/specs/` 下的 spec 文件。
- 提交信息用英文 conventional commits，结尾加 `Co-Authored-By: Claude Code <noreply@anthropic.com>`。

---

### Task 1: 更新 curriculum.md（插入新第 5 章并重编号）

**Files:**
- Modify: `docs/curriculum.md:3,13,22-26,30-31`

**Interfaces:**
- Consumes: spec 中已确认的新章行文案、编号映射、标签数。
- Produces: 新的大纲编号体系，Task 2/3 的文档内容必须与此编号一致。

- [ ] **Step 1: 更新章节数描述（第 3 行与第 13 行）**

第 3 行：
```markdown
> 本系列教程将分为十一个章节（0-10），每章包含建什么、引入哪些文件与依赖、怎么验收。
```

第 13 行：
```markdown
十一个章节各占一个目录，内含该章的子模块（`ch0X.md`）。
```

- [ ] **Step 2: 替换目录表中第 5-9 章的行块为新的第 5 章 + 顺延的 6-10 章**

将现有五行（原 5-9 章）整体替换为以下六行：

```markdown
| 5. 评测与确定性测试 | **Record/Replay 盒带**、任务集评测框架（LLM-as-judge）、轨迹指标与回归对比 | **建立守门机制**：把非确定性的 Agent 行为纳入确定性测试；为后续所有章节提供回归防线。 |
| 6. 单体编排与收束 | Integrated Harness、Workflow (DAG)、Goal | **复杂任务解构**：基于有向无环图实现任务依赖规划，确保单体 Agent 能收敛到最终目标，避免死循环。 |
| 7. 插件化与标准化生态 | **Skills 抽象层**、**MCP Client 集成** | **突破 Rust 编译边界**：将能力从进程内剥离，通过标准协议动态挂载外部生态（Python 脚本、本地文件系统）。 |
| 8. 多 Agent 协作与网络 | Subagent 路由、Teams 网络、Actor 消息通信 | **状态隔离与并发**：拆分上下文域；采用 Actor 模型与无锁通道（Channel）实现独立 Agent 的心跳维持与联邦协作。 |
| 9. 生产化基建 | 可观测性 (Tracing)、韧性 (熔断)、元数据注册 | **解决系统黑盒与高可用**：分布式追踪落盘；引入基于 etcd 的 Leader 选举与节点发现机制以支持远程 Agent。 |
| 10. 进阶拓展 | 多模态输入、**极致性能调优 (SIMD/Tokenizer)** | **压榨硬件性能**：本地轻量级计算的指令级优化（如针对 128 维向量距离计算的 f32x8 SIMD 优化）与 Tokenizer 效率。 |
```

- [ ] **Step 3: 更新阶段标签约定（第 30 行）**

```markdown
* 每章结束都有一版**能跑的 bytemaker**；核心机制章每章一个 git 阶段标签（`s01` 到 `s19`），学生可逐章 `git checkout s0X` 对照最小实现。生产化与进阶章在 `s19` 之上增量提交，另起 `prod01`..`prod05` 标签。
```

- [ ] **Step 4: 在产出物约定中追加评测铁律（第 31 行之后新增一条 bullet）**

```markdown
* 自第 6 章起，每章验收追加一条铁律：**新增 eval 任务集条目 + 离线回放全绿**——新能力必须先有失败模式测试，再有实现。
```

- [ ] **Step 5: 验证编号一致性**

Run: `grep -nE '^\| [0-9]+\.' docs/curriculum.md`
Expected: 11 行，编号 0-10 各一次，无跳号、无重复；第 5 章为「评测与确定性测试」。

Run: `grep -nE 's17|十个章节|十大章节' docs/curriculum.md`
Expected: 无输出（旧文案已全部清除）。

- [ ] **Step 6: Commit**

```bash
git add docs/curriculum.md
git commit -m "docs(curriculum): insert ch5 evals chapter, renumber ch5-9 to ch6-10"
```

---

### Task 2: 创建 docs/5.evals.md 章节详解

**Files:**
- Create: `docs/5.evals.md`

**Interfaces:**
- Consumes: Task 1 确立的编号体系（引用第 0/1/2/4/6/8/9 章时的编号）。
- Produces: 章节详解文档，后续章节文档将引用其约定（`evals/` 目录、`BYTEMAKER_CASSETTE` 环境变量、`bytemaker eval` 子命令）。

- [ ] **Step 1: 写入完整章节文档**

写入以下完整内容：

````markdown
# 第 5 章：评测与确定性测试基建 (Evaluation & Deterministic Testing) —— 守门机制

## 1. 模块定位与核心目标

到第 4 章为止，`bytemaker` 已具备后台任务、Cron 定时唤醒与跨会话状态——行为空间开始爆炸。此时「我肉眼看了，能跑」已经不构成验收：改一行 system prompt 可能悄悄弄坏 compact 的行为，而没有任何手段发现。更关键的是，第 6 章将引入 Workflow DAG 与死循环风险——**在制造更多复杂性之前，必须先有守门机制**。

本章把前四章的人工验收 prompt 全部升级为 CI 可守的回归测试。

**核心目标：**

1. **确定性测试基建**：录制真实 LLM 交互为盒带（Cassette），离线确定性复放，让 Agent 端到端测试摆脱网络、费用与非确定性。
2. **评测框架**：任务集（YAML）+ 运行器 + LLM-as-judge，输出可对比的结构化成功率报告。
3. **轨迹指标与回归对比**：步数、Token/成本、工具调用分布、死循环检测；两次运行的 diff 能把回归显式指出来。

## 2. 架构决策与技术选型（Why we do this）

### 2.1 为什么是「盒带」而不是手写 Mock

Agent 端到端测试存在两难：

* **真实 LLM 调用**：非确定（温度>0、模型版本漂移）、慢、花钱，CI 里不可行。
* **手写 Mock 响应**：与真实供应商响应结构漂移大，覆盖不了真实 JSON 的边角情况，维护成本随测试数量线性增长。

盒带（Cassette，VCR.js / pytest-recording 验证过的模式）取两者之长：**录制一次真实交互，之后无限次离线确定性复放**。真实结构由录制保证，测试速度由回放保证。

### 2.2 回放策略：顺序流 + 分级指纹校验

盒带是**有序的 `(请求指纹, 响应)` 列表**，回放时按调用顺序吐出响应，同时校验指纹。为什么不用「请求哈希 → 响应」的 map（乱序安全、并发安全）？因为 Agent 的 prompt 是持续演化的：上下文里混入一个时间戳就会 miss，盒带极脆、重录频繁，维护成本不可接受。

顺序流的代价是带随机分支时可能错位——但指纹校验恰好把错位变成**显式失败**（带 diff 的明确报错），而不是静默假绿。这是刻意的设计保守性：任何真实语义变化（prompt 改动、消息序列变化）都必须重录。

### 2.3 一切挂在防腐层上：三种模式，一个 Trait

第 0 章的 `LlmProvider` trait 在本章迎来最大红利：**录制、回放、在线评测共用同一个运行器，唯一区别是注入哪个 Provider**。

```
Record 模式:  OpenAiProvider ──► CassetteWriter ──► evals/cassettes/core.json
Replay 模式:  CassetteProvider（读盒带，零网络）
Live   模式:  OpenAiProvider（真实调用，测真实成功率）
```

裁判（judge）本身也是一次 `LlmProvider` 调用，因此裁判的响应同样可以录制进盒带——离线模式下**评测全链路无网络**。

### 2.4 轨迹采集：中间件而不是侵入

轨迹采集器实现为**第 2 章的 Hooks 中间件**——旁路观测，不侵入 Agent Loop 一行代码。这是第 2 章中间件体系的第一次大规模回收利用，也意味着死循环检测（相邻 N 次工具调用签名重复）天然对 Workflow（第 6 章）和 Subagent（第 8 章）生效。

### 2.5 故障注入：手写 fixture 的正确归宿

手写响应不适合模拟正常流（漂移大），却是模拟**错误路径**的最佳工具：手写响应反而最真实——没有供应商会为你稳定地复现 429。`FaultyProvider` 用声明式故障脚本（「第 2 次调用返回 429，第 3 次返回畸形 JSON」）验证第 0 章的 `LlmError` 分类与重试逻辑，全程零网络。

## 3. 核心接口与数据流设计

### 3.1 CassetteProvider：录制/回放双模 Provider

第 0 章验收中的 `MockProvider`（返回写死字符串）在本章正式演进为盒带 Provider：

```rust
pub enum CassetteMode {
    Record {
        inner: Box<dyn LlmProvider>,
        sink: CassetteWriter,
    },
    Replay {
        entries: VecDeque<CassetteEntry>,
    },
}

pub struct CassetteProvider {
    mode: CassetteMode,
}

impl LlmProvider for CassetteProvider {
    // Record: 转发给 inner，落盘 (指纹, 响应) 后透传结果
    // Replay: 弹出下一条 entry，校验指纹后返回响应
}
```

盒带文件为 JSONL，每行一条交互记录：

```json
{"fingerprint": "sha256:9f2c...", "request": {...}, "response": {...}, "model": "deepseek-chat", "recorded_at": "2026-08-29T10:00:00Z"}
```

模式由环境变量切换：`BYTEMAKER_CASSETTE=evals/cassettes/core.json`（文件存在即回放；加 `+record` 后缀即录制）。单元测试中也可直接构造 `CassetteProvider`，不依赖环境变量。

**回放错位必须报可行动的错误**：

```text
error: cassette drift at call #3 of evals/cassettes/core.json
  expected fingerprint: sha256:9f2c...  (masked: <TS>, <UUID>)
  actual   fingerprint: sha256:41aa...
  drift in message[2].content: "Read README.md and summarize" vs "Read README.md and summarise"
  → prompt semantics changed; re-record with BYTEMAKER_CASSETTE=...+record
```

### 3.2 请求指纹：三级归一化（fingerprint.rs）

对请求消息做三级处理后再哈希：

```rust
pub fn fingerprint(
    messages: &[Message],
    masks: &MaskRules,
) -> Fingerprint {
    // 1. 结构化剥离：时间戳、请求 ID、会话 ID 等已知易变字段
    // 2. 掩码规则：可配置正则（如 ISO8601 时间 → <TS>、UUID → <UUID>）
    // 3. 剩余全文 SHA-256
}
```

掩码规则文件与盒带同目录（`evals/cassettes/masks.yaml`），新增易变字段时只加规则、不重录：

```yaml
masks:
  - { pattern: '\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z', replace: '<TS>' }
  - { pattern: '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}', replace: '<UUID>' }
```

### 3.3 任务集 Schema（suite.rs）

任务集用 YAML 声明，种子来自 README 的 5 条示例 prompt 与前四章的验收 prompt：

```yaml
suite: core
tasks:
  - name: read-project-summary
    prompt: "Read the file README.md and tell me what this project is about"
    workspace: fixtures/readme/        # 运行前复制到临时目录
    assertions:
      - kind: judge                    # LLM-as-judge 裁判
        rubric: "回答准确概括了 bytemaker 是什么"
      - kind: trajectory               # 轨迹断言
        max_steps: 6
        max_tokens: 4000
        forbidden_tools: [write_file]  # 只读任务不许写文件
```

断言分两类：`judge`（语义正确性，由裁判打分）与 `trajectory`（行为约束，纯规则、零成本）。

### 3.4 评测运行器与裁判（mod.rs + judge.rs）

```bash
bytemaker eval run --suite core --replay   # 离线，CI 用
bytemaker eval run --suite core --live     # 真实 LLM，测真实成功率
```

两种模式共用运行器，仅 Provider 注入不同。裁判的提示词要求输出结构化结论：

```rust
pub struct JudgeVerdict {
    pub pass: bool,
    pub score: f32,      // 0.0 - 1.0
    pub rationale: String,
}
```

报告双输出：JSON 落盘到 `evals/runs/<timestamp>-<suite>.json`（供回归对比），终端表格给人体读（每任务 pass/fail、步数、token、耗时）。

### 3.5 轨迹采集与回归对比（trajectory.rs + report.rs）

轨迹采集中间件产出事件流，聚合成指标：

```rust
pub struct TrajectoryMetrics {
    pub steps: u32,
    pub tool_calls: BTreeMap<String, u32>,  // 工具名 → 次数
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub wall_ms: u128,
    pub loop_detected: bool,                // 相邻 N 次工具调用签名重复
}
```

回归对比是本章的「终验」：

```bash
bytemaker eval compare evals/runs/run-a.json evals/runs/run-b.json
```

输出三次差：**成功率差**、**Token 成本涨幅**（超阈值告警）、**新增死循环模式**。场景演示：故意弄坏一个工具（如让 `read_file` 返回空），`compare` 必须把成功率下降与新增失败任务明确标出。

## 4. 目录结构规划

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
    ├── suites/*.yaml    # 任务集
    ├── cassettes/*.json # 录制盒带（提交进 git，CI 离线复放）
    └── runs/            # 历次运行报告（gitignore）
```

盒带**提交进 git** 而非 gitignore——它是测试资产的一部分，CI 离线复放依赖它；`runs/` 则是运行产物，必须 gitignore。

## 5. 演进预留 (What's Next)

1. **多 Agent 协作断言**：第 8 章 Teams 出现后，任务集需支持子 Agent 调用拓扑断言——届时在 suite schema 上扩展，不动核心运行器。
2. **工具层录制**：盒带目前只覆盖 `LlmProvider`；MCP 外部进程的响应不在盒带管辖内，工具层录制是更激进的方案，超出本章范围。
3. **CI 门禁**：成功率阈值拦截、盒带陈旧检测（供应商 schema 变更告警）属于生产化策略，见第 9 章。
4. **趋势面板**：对比报告目前是 CLI 表格，第 9 章可观测性落地后可升级为趋势面板。

## 6. 验收标准 (Acceptance Criteria)

完成本章后，需通过以下验收：

1. **盒带往返一致性**：录制一个真实任务（如 README 第 1 条示例 prompt），断网后回放重跑，轨迹完全一致（步数、工具调用序列、最终结果）。
2. **指纹漂移显式失败**：故意修改 prompt 中一个词后回放旧盒带，必须得到「第 N 次调用指纹漂移 + 易变字段 diff + 建议重录」的明确报错——而不是静默通过或含糊 panic。
3. **CI 离线全绿**：无 `OPENAI_API_KEY` 的环境下 `cargo test` 全部通过，其中包含至少 3 个走盒带的 Agent 端到端测试。
4. **故障注入验证韧性**：用 `FaultyProvider` 注入「429 两次后成功」，断言重试逻辑正确工作且总调用次数 = 3，全程零网络。
5. **评测闭环**：`bytemaker eval run --suite core --replay` 输出结构化报告（≥5 个种子任务）；judge 离线打分可复现。
6. **回归捕获演示**：故意弄坏一个工具（如让 `read_file` 返回空），`bytemaker eval compare` 必须把成功率下降与新增失败任务明确标出。
````

- [ ] **Step 2: 验证文档完整性**

Run: `grep -c '^## ' docs/5.evals.md`
Expected: `6`（模块定位 / 架构决策 / 核心接口 / 目录结构 / 演进预留 / 验收标准，与 ch0 文档同构）。

Run: `grep -nE '第 [0-9]+ 章' docs/5.evals.md`
Expected: 只出现第 0、1、2、4、6、8、9 章（重编号后的引用），不得出现「第 5 章」自引用或旧编号（如把编排章叫第 5 章、多 Agent 叫第 7 章）。

- [ ] **Step 3: Commit**

```bash
git add docs/5.evals.md
git commit -m "docs: add ch5 evals & deterministic testing chapter"
```

---

### Task 3: 更新 docs/0.hello_agent.md 前瞻引用

**Files:**
- Modify: `docs/0.hello_agent.md:18,149`

**Interfaces:**
- Consumes: Task 1 的编号映射（原第 4 章持久化 → 第 5 章；原第 7 章多 Agent → 第 8 章；新第 5 章 = 评测）。
- Produces: 与新大纲一致的 ch0 文档。

- [ ] **Step 1: 顺延第 18 行的前瞻引用**

将：
```markdown
* **设计约束**：整个 LLM 交互过程必须是非阻塞的 `async/await`，为第 4 章的并发 Task Graph 和第 7 章的 Actor 消息通信提供底层运行时。
```
改为：
```markdown
* **设计约束**：整个 LLM 交互过程必须是非阻塞的 `async/await`，为第 5 章的并发 Task Graph 和第 8 章的 Actor 消息通信提供底层运行时。
```

- [ ] **Step 2: 在「演进预留」补第 3 条（MockProvider → 盒带）**

在第 149 行（「**结构化缺失**」条目）之后追加：

```markdown
3. **可测性演进**：本章验收中的 `MockProvider` 只能返回写死字符串；第 5 章会把它演进为支持录制/回放的 `CassetteProvider`，让 Agent 端到端测试摆脱真实 API 依赖。
```

- [ ] **Step 3: 验证引用无残留旧编号**

Run: `grep -nE '第 [0-9]+ 章' docs/0.hello_agent.md`
Expected: 第 1 章（×2，不变）、第 5 章（Task Graph，新）、第 5 章（Cassette，新）、第 8 章（Actor，新）；不得再出现「第 4 章」或「第 7 章」。

- [ ] **Step 4: Commit**

```bash
git add docs/0.hello_agent.md
git commit -m "docs(ch0): renumber forward references for new ch5 evals chapter"
```

---

## Self-Review 结论

* **Spec 覆盖**：curriculum.md 五处改动（决策表 1-5）→ Task 1 全覆盖；模块架构与验收标准 → Task 2 全文；ch0 引用顺延 → Task 3。spec 交付物清单三项均有着落。
* **占位符扫描**：无 TBD/TODO；Task 2 含完整文档全文，Task 1/3 含精确的旧文/新文对照。
* **一致性**：编号映射在三个任务间一致；`s19`、`BYTEMAKER_CASSETTE`、`docs/5.evals.md` 等命名与 spec 逐字一致。
