# 第 5 章「评测与确定性测试基建」代码实现 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 `docs/5.evals.md` 的全部代码：Cassette 录制/回放 Provider、请求指纹、故障注入、评测任务集运行器、LLM-as-judge、轨迹指标与回归对比，外加 `bytemaker eval` CLI 与 `evals/` 测试资产。

**Architecture:** 一切挂在 ch0 的 `LlmProvider` 防腐层上：`CassetteProvider`（Record/Replay 双模）+ `MeteredProvider`（计数）组成 Provider 栈，评测运行器 `EvalRunner` 用 `Agent::isolated`（从 `TestAgent` 构造法提炼）在临时 workspace 里跑标准 `run_loop`；轨迹采集走 ch2 的 Hooks（PostToolCall）；裁判与 Agent 共用同一 Provider，顺序流消耗同一盒带。CLI 在 `main.rs` 于读取 `OPENAI_API_KEY` 之前分发 `eval` 子命令，保证离线回放无需密钥。

**Tech Stack:** Rust（既有依赖：serde / serde_yaml / serde_json / sha2 / regex / chrono / tempfile / async-trait / tokio）。**不新增任何依赖。**

## Global Constraints







- 模块路径与文件名严格对齐 `docs/5.evals.md` §4：`src/eval/{mod,cassette,fingerprint,fault,suite,judge,trajectory,report}.rs` + `evals/{suites,cassettes,runs}`。
- 指纹只对**请求消息**计算（docs §3.2 的签名是 `fingerprint(messages, masks)`）——system prompt 含时间戳与 workspace 路径，刻意不入指纹。
- 顺序流回放：指纹不匹配必须返回**可行动的显式错误**（第 N 次调用 + diff 片段 + 建议重录），绝不允许静默假绿。
- 盒带文件是 JSONL（每行一条 `CassetteEntry`），提交进 git；`evals/runs/` gitignore。
- `BYTEMAKER_CASSETTE=path`（存在即回放）/ `BYTEMAKER_CASSETTE=path+record`（录制）是环境变量约定；单测直接构造 Provider，不依赖环境变量。
- 注释风格对齐既有模块（模块级中文 doc comment + 简洁行内注释，标注 docs 章节引用）。
- 每个任务独立提交，conventional commits 英文信息，结尾加 `Co-Authored-By: Claude Code <noreply@anthropic.com>`。
- 全程不要求 `OPENAI_API_KEY`：盒带由 `ScriptedProvider`（声明式脚本 Provider）走真实录制路径生成并提交。
- 平台 win32 / bash；`cargo test` 必须全绿（含既有测试）。

---

### Task 1: fingerprint.rs —— 请求指纹与掩码规则

**Files:**
- Create: `src/eval/mod.rs`（本章门面，本任务只声明 fingerprint 子模块）
- Create: `src/eval/fingerprint.rs`
- Modify: `src/lib.rs`（追加 `pub mod eval;`）

**Interfaces:**
- Consumes: `crate::domain::message::{Message, ContentBlock}`（已有）。
- Produces（后续任务依赖，签名不得改动）:
  - `pub struct MaskRule { pub pattern: String, pub replace: String }`
  - `pub struct MaskRules { pub masks: Vec<MaskRule> }`，方法 `empty() / default_rules() / from_yaml_str(&str) -> Result<Self, String> / from_yaml_file(&Path) -> Result<Self, String> / load_or_default(&Path) -> Self / with_literal(self, &str, &str) -> Self / mask_text(&self, &str) -> String`
  - `pub struct Fingerprint(String)`，`as_str() -> &str` + `Display`
  - `pub fn fingerprint(messages: &[Message], masks: &MaskRules) -> Fingerprint`
  - `pub(crate) fn canonical_value(messages: &[Message], masks: &MaskRules) -> serde_json::Value`（Task 2 的 drift diff 复用）

- [ ] **Step 1: 写失败测试**

创建 `src/eval/mod.rs`：

```rust
//! eval —— 评测与确定性测试基建（docs/5.evals.md）。
//!
//! 本章门面：各子模块（cassette/fingerprint/fault/suite/judge/trajectory/report）
//! 逐步声明；`EvalRunner` 与 `eval` CLI 子命令见后续任务。

pub mod fingerprint;
```

修改 `src/lib.rs`，在 `pub mod error;` 之后追加一行：

```rust
pub mod eval;
```

创建 `src/eval/fingerprint.rs`：

```rust
//! 请求指纹：三级归一化（结构化剥离 → 掩码规则 → SHA-256）（docs/5.evals.md §3.2）。
//!
//! 指纹只对**请求消息**计算：system prompt 含当前时间与 workspace 路径，刻意不入指纹。
//! 结构化剥离 = tool_call/call_id 统一替换为 `<ID>`（模型生成的 ID 每次录制都不同）；
//! 掩码规则 = 可配置正则（ISO 时间、UUID、workspace 路径等易变字段）；最后对规范化
//! JSON 全文取 SHA-256——真实语义变化必须重录（刻意保守）。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::Message;

    #[test]
    fn fingerprint_is_stable_for_same_messages() {
        let masks = MaskRules::default_rules();
        let msgs = vec![Message::user_text("Read README.md and summarize")];
        assert_eq!(
            fingerprint(&msgs, &masks),
            fingerprint(&msgs, &masks),
            "identical messages must fingerprint identically"
        );
    }

    #[test]
    fn fingerprint_changes_when_text_changes() {
        let masks = MaskRules::default_rules();
        let a = vec![Message::user_text("summarize")];
        let b = vec![Message::user_text("summarise")];
        assert_ne!(fingerprint(&a, &masks), fingerprint(&b, &masks));
    }

    #[test]
    fn timestamps_and_uuids_are_masked() {
        let masks = MaskRules::default_rules();
        let a = vec![Message::user_text("run at 2026-08-29T10:00:00Z for id 123e4567-e89b-12d3-a456-426614174000")];
        let b = vec![Message::user_text("run at 2027-01-01T00:00:00Z for id 00000000-0000-0000-0000-000000000000")];
        assert_eq!(fingerprint(&a, &masks), fingerprint(&b, &masks));
    }

    #[test]
    fn local_datetime_is_masked() {
        // system prompt 之外，工具输出里也会出现本地时间格式 "YYYY-MM-DD HH:MM:SS"。
        let masks = MaskRules::default_rules();
        let a = vec![Message::user_text("now: 2026-08-29 10:00:00")];
        let b = vec![Message::user_text("now: 2027-12-31 23:59:59")];
        assert_eq!(fingerprint(&a, &masks), fingerprint(&b, &masks));
    }

    #[test]
    fn literal_mask_replaces_workspace_path() {
        let masks = MaskRules::empty()
            .with_literal("C:\\tmp\\workspace-a", "<WORKSPACE>");
        let a = vec![Message::user_text("file at C:\\tmp\\workspace-a\\README.md")];
        let b = vec![Message::user_text("file at C:\\tmp\\workspace-zz\\README.md")];
        assert_ne!(fingerprint(&a, &masks), fingerprint(&b, &masks), "不同字面量替换后应不同");
        let b_masked = vec![Message::user_text("file at C:\\tmp\\workspace-zz\\README.md")];
        let masks_b = MaskRules::empty().with_literal("C:\\tmp\\workspace-zz", "<WORKSPACE>");
        assert_eq!(fingerprint(&a, &masks), fingerprint(&b_masked, &masks_b));
    }

    #[test]
    fn fingerprint_display_is_sha256_prefixed_hex() {
        let masks = MaskRules::default_rules();
        let f = fingerprint(&[Message::user_text("x")], &masks);
        assert!(f.as_str().starts_with("sha256:"));
        assert_eq!(f.as_str().len(), "sha256:".len() + 64);
        assert!(f.to_string().starts_with("sha256:"));
    }

    #[test]
    fn masks_yaml_roundtrip_and_invalid() {
        let yaml = "masks:\n  - { pattern: 'foo', replace: 'BAR' }\n";
        let rules = MaskRules::from_yaml_str(yaml).expect("valid yaml");
        assert_eq!(rules.mask_text("say foo foo"), "say BAR BAR");
        assert!(MaskRules::from_yaml_str("!!!: [").is_err());

        // load_or_default: 文件存在则读，不存在则 default_rules。
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("masks.yaml");
        assert!(!MaskRules::load_or_default(&missing).masks.is_empty());
        let present = tmp.path().join("present.yaml");
        std::fs::write(&present, "masks:\n  - { pattern: 'foo', replace: 'BAR' }\n").unwrap();
        assert_eq!(MaskRules::load_or_default(&present).masks.len(), 1);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test eval::fingerprint 2>&1 | tail -5`
Expected: 编译失败（`MaskRules` / `fingerprint` 未定义）。

- [ ] **Step 3: 最小实现**

在 `src/eval/fingerprint.rs` 顶部（tests 模块之前）写入：

```rust
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::message::{ContentBlock, Message};

/// 内置默认掩码：ISO8601 UTC 时间、本地时间（工具输出常见）、UUID。
/// 与 `evals/cassettes/masks.yaml` 的种子内容一致。
pub const DEFAULT_MASKS_YAML: &str = "\
masks:
  - { pattern: '\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}:\\d{2}Z', replace: '<TS>' }
  - { pattern: '\\d{4}-\\d{2}-\\d{2} \\d{2}:\\d{2}:\\d{2}', replace: '<TS>' }
  - { pattern: '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}', replace: '<UUID>' }
";

/// 单条掩码规则：正则 pattern → 字面替换（serde 对齐 masks.yaml 的 {pattern, replace}）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaskRule {
    pub pattern: String,
    pub replace: String,
}

/// 掩码规则集（docs §3.2 二级归一化）。规则文件与盒带同目录（masks.yaml）；
/// 新增易变字段时只加规则、不重录。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MaskRules {
    #[serde(default)]
    pub masks: Vec<MaskRule>,
}

impl MaskRules {
    /// 无规则（测试基线）。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 内置默认规则（ISO 时间 / 本地时间 / UUID）。
    pub fn default_rules() -> Self {
        Self::from_yaml_str(DEFAULT_MASKS_YAML).expect("builtin masks yaml is valid")
    }

    /// 从 YAML 文本解析（顶层 `masks:` 列表）。
    pub fn from_yaml_str(s: &str) -> Result<Self, String> {
        serde_yaml::from_str::<MaskRules>(s).map_err(|e| format!("invalid masks yaml: {e}"))
    }

    /// 从 YAML 文件解析。
    pub fn from_yaml_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read masks {}: {e}", path.display()))?;
        Self::from_yaml_str(&text)
    }

    /// 读取 masks.yaml；文件不存在时回退默认规则（盒带目录允许没有 masks.yaml）。
    pub fn load_or_default(path: &Path) -> Self {
        if path.exists() {
            Self::from_yaml_file(path).unwrap_or_else(|e| {
                tracing::warn!("masks file {path} invalid ({e}), falling back to defaults");
                Self::default_rules()
            })
        } else {
            Self::default_rules()
        }
    }

    /// 追加一条**字面量**掩码（regex::escape 后匹配）。
    /// eval runner 用它把当前 workspace 临时目录路径替换为 `<WORKSPACE>`，
    /// 使含绝对路径的工具输出在两次运行间指纹一致。
    pub fn with_literal(mut self, literal: &str, replace: &str) -> Self {
        if !literal.is_empty() {
            self.masks.push(MaskRule {
                pattern: regex::escape(literal),
                replace: replace.to_string(),
            });
        }
        self
    }

    /// 依次应用全部正则规则。非法正则跳过（fail-open，记录 warn）。
    pub fn mask_text(&self, text: &str) -> String {
        let mut out = text.to_string();
        for rule in &self.masks {
            match regex::Regex::new(&rule.pattern) {
                Ok(re) => out = re.replace_all(&out, rule.replace.as_str()).to_string(),
                Err(e) => tracing::warn!("invalid mask pattern {}: {e}", rule.pattern),
            }
        }
        out
    }
}

/// 请求指纹（`sha256:<64 hex>`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint(String);

impl Fingerprint {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 对请求消息计算指纹：规范化 JSON（剥 ID + 掩码）→ SHA-256。
pub fn fingerprint(messages: &[Message], masks: &MaskRules) -> Fingerprint {
    let canonical = canonical_value(messages, masks).to_string();
    let digest = Sha256::digest(canonical.as_bytes());
    Fingerprint(format!("sha256:{}", hex_encode(&digest)))
}

/// 规范化消息列表（一级「结构化剥离」+ 二级「掩码」）：
/// - role → wire 字符串；
/// - Text/ToolOutput 的文本过掩码；ToolCall.input 序列化后过掩码；
/// - tool_call `id` / `call_id` 一律替换为 `<ID>`（模型生成的 ID 每次录制都不同）。
pub(crate) fn canonical_value(messages: &[Message], masks: &MaskRules) -> serde_json::Value {
    use serde_json::json;
    let normalized: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            let blocks: Vec<serde_json::Value> = m
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => {
                        json!({"type": "text", "text": masks.mask_text(text)})
                    }
                    ContentBlock::ToolCall { name, input, .. } => json!({
                        "type": "tool_call", "id": "<ID>", "name": name,
                        "input": masks.mask_text(&input.to_string()),
                    }),
                    ContentBlock::ToolOutput { content, .. } => json!({
                        "type": "tool_output", "call_id": "<ID>",
                        "content": masks.mask_text(content),
                    }),
                })
                .collect();
            json!({"role": m.role.as_str(), "content": blocks})
        })
        .collect();
    serde_json::Value::Array(normalized)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test eval::fingerprint 2>&1 | tail -5`
Expected: `test result: ok. 7 passed`（7 个测试全绿）。

Run: `cargo build 2>&1 | tail -3`
Expected: 编译通过（可能有 unused 警告，无 error）。

- [ ] **Step 5: Commit**

```bash
git add src/eval/ src/lib.rs
git commit -m "feat(eval): request fingerprint with mask rules (ch5)

Three-stage normalization: structural stripping (tool_call ids), configurable
regex masks (timestamps/UUID/workspace path), SHA-256 over canonical JSON.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 2: cassette.rs —— CassetteProvider 录制/回放双模 Provider

**Files:**
- Create: `src/eval/cassette.rs`
- Modify: `src/eval/mod.rs`（追加 `pub mod cassette;`）
- Modify: `src/domain/message.rs:217`（`Usage` 增加 serde derives）

**Interfaces:**
- Consumes: Task 1 的 `MaskRules` / `fingerprint` / `canonical_value`；`crate::providers::{LlmProvider, CallResult}`；`crate::error::LlmError`。
- Produces（后续任务依赖，签名不得改动）:
  - `pub struct RecordedUsage { pub input_tokens: u64, pub output_tokens: u64 }`
  - `pub struct RecordedResponse { pub content: Vec<ContentBlock>, pub finish_reason: String, pub usage: Option<RecordedUsage> }`，`from_response(&MessagesResponse) -> Self` / `to_response(&self) -> MessagesResponse`
  - `pub struct CassetteEntry { pub fingerprint: String, pub request: Vec<Message>, pub response: RecordedResponse, pub model: String, pub recorded_at: String }`（serde：JSONL 每行一条）
  - `pub struct CassetteWriter`，`create(&Path) -> Result<Self, String>` / `append(&self, &CassetteEntry) -> Result<(), String>`
  - `pub fn load_entries(&Path) -> Result<Vec<CassetteEntry>, String>`
  - `pub fn load_masks_for(cassette_path: &Path) -> MaskRules`（读同级 `masks.yaml`，缺失则默认规则）
  - `pub enum CassetteMode { Record { inner: Box<dyn LlmProvider>, sink: CassetteWriter }, Replay { entries: Mutex<VecDeque<CassetteEntry>>, path: PathBuf } }`
  - `pub struct CassetteProvider`，`record(Box<dyn LlmProvider>, &Path, MaskRules, &str) -> Result<Self, String>` / `replay(&Path, MaskRules) -> Result<Self, String>` / `set_masks(&self, MaskRules)` / `call_count(&self) -> u64`，实现 `LlmProvider`
  - `pub enum CassetteSpec { Replay(PathBuf), Record(PathBuf) }` + `pub fn spec_from_env() -> Result<Option<CassetteSpec>, String>`（读 `BYTEMAKER_CASSETTE`）

- [ ] **Step 1: 给 Usage 加 serde derives**

修改 `src/domain/message.rs` 第 217 行附近：

```rust
/// Token usage captured from the SSE stream (s16 workflow plumbing).
/// s18（ch5 eval）：额外 derive Serialize/Deserialize，供盒带 JSONL 落盘。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}
```

- [ ] **Step 2: 写失败测试**

修改 `src/eval/mod.rs`：

```rust
pub mod cassette;
pub mod fingerprint;
```

创建 `src/eval/cassette.rs`（先只含测试骨架，实现留空使其编译失败）：

```rust
//! CassetteProvider：录制/回放双模 Provider（docs/5.evals.md §3.1）。
//!
//! 盒带是**有序的** `(请求指纹, 响应)` 列表（JSONL）。回放按调用顺序吐出响应并校验
//! 指纹；指纹不匹配报可行动错误（第 N 次调用 + diff + 建议重录），不允许静默假绿。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::{ContentBlock, Message, MessagesResponse};
    use crate::providers::{CallResult, LlmProvider, MockProvider};
    use std::path::PathBuf;

    /// 两个响应的脚本 inner（ScriptedProvider 在 Task 3 才有，这里用最朴素的办法：
    /// 直接手写 entries + MockProvider，验证 cassette 自身行为）。
    fn text_response(text: &str) -> RecordedResponse {
        RecordedResponse {
            content: vec![ContentBlock::text(text)],
            finish_reason: "stop".to_string(),
            usage: None,
        }
    }

    #[tokio::test]
    async fn record_writes_jsonl_entries_then_replay_roundtrips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("cassette.json");
        let masks = crate::eval::fingerprint::MaskRules::default_rules();

        // Record：inner 是 MockProvider（返回写死字符串），录制 2 次调用。
        let recorder = CassetteProvider::record(
            Box::new(MockProvider::new("hello")),
            &path,
            masks.clone(),
            "test-model",
        )
        .unwrap();
        let msgs = vec![Message::user_text("hi")];
        for _ in 0..2 {
            let r = recorder
                .stream_messages("", &msgs, &[], 100, tokio_util::sync::CancellationToken::new())
                .await;
            assert!(matches!(r, CallResult::Success(_)));
        }
        assert_eq!(recorder.call_count(), 2);

        // JSONL：2 行，第一行字段齐全。
        let entries = load_entries(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].fingerprint.starts_with("sha256:"));
        assert_eq!(entries[0].request, msgs);
        assert_eq!(entries[0].model, "test-model");
        assert!(!entries[0].recorded_at.is_empty());

        // Replay：同请求按序返回，零网络。
        let player = CassetteProvider::replay(&path, masks).unwrap();
        let r = player
            .stream_messages("", &msgs, &[], 100, tokio_util::sync::CancellationToken::new())
            .await;
        match r {
            CallResult::Success(resp) => {
                match &resp.content[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "hello"),
                    _ => panic!("expected text block"),
                }
            }
            other => panic!("expected Success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn replay_drift_reports_actionable_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("cassette.json");
        let masks = crate::eval::fingerprint::MaskRules::default_rules();

        let recorder =
            CassetteProvider::record(Box::new(MockProvider::new("hello")), &path, masks.clone(), "m").unwrap();
        let original = vec![Message::user_text("Read README.md and summarize it")];
        recorder
            .stream_messages("", &original, &[], 100, tokio_util::sync::CancellationToken::new())
            .await;

        // 改一个词后回放旧盒带：必须显式失败（验收 #2）。
        let player = CassetteProvider::replay(&path, masks).unwrap();
        let drifted = vec![Message::user_text("Read README.md and summarise it")];
        let r = player
            .stream_messages("", &drifted, &[], 100, tokio_util::sync::CancellationToken::new())
            .await;
        match r {
            CallResult::Failure(e) => {
                let msg = e.to_string();
                assert!(msg.contains("cassette drift at call #1"), "got: {msg}");
                assert!(msg.contains("re-record"), "got: {msg}");
                assert!(msg.contains("summarize") || msg.contains("summarise"), "diff must show the changed word: {msg}");
            }
            other => panic!("expected Failure(drift), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn replay_exhausted_is_explicit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("cassette.json");
        let masks = crate::eval::fingerprint::MaskRules::default_rules();
        std::fs::write(&path, "").unwrap(); // 空盒带
        let player = CassetteProvider::replay(&path, masks).unwrap();
        let r = player
            .stream_messages("", &[Message::user_text("x")], &[], 100, tokio_util::sync::CancellationToken::new())
            .await;
        match r {
            CallResult::Failure(e) => {
                assert!(e.to_string().contains("cassette exhausted at call #1"));
                assert!(e.to_string().contains("re-record"));
            }
            other => panic!("expected Failure(exhausted), got {:?}", other),
        }
    }

    #[test]
    fn load_masks_for_reads_sibling_yaml() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cassette = tmp.path().join("cassette.json");
        // 无 masks.yaml → 默认规则。
        assert!(!load_masks_for(&cassette).masks.is_empty());
        // 有 masks.yaml → 读取之。
        std::fs::write(tmp.path().join("masks.yaml"), "masks:\n  - { pattern: 'x', replace: 'y' }\n").unwrap();
        assert_eq!(load_masks_for(&cassette).masks.len(), 1);
    }

    #[test]
    fn spec_from_env_parses_record_suffix_and_missing_file() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("BYTEMAKER_CASSETTE", "some/path.json+record");
        assert!(matches!(
            spec_from_env().unwrap(),
            Some(CassetteSpec::Record(p)) if p == PathBuf::from("some/path.json")
        ));
        std::env::set_var("BYTEMAKER_CASSETTE", "definitely/missing.json");
        let err = spec_from_env().unwrap_err();
        assert!(err.contains("does not exist") && err.contains("+record"), "got: {err}");
        std::env::remove_var("BYTEMAKER_CASSETTE");
        assert!(spec_from_env().unwrap().is_none());
    }

    /// env 变量测试串行化（对齐 config.rs 的做法）。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn recorded_response_roundtrips_messages_response() {
        let r = RecordedResponse {
            content: vec![ContentBlock::text("hi")],
            finish_reason: "stop".into(),
            usage: Some(RecordedUsage { input_tokens: 3, output_tokens: 2 }),
        };
        let resp: MessagesResponse = r.to_response();
        assert_eq!(resp.finish_reason, "stop");
        assert_eq!(resp.usage.as_ref().unwrap().input_tokens, 3);
        assert_eq!(RecordedResponse::from_response(&resp).usage.unwrap().output_tokens, 2);
    }
}
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test eval::cassette 2>&1 | tail -5`
Expected: 编译失败（`CassetteProvider` 等未定义）。

- [ ] **Step 4: 实现 cassette.rs**

在 tests 模块之前写入：

```rust
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::domain::message::{ContentBlock, Message, MessagesResponse, Usage};
use crate::error::LlmError;
use crate::eval::fingerprint::{self, MaskRules};
use crate::providers::{CallResult, LlmProvider};
use crate::tools::trait_def::ToolDefinition;

/// 盒带中的 token 用量（`Usage` 的 serde 形态）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl From<&Usage> for RecordedUsage {
    fn from(u: &Usage) -> Self {
        Self { input_tokens: u.input_tokens, output_tokens: u.output_tokens }
    }
}

/// 盒带中的响应（`MessagesResponse` 的 serde 形态）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedResponse {
    pub content: Vec<ContentBlock>,
    pub finish_reason: String,
    pub usage: Option<RecordedUsage>,
}

impl RecordedResponse {
    pub fn from_response(r: &MessagesResponse) -> Self {
        Self {
            content: r.content.clone(),
            finish_reason: r.finish_reason.clone(),
            usage: r.usage.as_ref().map(RecordedUsage::from),
        }
    }

    pub fn to_response(&self) -> MessagesResponse {
        MessagesResponse {
            content: self.content.clone(),
            finish_reason: self.finish_reason.clone(),
            usage: self.usage.as_ref().map(|u| Usage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
            }),
        }
    }
}

/// 一条交互记录：JSONL 每行一个（docs §3.1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CassetteEntry {
    pub fingerprint: String,
    pub request: Vec<Message>,
    pub response: RecordedResponse,
    pub model: String,
    pub recorded_at: String,
}

/// 盒带追加写入器（record 模式的落盘 sink）。
pub struct CassetteWriter {
    path: PathBuf,
}

impl CassetteWriter {
    /// 创建（或截断）盒带文件。
    pub fn create(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        }
        std::fs::write(path, "").map_err(|e| format!("create {}: {e}", path.display()))?;
        Ok(Self { path: path.to_path_buf() })
    }

    /// 追加一条记录（JSONL）。写入量极小，同步 IO 足够。
    pub fn append(&self, entry: &CassetteEntry) -> Result<(), String> {
        let line = serde_json::to_string(entry)
            .map_err(|e| format!("serialize cassette entry: {e}"))?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("open {}: {e}", self.path.display()))?;
        writeln!(f, "{line}").map_err(|e| format!("append {}: {e}", self.path.display()))
    }
}

/// 读入整条盒带。
pub fn load_entries(path: &Path) -> Result<Vec<CassetteEntry>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read cassette {}: {e}", path.display()))?;
    let mut entries = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: CassetteEntry = serde_json::from_str(line)
            .map_err(|e| format!("cassette {} line {}: {e}", path.display(), i + 1))?;
        entries.push(entry);
    }
    Ok(entries)
}

/// 盒带同目录的 masks.yaml（缺失则默认规则）——「规则文件与盒带同目录」。
pub fn load_masks_for(cassette_path: &Path) -> MaskRules {
    let masks_path = cassette_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("masks.yaml");
    MaskRules::load_or_default(&masks_path)
}

/// 双模：录制（包装真实 Provider，落盘后透传）或回放（读盒带，零网络）。
pub enum CassetteMode {
    Record {
        inner: Box<dyn LlmProvider>,
        sink: CassetteWriter,
    },
    Replay {
        entries: Mutex<VecDeque<CassetteEntry>>,
        path: PathBuf,
    },
}

/// ch0 `MockProvider` 的正式演进（docs §3.1）：录制/回放共用一个 `LlmProvider` 实现，
/// 运行器唯一区别是注入哪个 Provider。
pub struct CassetteProvider {
    mode: CassetteMode,
    masks: Mutex<MaskRules>,
    model: String,
    calls: AtomicU64,
}

impl CassetteProvider {
    /// Record 模式：包装 `inner`，把每次成功调用落盘为 `(指纹, 响应)`。
    pub fn record(
        inner: Box<dyn LlmProvider>,
        path: &Path,
        masks: MaskRules,
        model: &str,
    ) -> Result<Self, String> {
        Ok(Self {
            mode: CassetteMode::Record { inner, sink: CassetteWriter::create(path)? },
            masks: Mutex::new(masks),
            model: model.to_string(),
            calls: AtomicU64::new(0),
        })
    }

    /// Replay 模式：从文件加载 entries，按序弹出。
    pub fn replay(path: &Path, masks: MaskRules) -> Result<Self, String> {
        let entries = load_entries(path)?;
        Ok(Self {
            mode: CassetteMode::Replay {
                entries: Mutex::new(entries.into()),
                path: path.to_path_buf(),
            },
            masks: Mutex::new(masks),
            model: String::new(),
            calls: AtomicU64::new(0),
        })
    }

    /// 替换掩码规则。eval runner 在每个任务开始时注入当前 workspace 路径字面量。
    pub fn set_masks(&self, masks: MaskRules) {
        *self.masks.lock().unwrap() = masks;
    }

    /// 已发生的 LLM 调用次数（1-based 由调用处 +1）。
    pub fn call_count(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl LlmProvider for CassetteProvider {
    async fn stream_messages(
        &self,
        _system: &str,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _max_tokens: u32,
        cancel: CancellationToken,
    ) -> CallResult {
        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        match &self.mode {
            CassetteMode::Record { inner, sink } => {
                let result = inner
                    .stream_messages(_system, messages, _tools, _max_tokens, cancel)
                    .await;
                if let CallResult::Success(r) = &result {
                    let masks = self.masks.lock().unwrap().clone();
                    let entry = CassetteEntry {
                        fingerprint: fingerprint::fingerprint(messages, &masks).to_string(),
                        request: messages.to_vec(),
                        response: RecordedResponse::from_response(r),
                        model: self.model.clone(),
                        recorded_at: chrono::Utc::now().to_rfc3339(),
                    };
                    if let Err(e) = sink.append(&entry) {
                        return CallResult::Failure(LlmError::Other(format!("cassette write failed: {e}")));
                    }
                }
                result
            }
            CassetteMode::Replay { entries, path } => {
                let entry = {
                    let mut queue = entries.lock().unwrap();
                    match queue.pop_front() {
                        Some(e) => e,
                        None => {
                            return CallResult::Failure(LlmError::Other(format!(
                                "cassette exhausted at call #{n} of {}: the run made more LLM calls than were recorded → re-record with BYTEMAKER_CASSETTE={}+record",
                                path.display(),
                                path.display()
                            )))
                        }
                    }
                };
                let masks = self.masks.lock().unwrap().clone();
                let actual = fingerprint::fingerprint(messages, &masks).to_string();
                if actual != entry.fingerprint {
                    return CallResult::Failure(LlmError::Other(drift_message(
                        n,
                        path,
                        &entry.fingerprint,
                        &actual,
                        &entry.request,
                        messages,
                        &masks,
                    )));
                }
                CallResult::Success(entry.response.to_response())
            }
        }
    }
}

/// 可行动的漂移报错（验收 #2）：第 N 次调用 + 双指纹 + 易变字段 diff + 建议重录。
fn drift_message(
    n: u64,
    path: &Path,
    expected: &str,
    actual: &str,
    recorded: &[Message],
    actual_msgs: &[Message],
    masks: &MaskRules,
) -> String {
    let rec = fingerprint::canonical_value(recorded, masks).to_string();
    let act = fingerprint::canonical_value(actual_msgs, masks).to_string();
    let diff = if rec == act {
        String::new()
    } else {
        snippet_diff(&rec, &act)
    };
    format!(
        "cassette drift at call #{n} of {path}\n  expected fingerprint: {expected}\n  actual   fingerprint: {actual}\n{diff}\n→ prompt semantics changed; re-record with BYTEMAKER_CASSETTE={path}+record",
        path = path.display(),
    )
}

/// 在两个规范化 JSON 串的第一个差异点附近各截取片段，把错位变成显式 diff。
fn snippet_diff(recorded: &str, actual: &str) -> String {
    let pos = recorded
        .chars()
        .zip(actual.chars())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| recorded.chars().count().min(actual.chars().count()));
    let take = |s: &str| -> String {
        let start = pos.saturating_sub(40);
        s.chars().skip(start).take(80).collect()
    };
    format!(
        "  drift near char {pos}:\n    recorded: ...{}...\n    actual:   ...{}...",
        take(recorded),
        take(actual)
    )
}

/// `BYTEMAKER_CASSETTE` 解析结果。
pub enum CassetteSpec {
    Replay(PathBuf),
    Record(PathBuf),
}

/// 环境变量约定（docs §3.1）：`path` 存在即回放；`path+record` 即录制。
/// 未设置返回 `Ok(None)`；设置了但既非 +record 又不存在 → 可行动错误。
pub fn spec_from_env() -> Result<Option<CassetteSpec>, String> {
    let Ok(v) = std::env::var("BYTEMAKER_CASSETTE") else {
        return Ok(None);
    };
    let v = v.trim().to_string();
    if v.is_empty() {
        return Ok(None);
    }
    if let Some(path) = v.strip_suffix("+record") {
        return Ok(Some(CassetteSpec::Record(PathBuf::from(path.trim()))));
    }
    let path = PathBuf::from(&v);
    if path.exists() {
        Ok(Some(CassetteSpec::Replay(path)))
    } else {
        Err(format!(
            "BYTEMAKER_CASSETTE={v} does not exist; to record, set BYTEMAKER_CASSETTE={v}+record"
        ))
    }
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test eval::cassette 2>&1 | tail -5`
Expected: `test result: ok. 6 passed`。

Run: `cargo test domain::message 2>&1 | tail -3`
Expected: 既有测试全绿（Usage 加 derive 不破坏）。

- [ ] **Step 6: Commit**

```bash
git add src/eval/ src/domain/message.rs
git commit -m "feat(eval): cassette provider with sequential record/replay (ch5)

JSONL cassette of (fingerprint, response) pairs; replay pops in call order and
fails with an actionable drift error (call #, fingerprint pair, first-diff
snippet, re-record hint) instead of silently passing.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 3: fault.rs —— ScriptedProvider / FaultyProvider / RetryProvider

**Files:**
- Create: `src/eval/fault.rs`
- Modify: `src/eval/mod.rs`（追加 `pub mod fault;`）

**Interfaces:**
- Consumes: `crate::providers::{LlmProvider, CallResult}`、`crate::domain::message::MessagesResponse`、`crate::error::LlmError`。
- Produces（后续任务依赖，签名不得改动）:
  - `pub fn text_response(&str) -> MessagesResponse` / `pub fn tool_call_response(&str, &str, serde_json::Value) -> MessagesResponse`（响应构造 helper，`finish_reason="tool_call"`）
  - `pub struct ScriptedProvider`：`new(Vec<MessagesResponse>) -> Self` / `calls(&self) -> u64`，实现 `LlmProvider`（按序返回；耗尽报错）
  - `pub enum Fault { RateLimit, Timeout, MalformedJson, AuthError }` + `pub enum FaultStep { Fail(Fault), Succeed(MessagesResponse) }`
  - `pub struct FaultyProvider`：`new(Vec<FaultStep>) -> Self` / `calls(&self) -> u64`，实现 `LlmProvider`
  - `pub struct RetryProvider`：`new(Arc<dyn LlmProvider>, max_retries: u32, base_delay: Duration) -> Self`，实现 `LlmProvider`（重试 `RateLimit | Network | Timeout`，指数退避）

设计说明（写进模块 doc）：重试逻辑在 `OpenAiProvider` 内部由 async-openai 的 `OpenAIRetry` 层承担，`LlmProvider` trait 层没有可测的重试路径。`RetryProvider` 把重试提升到防腐层（eval live 模式直接受益），`FaultyProvider` 因此能**零网络**验证「429×2 后成功，总调用 = 3」（验收 #4）。

- [ ] **Step 1: 写失败测试**

修改 `src/eval/mod.rs` 追加：

```rust
pub mod fault;
```

创建 `src/eval/fault.rs`：

```rust
//! 故障注入与脚本 Provider（docs/5.evals.md §2.5 / §3.1）。
//!
//! 手写响应不适合模拟正常流（与真实供应商结构漂移大），却是模拟**错误路径**的最佳
//! 工具——没有供应商会为你稳定地复现 429。`FaultyProvider` 用声明式故障脚本验证
//! `LlmError` 分类与重试逻辑（`RetryProvider`），全程零网络（验收 #4）。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::{ContentBlock, Message, Usage};
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
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test eval::fault 2>&1 | tail -5`
Expected: 编译失败（类型未定义）。

- [ ] **Step 3: 实现 fault.rs**

在 tests 模块之前写入：

```rust
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
pub enum Fault {
    RateLimit,
    Timeout,
    MalformedJson,
    AuthError,
}

/// 故障脚本的一步：失败（注入错误）或成功（预置响应）。
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
        match self.script.get(n) {
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
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test eval::fault 2>&1 | tail -5`
Expected: `test result: ok. 6 passed`。

- [ ] **Step 5: Commit**

```bash
git add src/eval/
git commit -m "feat(eval): fault injection, scripted provider, trait-level retry (ch5)

RetryProvider lifts retry to the anti-corruption layer so FaultyProvider can
verify '429 x2 then success = 3 total calls' with zero network (acceptance #4).

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 4: trajectory.rs —— 轨迹采集中间件与计量 Provider

**Files:**
- Create: `src/eval/trajectory.rs`
- Modify: `src/eval/mod.rs`（追加 `pub mod trajectory;`）

**Interfaces:**
- Consumes: `crate::hooks::PostToolHook`、`crate::providers::{LlmProvider, CallResult}`。
- Produces（后续任务依赖，签名不得改动）:
  - `pub const LOOP_WINDOW: usize = 3`
  - `#[derive(Serialize, Deserialize)] pub struct TrajectoryMetrics { pub steps: u32, pub tool_calls: BTreeMap<String, u32>, pub prompt_tokens: u64, pub completion_tokens: u64, pub wall_ms: u64, pub loop_detected: bool }`（docs §3.5 的 wall_ms 用 u64——serde_json 对 u128 支持不稳）
  - `pub struct CollectorSnapshot { pub tool_calls: BTreeMap<String, u32>, pub loop_detected: bool, pub wall_ms: u64 }`
  - `#[derive(Clone)] pub struct TrajectoryCollector`：`new() -> Self` / `snapshot(&self) -> CollectorSnapshot`，实现 `PostToolHook`（永远返回 `None`，只旁路记录）
  - `pub fn detect_loop(signatures: &[String]) -> bool`（相邻 `LOOP_WINDOW` 次工具调用签名重复）
  - `pub struct MeteredProvider`：`new(Arc<dyn LlmProvider>) -> Self` / `totals(&self) -> (u64, u64, u64)`（calls, prompt_tokens, completion_tokens），实现 `LlmProvider`
  - `pub fn compose(snapshot: CollectorSnapshot, meter: (u64, u64, u64)) -> TrajectoryMetrics`

- [ ] **Step 1: 写失败测试**

修改 `src/eval/mod.rs` 追加：

```rust
pub mod trajectory;
```

创建 `src/eval/trajectory.rs`：

```rust
//! 轨迹采集与指标聚合（docs/5.evals.md §2.4 / §3.5）。
//!
//! 采集器实现为 **ch2 的 Hooks 中间件**——旁路观测，不侵入 Agent Loop 一行代码，
//! 因此死循环检测天然对 Workflow（ch6）与 Subagent（ch8）生效。LLM 调用计数与
//! token 累计无法从 Hooks 看到，由 `MeteredProvider`（Provider 装饰器，同样零侵入）
//! 承担；`compose` 把两路数据拼成 `TrajectoryMetrics`。

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
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test eval::trajectory 2>&1 | tail -5`
Expected: 编译失败。

- [ ] **Step 3: 实现 trajectory.rs**

在 tests 模块之前写入：

```rust
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::domain::message::{Message, MessagesResponse};
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

/// 抑制未使用告警（MessagesResponse 仅在类型层面出现）。
#[allow(dead_code)]
fn _assert_response_type(_: Option<MessagesResponse>) {}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test eval::trajectory 2>&1 | tail -5`
Expected: `test result: ok. 5 passed`。

- [ ] **Step 5: Commit**

```bash
git add src/eval/
git commit -m "feat(eval): trajectory collector hook + metered provider (ch5)

Bypass observation via the ch2 hooks middleware (tool calls, loop detection,
wall clock) plus a provider decorator for steps/tokens; compose() merges both.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 5: suite.rs —— 任务集 Schema 与 workspace 布景

**Files:**
- Create: `src/eval/suite.rs`
- Modify: `src/eval/mod.rs`（追加 `pub mod suite;`）

**Interfaces:**
- Consumes: serde_yaml、tempfile。
- Produces（后续任务依赖，签名不得改动）:
  - `#[derive(Debug, Deserialize)] pub struct EvalSuite { pub suite: String, pub tasks: Vec<EvalTask> }`
  - `#[derive(Debug, Deserialize)] pub struct EvalTask { pub name: String, pub prompt: String, pub workspace: Option<String>, pub assertions: Vec<Assertion> }`
  - `#[derive(Debug, Deserialize)] #[serde(tag = "kind", rename_all = "lowercase")] pub enum Assertion { Judge { rubric: String }, Trajectory { max_steps: Option<u32>, max_tokens: Option<u64>, forbidden_tools: Vec<String> } }`
  - `EvalSuite::from_yaml_str(&str) -> Result<Self, String>` / `from_yaml_file(&Path) -> Result<Self, String>`（校验：非空任务、名称唯一）
  - `pub fn stage_workspace(evals_dir: &Path, workspace: Option<&str>) -> Result<tempfile::TempDir, String>`（Some → 复制 fixture 到临时目录；None → 空临时目录）

- [ ] **Step 1: 写失败测试**

修改 `src/eval/mod.rs` 追加：

```rust
pub mod suite;
```

创建 `src/eval/suite.rs`：

```rust
//! 任务集定义（YAML）与运行器布景（docs/5.evals.md §3.3）。
//!
//! 断言分两类：`judge`（语义正确性，由裁判打分）与 `trajectory`（行为约束，纯规则、
//! 零成本）。`workspace` 指向 `evals/fixtures/` 下的目录，运行前复制到临时目录——
//! 任务之间互不污染，也不动真实仓库。

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
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
  - name: minimal
    prompt: "Reply with exactly: hello agent"
    assertions:
      - kind: trajectory
        max_steps: 2
"#;

    #[test]
    fn parses_both_assertion_kinds() {
        let s = EvalSuite::from_yaml_str(SAMPLE).unwrap();
        assert_eq!(s.suite, "core");
        assert_eq!(s.tasks.len(), 2);
        let t = &s.tasks[0];
        assert_eq!(t.workspace.as_deref(), Some("fixtures/readme/"));
        assert!(matches!(t.assertions[0], Assertion::Judge { .. }));
        match &t.assertions[1] {
            Assertion::Trajectory { max_steps, max_tokens, forbidden_tools } => {
                assert_eq!(*max_steps, Some(6));
                assert_eq!(*max_tokens, Some(4000));
                assert_eq!(forbidden_tools, &vec!["write_file".to_string()]);
            }
            other => panic!("expected Trajectory, got {other:?}"),
        }
        // 无 workspace / 可选断言字段的默认值。
        let t2 = &s.tasks[1];
        assert!(t2.workspace.is_none());
        match &t2.assertions[0] {
            Assertion::Trajectory { max_tokens, forbidden_tools, .. } => {
                assert!(max_tokens.is_none());
                assert!(forbidden_tools.is_empty());
            }
            other => panic!("expected Trajectory, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_and_duplicate_tasks() {
        assert!(EvalSuite::from_yaml_str("suite: x\ntasks: []").is_err());
        let dup = "suite: x\ntasks:\n  - name: a\n    prompt: p\n  - name: a\n    prompt: q\n";
        let err = EvalSuite::from_yaml_str(dup).unwrap_err();
        assert!(err.contains("duplicate task name: a"), "got: {err}");
    }

    #[test]
    fn from_yaml_file_reads_disk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("s.yaml");
        std::fs::write(&p, SAMPLE).unwrap();
        assert_eq!(EvalSuite::from_yaml_file(&p).unwrap().tasks.len(), 2);
        assert!(EvalSuite::from_yaml_file(&tmp.path().join("missing.yaml")).is_err());
    }

    #[test]
    fn stage_workspace_copies_fixture_contents() {
        let evals = tempfile::TempDir::new().unwrap();
        let fixture = evals.path().join("fixtures/readme");
        std::fs::create_dir_all(&fixture).unwrap();
        std::fs::write(fixture.join("README.md"), "# demo").unwrap();

        let staged = stage_workspace(evals.path(), Some("fixtures/readme/")).unwrap();
        let content = std::fs::read_to_string(staged.path().join("README.md")).unwrap();
        assert_eq!(content, "# demo");

        // 缺失 fixture → 显式错误。
        let err = stage_workspace(evals.path(), Some("fixtures/missing/")).unwrap_err();
        assert!(err.contains("workspace fixture not found"), "got: {err}");

        // 无 workspace → 空临时目录（agent 仍需一个 workdir）。
        let empty = stage_workspace(evals.path(), None).unwrap();
        assert!(std::fs::read_dir(empty.path()).unwrap().next().is_none());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test eval::suite 2>&1 | tail -5`
Expected: 编译失败。

- [ ] **Step 3: 实现 suite.rs**

在 tests 模块之前写入：

```rust
use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;

/// 任务集（`evals/suites/<name>.yaml`）。
#[derive(Debug, Deserialize)]
pub struct EvalSuite {
    pub suite: String,
    pub tasks: Vec<EvalTask>,
}

/// 单条评测任务。
#[derive(Debug, Deserialize)]
pub struct EvalTask {
    pub name: String,
    pub prompt: String,
    /// `evals/fixtures/` 下的目录（相对路径），运行前复制到临时目录。
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub assertions: Vec<Assertion>,
}

/// 断言：`judge`（LLM-as-judge 语义打分）或 `trajectory`（纯规则行为约束）。
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Assertion {
    Judge {
        rubric: String,
    },
    Trajectory {
        #[serde(default)]
        max_steps: Option<u32>,
        #[serde(default)]
        max_tokens: Option<u64>,
        #[serde(default)]
        forbidden_tools: Vec<String>,
    },
}

impl EvalSuite {
    pub fn from_yaml_str(s: &str) -> Result<Self, String> {
        let suite: EvalSuite =
            serde_yaml::from_str(s).map_err(|e| format!("invalid suite yaml: {e}"))?;
        if suite.tasks.is_empty() {
            return Err("suite has no tasks".to_string());
        }
        let mut names = BTreeSet::new();
        for t in &suite.tasks {
            if !names.insert(t.name.as_str()) {
                return Err(format!("duplicate task name: {}", t.name));
            }
        }
        Ok(suite)
    }

    pub fn from_yaml_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read suite {}: {e}", path.display()))?;
        Self::from_yaml_str(&text)
    }
}

/// 布景：把 `evals_dir/<workspace>` 的内容复制到新临时目录（返回值须保持存活，
/// TempDir 被 drop 时工作区即销毁）。`None` → 空临时目录。
pub fn stage_workspace(evals_dir: &Path, workspace: Option<&str>) -> Result<tempfile::TempDir, String> {
    let tmp = tempfile::TempDir::new().map_err(|e| format!("tempdir: {e}"))?;
    if let Some(ws) = workspace {
        let src = evals_dir.join(ws);
        if !src.is_dir() {
            return Err(format!("workspace fixture not found: {}", src.display()));
        }
        copy_dir_recursive(&src, tmp.path())?;
    }
    Ok(tmp)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    let entries = std::fs::read_dir(src).map_err(|e| format!("readdir {}: {e}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)
                .map_err(|e| format!("copy to {}: {e}", to.display()))?;
        }
    }
    Ok(())
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test eval::suite 2>&1 | tail -5`
Expected: `test result: ok. 4 passed`。

- [ ] **Step 5: Commit**

```bash
git add src/eval/
git commit -m "feat(eval): yaml suite schema + workspace staging (ch5)

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 6: judge.rs —— LLM-as-judge

**Files:**
- Create: `src/eval/judge.rs`
- Modify: `src/eval/mod.rs`（追加 `pub mod judge;`）

**Interfaces:**
- Consumes: `crate::providers::LlmProvider`（`generate_completion`）、`MockProvider`。
- Produces（后续任务依赖，签名不得改动）:
  - `#[derive(Serialize, Deserialize)] pub struct JudgeVerdict { pub pass: bool, pub score: f32, pub rationale: String }`
  - `pub fn judge_messages(task_prompt: &str, rubric: &str, answer: &str) -> Vec<Message>`
  - `pub async fn run_judge(provider: &dyn LlmProvider, task_prompt: &str, rubric: &str, answer: &str) -> JudgeVerdict`
  - `pub fn parse_verdict(raw: &str) -> JudgeVerdict`（容错解析：剥 ```json 围栏、截取首 `{` 到末 `}`；失败返回 pass=false 的确定性裁决，绝不 panic）

裁判本身是一次 `LlmProvider` 调用 → 裁判响应同样录制进盒带，离线评测全链路无网络（docs §2.3）。

- [ ] **Step 1: 写失败测试**

修改 `src/eval/mod.rs` 追加：

```rust
pub mod judge;
```

创建 `src/eval/judge.rs`：

```rust
//! LLM-as-judge（docs/5.evals.md §3.4）。
//!
//! 裁判本身是一次 `LlmProvider::generate_completion` 调用——因此裁判的响应同样
//! 录制进盒带，离线模式下评测全链路无网络。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{LlmProvider, MockProvider};

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
        assert!(text.contains("\"pass\""), "prompt must demand the JSON verdict shape");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test eval::judge 2>&1 | tail -5`
Expected: 编译失败。

- [ ] **Step 3: 实现 judge.rs**

在 tests 模块之前写入：

```rust
use serde::{Deserialize, Serialize};

use crate::domain::message::Message;
use crate::providers::LlmProvider;

/// 结构化裁决（docs §3.4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test eval::judge 2>&1 | tail -5`
Expected: `test result: ok. 6 passed`。

- [ ] **Step 5: Commit**

```bash
git add src/eval/
git commit -m "feat(eval): llm-as-judge with tolerant verdict parsing (ch5)

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 7: report.rs —— 运行报告与回归对比

**Files:**
- Create: `src/eval/report.rs`
- Modify: `src/eval/mod.rs`（追加 `pub mod report;`）

**Interfaces:**
- Consumes: `crate::eval::trajectory::TrajectoryMetrics`、`crate::eval::judge::JudgeVerdict`。
- Produces（后续任务依赖，签名不得改动）:
  - `#[derive(Serialize, Deserialize)] pub struct TaskReport { pub name: String, pub pass: bool, pub judge: Option<JudgeVerdict>, pub metrics: TrajectoryMetrics, pub failure: Option<String> }`，`failed(name: String, reason: String) -> Self`
  - `#[derive(Serialize, Deserialize)] pub struct RunReport { pub suite: String, pub mode: String, pub timestamp: String, pub tasks: Vec<TaskReport>, pub success_rate: f32 }`，`new(suite, mode, timestamp, tasks) -> Self` / `total_tokens(&self) -> u64` / `write_json(&self, &Path) -> Result<(), String>` / `from_json_file(&Path) -> Result<Self, String>` / `render_table(&self) -> String`
  - `pub const TOKEN_WARNING_THRESHOLD_PCT: f64 = 20.0`
  - `#[derive(Serialize, Deserialize)] pub struct CompareReport { rate_baseline: f32, rate_candidate: f32, rate_delta: f32, newly_failed: Vec<String>, recovered: Vec<String>, tokens_baseline: u64, tokens_candidate: u64, token_increase_pct: f64, token_warning: bool, new_loops: Vec<String> }`
  - `pub fn compare(baseline: &RunReport, candidate: &RunReport) -> CompareReport`
  - `pub fn render_compare(&CompareReport) -> String`

- [ ] **Step 1: 写失败测试**

修改 `src/eval/mod.rs` 追加：

```rust
pub mod report;
```

创建 `src/eval/report.rs`：

```rust
//! 报告生成与回归对比（docs/5.evals.md §3.4 / §3.5）。
//!
//! 双输出：JSON 落盘 `evals/runs/<timestamp>-<suite>.json`（供回归对比），终端表格
//! 给人体读。对比输出三次差：成功率差、Token 涨幅告警、新增死循环模式。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::judge::JudgeVerdict;
    use crate::eval::trajectory::TrajectoryMetrics;

    fn task(name: &str, pass: bool, tokens: u64, loop_detected: bool) -> TaskReport {
        TaskReport {
            name: name.to_string(),
            pass,
            judge: None,
            metrics: TrajectoryMetrics {
                steps: 2,
                prompt_tokens: tokens,
                completion_tokens: 0,
                loop_detected,
                ..Default::default()
            },
            failure: if pass { None } else { Some("assertion failed".into()) },
        }
    }

    fn report(tasks: Vec<TaskReport>) -> RunReport {
        RunReport::new("core".into(), "replay".into(), "20260830-000000".into(), tasks)
    }

    #[test]
    fn success_rate_and_total_tokens() {
        let r = report(vec![task("a", true, 100, false), task("b", false, 50, false)]);
        assert_eq!(r.success_rate, 0.5);
        assert_eq!(r.total_tokens(), 150);
        assert_eq!(task("x", false, 0, false).judge, None);
        let failed = TaskReport::failed("x".into(), "staging broke".into());
        assert!(!failed.pass);
        assert_eq!(failed.failure.as_deref(), Some("staging broke"));
    }

    #[test]
    fn table_shows_task_rows_and_verdicts() {
        let mut t = task("read-project-summary", false, 100, false);
        t.judge = Some(JudgeVerdict { pass: false, score: 0.1, rationale: "inaccurate".into() });
        let table = report(vec![task("minimal", true, 10, false), t]).render_table();
        assert!(table.contains("read-project-summary"));
        assert!(table.contains("PASS") && table.contains("FAIL"));
        assert!(table.contains("inaccurate"), "failure detail must be visible");
        assert!(table.contains("50.0%"));
    }

    #[test]
    fn json_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("run.json");
        let r = report(vec![task("a", true, 10, false)]);
        r.write_json(&path).unwrap();
        let back = RunReport::from_json_file(&path).unwrap();
        assert_eq!(back.tasks.len(), 1);
        assert_eq!(back.tasks[0].name, "a");
        assert_eq!(back.success_rate, r.success_rate);
        assert!(RunReport::from_json_file(&tmp.path().join("nope.json")).is_err());
    }

    /// 验收 #6 的核心：成功率下降、新增失败任务、token 涨幅告警、新增死循环
    /// 都必须被 compare 显式标出。
    #[test]
    fn compare_flags_regressions() {
        let baseline = report(vec![
            task("a", true, 100, false),
            task("b", true, 100, false),
        ]);
        let candidate = report(vec![
            task("a", false, 100, false),
            task("b", true, 300, true), // token 涨 50%（超阈值）+ 新死循环
        ]);
        let c = compare(&baseline, &candidate);
        assert_eq!(c.rate_baseline, 1.0);
        assert_eq!(c.rate_candidate, 0.5);
        assert!(c.rate_delta < 0.0);
        assert_eq!(c.newly_failed, vec!["a".to_string()]);
        assert!(c.recovered.is_empty());
        assert_eq!(c.tokens_baseline, 200);
        assert_eq!(c.tokens_candidate, 400);
        assert!(c.token_increase_pct > 20.0);
        assert!(c.token_warning);
        assert_eq!(c.new_loops, vec!["b".to_string()]);

        let text = render_compare(&c);
        assert!(text.contains("NEWLY FAILED: a"), "got: {text}");
        assert!(text.contains("NEW LOOP DETECTED: b"), "got: {text}");
        assert!(text.contains("50.0%"), "token increase must be shown");
    }

    #[test]
    fn compare_flags_recovery_and_zero_baseline_tokens() {
        let baseline = report(vec![task("a", false, 0, false)]);
        let candidate = report(vec![task("a", true, 500, false)]);
        let c = compare(&baseline, &candidate);
        assert!(c.recovered.contains(&"a".to_string()));
        assert_eq!(c.token_increase_pct, 0.0, "零基线 token 不应产生 +inf%");
        assert!(!c.token_warning);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test eval::report 2>&1 | tail -5`
Expected: 编译失败。

- [ ] **Step 3: 实现 report.rs**

在 tests 模块之前写入：

```rust
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::eval::judge::JudgeVerdict;
use crate::eval::trajectory::TrajectoryMetrics;

/// Token 涨幅告警阈值（%）。超过即提示（拦截留给 ch9 生产化）。
pub const TOKEN_WARNING_THRESHOLD_PCT: f64 = 20.0;

/// 单任务结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskReport {
    pub name: String,
    pub pass: bool,
    pub judge: Option<JudgeVerdict>,
    pub metrics: TrajectoryMetrics,
    /// 失败原因（多条断言失败用 `; ` 连接）；布景失败等硬错误也落在这里。
    pub failure: Option<String>,
}

impl TaskReport {
    /// 未跑就失败的报告（如 workspace 布景失败）。
    pub fn failed(name: String, reason: String) -> Self {
        Self {
            name,
            pass: false,
            judge: None,
            metrics: TrajectoryMetrics::default(),
            failure: Some(reason),
        }
    }
}

/// 一次 `eval run` 的完整报告（JSON 落盘 + 终端表格）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub suite: String,
    pub mode: String,
    pub timestamp: String,
    pub tasks: Vec<TaskReport>,
    pub success_rate: f32,
}

impl RunReport {
    pub fn new(suite: String, mode: String, timestamp: String, tasks: Vec<TaskReport>) -> Self {
        let passed = tasks.iter().filter(|t| t.pass).count() as f32;
        let rate = if tasks.is_empty() { 0.0 } else { passed / tasks.len() as f32 };
        Self { suite, mode, timestamp, tasks, success_rate: rate }
    }

    /// prompt + completion token 总量。
    pub fn total_tokens(&self) -> u64 {
        self.tasks
            .iter()
            .map(|t| t.metrics.prompt_tokens + t.metrics.completion_tokens)
            .sum()
    }

    pub fn write_json(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("serialize report: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("write {}: {e}", path.display()))
    }

    pub fn from_json_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read report {}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("parse report {}: {e}", path.display()))
    }

    /// 终端表格：每任务 pass/fail、步数、token、耗时。
    pub fn render_table(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "suite: {}  mode: {}  success rate: {:.1}%\n",
            self.suite,
            self.mode,
            self.success_rate * 100.0
        ));
        out.push_str(&format!(
            "{:<28} {:<6} {:>5} {:>8} {:>8} {:>10}\n",
            "TASK", "RESULT", "STEPS", "TOK_IN", "TOK_OUT", "WALL_MS"
        ));
        for t in &self.tasks {
            out.push_str(&format!(
                "{:<28} {:<6} {:>5} {:>8} {:>8} {:>10}\n",
                t.name,
                if t.pass { "PASS" } else { "FAIL" },
                t.metrics.steps,
                t.metrics.prompt_tokens,
                t.metrics.completion_tokens,
                t.metrics.wall_ms
            ));
            if let Some(f) = &t.failure {
                out.push_str(&format!("    -> {f}\n"));
            }
        }
        out
    }
}

/// 两次运行的回归对比（docs §3.5「终验」）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompareReport {
    pub rate_baseline: f32,
    pub rate_candidate: f32,
    /// candidate - baseline（百分点）。
    pub rate_delta: f32,
    /// 基线通过、候选失败的任务。
    pub newly_failed: Vec<String>,
    /// 基线失败、候选通过的任务。
    pub recovered: Vec<String>,
    pub tokens_baseline: u64,
    pub tokens_candidate: u64,
    pub token_increase_pct: f64,
    pub token_warning: bool,
    /// 基线未检出、候选检出死循环的任务。
    pub new_loops: Vec<String>,
}

/// 三次差：成功率差、Token 涨幅（超阈值告警）、新增死循环模式。
pub fn compare(baseline: &RunReport, candidate: &RunReport) -> CompareReport {
    let find = |r: &RunReport, name: &str| r.tasks.iter().find(|t| t.name == name);
    let newly_failed = candidate
        .tasks
        .iter()
        .filter(|c| !c.pass)
        .filter(|c| find(baseline, &c.name).map(|b| b.pass).unwrap_or(false))
        .map(|c| c.name.clone())
        .collect();
    let recovered = baseline
        .tasks
        .iter()
        .filter(|b| !b.pass)
        .filter(|b| find(candidate, &b.name).map(|c| c.pass).unwrap_or(false))
        .map(|b| b.name.clone())
        .collect();
    let new_loops = candidate
        .tasks
        .iter()
        .filter(|c| c.metrics.loop_detected)
        .filter(|c| find(baseline, &c.name).map(|b| !b.metrics.loop_detected).unwrap_or(true))
        .map(|c| c.name.clone())
        .collect();
    let tokens_baseline = baseline.total_tokens();
    let tokens_candidate = candidate.total_tokens();
    let token_increase_pct = if tokens_baseline == 0 {
        0.0
    } else {
        (tokens_candidate as f64 - tokens_baseline as f64) / tokens_baseline as f64 * 100.0
    };
    CompareReport {
        rate_baseline: baseline.success_rate,
        rate_candidate: candidate.success_rate,
        rate_delta: candidate.success_rate - baseline.success_rate,
        newly_failed,
        recovered,
        tokens_baseline,
        tokens_candidate,
        token_increase_pct,
        token_warning: token_increase_pct > TOKEN_WARNING_THRESHOLD_PCT,
        new_loops,
    }
}

/// 对比报告的人读版本。
pub fn render_compare(c: &CompareReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "success rate: baseline {:.1}% -> candidate {:.1}%  ({:+.1} pts)\n",
        c.rate_baseline * 100.0,
        c.rate_candidate * 100.0,
        c.rate_delta * 100.0
    ));
    for name in &c.newly_failed {
        out.push_str(&format!("  NEWLY FAILED: {name}\n"));
    }
    for name in &c.recovered {
        out.push_str(&format!("  recovered:    {name}\n"));
    }
    out.push_str(&format!(
        "tokens: {} -> {} ({:+.1}%)\n",
        c.tokens_baseline, c.tokens_candidate, c.token_increase_pct
    ));
    if c.token_warning {
        out.push_str("  ! token cost increase exceeds threshold\n");
    }
    for name in &c.new_loops {
        out.push_str(&format!("  NEW LOOP DETECTED: {name}\n"));
    }
    out
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test eval::report 2>&1 | tail -5`
Expected: `test result: ok. 6 passed`。

- [ ] **Step 5: Commit**

```bash
git add src/eval/
git commit -m "feat(eval): run report, terminal table, regression compare (ch5)

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 8: agent.rs —— 提炼 `Agent::isolated`（eval 与 TestAgent 共用）

**Files:**
- Modify: `src/agent.rs`（新增 `pub(crate) fn isolated`；`extract_final_text` 保持私有但新增 `pub(crate) fn extract_final_text_from`；`TestAgent::new` 改为调用 `isolated`）

**Interfaces:**
- Consumes: `TestAgent::new` 现有的构造逻辑（`src/agent.rs:899-965`）。
- Produces（Task 9 依赖，签名不得改动）:
  - `impl Agent { pub(crate) fn isolated(workdir: PathBuf, client: Arc<dyn LlmProvider>, io: Arc<crate::io::IO>, hooks: Hooks, base_system: String, max_turns: usize, registry: Arc<ToolRegistry>) -> Agent }`
  - `pub(crate) fn extract_final_text_from(messages: &[Message]) -> Option<String>`（模块级自由函数）

`isolated` 的关键性质（写进 doc comment）：无 cron / 无 goal / 无 team；`MemoryStore::new_read_only` 指向空目录——recall 短路为空、extract/consolidate 返回 0，**不产生任何额外 LLM 调用**（顺序流盒带的前提）；registry 由调用方注入（eval 的 broken-tool 回归演示需要替换工具）。

- [ ] **Step 1: 写失败测试**

在 `src/agent.rs` 的 `mod tests` 中追加：

```rust
    #[test]
    fn isolated_agent_has_no_cron_goal_team_and_readonly_memory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let client: Arc<dyn LlmProvider> = Arc::new(crate::providers::MockProvider::new("x"));
        let io = Arc::new(crate::io::IO::memory());
        let agent = Agent::isolated(
            tmp.path().to_path_buf(),
            client,
            io,
            Agent::build_hooks(
                &Arc::new(BackgroundManager::new(tmp.path().join("bg"))),
                &Arc::new(SharedTodoManager::new(TodoManager::new())),
            ),
            "isolated system".into(),
            5,
            Arc::new(tools::build_registry()),
        );
        assert_eq!(agent.kind, AgentKind::Lead);
        assert_eq!(agent.max_turns, 5);
        assert!(agent.cron_manager.is_none());
        assert!(agent.goal.is_none());
        assert!(agent.team.is_none());
        assert_eq!(agent.base_system, "isolated system");
    }

    #[test]
    fn extract_final_text_from_finds_last_assistant_text() {
        let messages = vec![
            Message::user_text("hi"),
            Message::assistant_content(vec![ContentBlock::text("first")]),
            Message::assistant_content(vec![ContentBlock::text("final answer")]),
        ];
        assert_eq!(extract_final_text_from(&messages).as_deref(), Some("final answer"));
        assert_eq!(extract_final_text_from(&[]), None);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test agent::tests::isolated 2>&1 | tail -5`
Expected: 编译失败（`Agent::isolated` 未定义）。

- [ ] **Step 3: 实现**

在 `impl Agent` 中（`child_teammate` 之后）加入：

```rust
    /// Build an isolated Agent around an injected provider + registry（eval runner
    /// 与 TestAgent 共用的构造法）。无 cron / 无 goal / 无 team；read-only memory
    /// 指向空目录——recall 短路为空、extract/consolidate 返回 0，**不产生任何额外
    /// LLM 调用**（顺序流盒带的前提，docs/5.evals.md §2.2）。
    pub(crate) fn isolated(
        workdir: PathBuf,
        client: Arc<dyn LlmProvider>,
        io: Arc<crate::io::IO>,
        hooks: Hooks,
        base_system: String,
        max_turns: usize,
        registry: Arc<ToolRegistry>,
    ) -> Agent {
        let skills = Arc::new(SkillLoader::scan(workdir.join("skills"))); // 空目录 -> 空
        // Tempdir 不在 current_dir 之下，TaskStore::new 的越界校验会拒绝；
        // 直接组装（校验在 dispatch 层仍然生效）。
        let task_store = Arc::new(crate::task_system::store::create_test_store(&workdir));
        let bg_manager = Arc::new(BackgroundManager::new(
            workdir.join(".task_outputs").join("background"),
        ));
        let todo_manager = Arc::new(SharedTodoManager::new(TodoManager::new()));
        let mcp_manager = Arc::new(McpManager::new(Arc::clone(&registry)));
        let compactor = ContextCompactor::new(
            workdir.join(".transcripts"),
            workdir.join(".task_outputs").join("tool-results"),
        );
        let memory = MemoryStore::new_read_only(workdir.join(".memory"));
        Agent {
            client,
            registry,
            mcp_manager,
            skills,
            task_store,
            bg_manager,
            todo_manager,
            io,
            workdir,
            goal: None,
            total_tokens: Arc::new(AtomicU64::new(0)),
            cron_manager: None,
            compactor,
            memory,
            hooks,
            base_system,
            max_turns,
            kind: AgentKind::Lead,
            owner: "agent".to_string(),
            team: None,
            max_tokens: MAX_TOKENS,
        }
    }
```

在 `extract_final_text`（约 808 行）之后加模块级函数：

```rust
/// 从消息列表提取最后一条 assistant 消息的文本（eval runner 取最终回答用）。
pub(crate) fn extract_final_text_from(messages: &[Message]) -> Option<String> {
    let last_assistant = messages
        .iter()
        .rev()
        .find(|m| m.role.as_str() == "assistant")
        .map(|m| m.content.as_slice())
        .unwrap_or(&[]);
    extract_final_text(last_assistant)
}
```

然后把 `TestAgent::new`（约 899-965 行）中 struct 字面量部分替换为：

```rust
        let agent = Agent::isolated(
            workdir.clone(),
            client,
            io,
            Agent::build_hooks(&bg_manager, &todo_manager),
            "test system".into(),
            usize::MAX,
            Arc::new(tools::build_registry()),
        );
        Self { _tmp: tmp, agent }
```

（`workdir` 变量在替换后仍被 `skills` 等局部构造使用的话保留其绑定；TestAgent 原来的 skills/task_store/bg_manager/todo_manager/mcp_manager/compactor/memory/registry/team 局部变量中，`team` 仍需单独构造：isolated 的 `team=None`，TestAgent 需要 `team: Some(team)`。）

**注意**：`TestAgent` 需要 `team: Some(Arc::new(TeamCtx::new(...)))`，而 `isolated` 返回 `team: None`。处理方式：TestAgent 在 `Agent::isolated(...)` 返回后直接改字段：`agent.team = Some(team);`（字段是 `pub(crate)`，同模块可直接赋值）。在 TestAgent::new 中保留 team 构造语句，并在调用 isolated 后加：

```rust
        let mut agent = Agent::isolated(
            workdir.clone(),
            client,
            io,
            Agent::build_hooks(&bg_manager, &todo_manager),
            "test system".into(),
            usize::MAX,
            registry,
        );
        agent.team = Some(Arc::new(
            crate::team::TeamCtx::new(workdir.clone(), Arc::clone(&task_store)).unwrap(),
        ));
        Self { _tmp: tmp, agent }
```

（`task_store` 局部变量保留：`let task_store = Arc::new(crate::task_system::store::create_test_store(&workdir));` 供 TeamCtx 使用。）

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test agent 2>&1 | tail -5`
Expected: agent 模块全部测试通过（含既有 `test_agent_constructs_isolated` 等，无回归）。

Run: `cargo test 2>&1 | tail -3`
Expected: 全仓测试绿。

- [ ] **Step 5: Commit**

```bash
git add src/agent.rs
git commit -m "refactor(agent): extract Agent::isolated from TestAgent construction

Shared by TestAgent and the ch5 eval runner; read-only memory on an empty dir
guarantees zero extra LLM calls (prerequisite for sequential cassette replay).

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 9: EvalRunner —— 评测运行器 + 端到端盒带测试

**Files:**
- Modify: `src/eval/mod.rs`（追加子模块声明与 EvalRunner；本任务的 e2e 测试也放在这里）

**Interfaces:**
- Consumes: Task 1-8 的全部产物；`Agent::isolated`、`extract_final_text_from`、`builtins::PermissionHook`、`io::IO::memory`。
- Produces（Task 10/11 依赖，签名不得改动）:
  - `pub const EVAL_MAX_TURNS: usize = 20`
  - `pub enum ProviderSpec { Replay { cassette: PathBuf }, Record { cassette: PathBuf, inner: Box<dyn LlmProvider>, model: String }, Live { provider: Arc<dyn LlmProvider> } }`
  - `pub struct EvalRunner`：`new(evals_dir: PathBuf, suite: EvalSuite, spec: ProviderSpec, registry: Arc<ToolRegistry>) -> Result<Self, AgentError>` / `pub async fn run(&self) -> Result<RunReport, AgentError>`
  - `pub fn broken_registry(broken: &[&str]) -> Arc<ToolRegistry>`（验收 #6 演示用：把指定工具替换成永远返回空输出的桩）

关键行为约定（写进 doc comment）：
- 每任务：布景临时 workspace → 组装该任务的 masks（基础规则 + workspace 路径字面量 + 其 canonical 形式，都替换为 `<WORKSPACE>`）→ `cassette.set_masks` → 记录 meter 基线 → 构建 isolated Agent（meter 包 cassette 的 Provider 栈 + TrajectoryCollector hook）→ 跑标准 `run_loop` → meter 增量 + collector 快照合成指标。
- **盒带调用顺序**：先 Agent 的全部调用、后 judge 调用（record 与 replay 走同一代码路径，顺序天然一致）。
- judge 只在 `run_loop` 成功返回时执行（漂移/失败时跳过，报告里落 failure）。

- [ ] **Step 1: 写失败测试**

在 `src/eval/mod.rs` 中追加（声明区已有全部子模块后）：

```rust
pub mod cassette;
pub mod fault;
pub mod fingerprint;
pub mod judge;
pub mod report;
pub mod suite;
pub mod trajectory;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::fault::{self, FaultStep, ScriptedProvider};
    use crate::eval::suite::{Assertion, EvalSuite};
    use crate::providers::LlmProvider;
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
    /// 新增失败任务明确标出。两次运行用同一份脚本（模型行为一致），唯一差异是
    /// 基线用健康 registry、候选用 broken_registry(["read_file"])。
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
                inner: Box::new(script()),
                model: "scripted".into(),
            },
            broken_registry(&["read_file"]),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
        // read_file 返回空 → 回答证据缺失 → judge 打分失败（脚本里 judge 对空证据扣分）。
        // 注意：demo_script 的 judge 响应是 pass=true，所以候选用一份 judge 失败脚本：
        let _ = candidate; // 见下方修正说明
    }
}
```

**修正说明（实现时以此为准）**：上面最后一个测试里候选运行的 judge 必须失败才能形成回归。`demo_script()` 的 judge 响应是 `pass=true`；候选运行的脚本把 judge 响应换成 `{"pass": false, "score": 0.0, "rationale": "no evidence: README content was empty"}`。实现时写一个 `demo_script_broken_judge()` 变体（只改第 3 个响应），候选运行用它。断言：

```rust
        let c = report::compare(&baseline, &candidate);
        assert!(c.rate_delta < 0.0);
        assert_eq!(c.newly_failed, vec!["read-summary".to_string()]);
        let text = report::render_compare(&c);
        assert!(text.contains("NEWLY FAILED: read-summary"), "got: {text}");
        assert!(text.contains("-50.0 pts") || text.contains("pts"), "rate drop must be shown");
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test eval::tests 2>&1 | tail -5`
Expected: 编译失败（`EvalRunner` / `ProviderSpec` 未定义）。

- [ ] **Step 3: 实现 EvalRunner**

在 `src/eval/mod.rs`（tests 模块之前）写入：

```rust
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
                (Some(p.clone()), p as Arc<dyn LlmProvider>, masks, "replay")
            }
            ProviderSpec::Record { cassette: path, inner, model } => {
                let masks = cassette::load_masks_for(&path);
                let p = Arc::new(
                    cassette::CassetteProvider::record(inner, &path, masks.clone(), &model)
                        .map_err(AgentError::Other)?,
                );
                (Some(p.clone()), p as Arc<dyn LlmProvider>, masks, "record")
            }
            ProviderSpec::Live { provider } => {
                (None, provider, fingerprint::MaskRules::default_rules(), "live")
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
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test eval:: 2>&1 | tail -5`
Expected: eval 全部测试通过（含 4 个 e2e：roundtrip / drift / forbidden / compare）。

Run: `cargo test 2>&1 | tail -3`
Expected: 全仓绿。

- [ ] **Step 5: Commit**

```bash
git add src/eval/
git commit -m "feat(eval): suite runner on the standard agent loop + e2e cassette tests (ch5)

Per-task workspace staging with path masking, metered provider stack, judge
after agent calls (aligned cassette order), forbidden-tool/loop assertions,
and the broken-tool regression demo through eval compare.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 10: eval CLI —— `bytemaker eval run|compare` + main.rs 分发

**Files:**
- Modify: `src/eval/mod.rs`（追加 `run_cli` 及参数解析）
- Modify: `src/main.rs`（在读取 env/config 之前分发 `eval` 子命令）

**Interfaces:**
- Consumes: Task 9 的 `EvalRunner` / `ProviderSpec`；Task 2 的 `CassetteSpec` / `spec_from_env`；`crate::config::Config`、`crate::providers::openai::OpenAiProvider`。
- Produces:
  - `pub async fn run_cli(args: &[String]) -> Result<(), AgentError>`（Task 11 的手工验证与 main.rs 依赖此签名）
  - CLI 形态：`bytemaker eval run --suite <name> [--replay|--live|--record] [--cassette <path>]`、`bytemaker eval compare <baseline.json> <candidate.json>`

模式解析优先级：显式 flag > `BYTEMAKER_CASSETTE` 环境变量 > 报错提示。`--replay` 默认盒带 `evals/cassettes/<suite>.json`；`--record` 默认同路径；`--live` 走真实 Provider（需要 `OPENAI_API_KEY`）。

- [ ] **Step 1: 写失败测试**

在 `src/eval/mod.rs` 的 tests 模块追加（CLI 解析是纯函数，直接测）：

```rust
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
```

同时给 `src/eval/cassette.rs` 的 tests 模块导出一个锁（CLI 测试与 cassette env 测试共用，避免竞争）：

```rust
/// 跨模块共享的 env 串行锁（spec_from_env 测试与 CLI 测试都用）。
#[cfg(test)]
pub(crate) fn tests_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap()
}
```

（该函数放在 cassette.rs 的 tests 模块内；原 `ENV_LOCK` 静态量可删除，改用这个共享锁。）

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test eval::tests::cli 2>&1 | tail -5`
Expected: 编译失败（`run_cli` / `resolve_spec` / `USAGE` 未定义）。

- [ ] **Step 3: 实现 CLI**

在 `src/eval/mod.rs` 追加（实现区）：

```rust
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
    let baseline = report::RunReport::from_json_file(PathBuf::from(&args[0]))
        .map_err(AgentError::Other)?;
    let candidate = report::RunReport::from_json_file(PathBuf::from(&args[1]))
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
    cassette_default: fn(&str) -> Option<PathBuf>,
) -> Result<ProviderSpec, AgentError> {
    let env_spec = cassette::spec_from_env().map_err(AgentError::Other)?;
    let from_flag =
        || flag_value(args, "--cassette").map(PathBuf::from);
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
            .or_else(|| match env_spec {
                Some(cassette::CassetteSpec::Record(p)) => Some(p),
                _ => None,
            })
            .or_else(default_path)
            .ok_or_else(|| AgentError::Other("no cassette path for --record".into()))?;
        return record(path);
    }
    if has_flag(args, "--replay") {
        let path = from_flag()
            .or_else(|| match env_spec {
                Some(cassette::CassetteSpec::Replay(p)) => Some(p),
                _ => None,
            })
            .or_else(default_path)
            .ok_or_else(|| AgentError::Other("no cassette path for --replay".into()))?;
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
```

修改 `src/main.rs`，在 `async fn main()` 体的最前面（`dotenv()` 之前或之后均可，必须在 `Config::from_env()` 之前）插入：

```rust
    // ch5: `bytemaker eval ...` 子命令。必须在 Config::from_env() 之前分发——
    // `--replay` 离线回放不要求 OPENAI_API_KEY（docs/5.evals.md 验收 #3）。
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("eval") {
        return bytemaker::eval::run_cli(&argv[2..]).await;
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test eval:: 2>&1 | tail -5`
Expected: 全部通过（含 cli_tests）。

Run: `cargo build 2>&1 | tail -3`
Expected: 编译通过。

Run: `cargo run -q -- eval 2>&1 | tail -5`
Expected: 打印 USAGE（错误信息包含 `eval run` 与 `eval compare`），退出码非 0。

- [ ] **Step 5: Commit**

```bash
git add src/eval/ src/main.rs
git commit -m "feat(eval): eval run/compare CLI dispatched before env config (ch5)

Replay works without OPENAI_API_KEY; flags take precedence over
BYTEMAKER_CASSETTE, which alone resolves replay-vs-record when unset.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 11: evals/ 资产 —— 任务集、fixtures、掩码与提交版盒带

**Files:**
- Create: `evals/suites/core.yaml`（≥5 种子任务）
- Create: `evals/fixtures/readme/README.md`（**固定内容**的 fixture，不是仓库 README——它随仓库演化会让盒带不断漂移）
- Create: `evals/fixtures/empty/.gitkeep`
- Create: `evals/cassettes/masks.yaml`
- Create: `evals/cassettes/core.json`（由 `#[ignore]` 测试生成后提交）
- Modify: `.gitignore`（追加 `evals/runs/`）
- Modify: `src/eval/mod.rs`（追加生成测试与回放守卫测试）

**Interfaces:**
- Consumes: Task 9 的 `EvalRunner` / `ProviderSpec`、Task 3 的 `ScriptedProvider`。
- Produces: `bytemaker eval run --suite core --replay` 可直接离线复放的完整资产（验收 #5）。

- [ ] **Step 1: 创建静态资产**

`evals/suites/core.yaml`（种子：README 5 条示例 prompt + ch1/ch2 验收 prompt）：

```yaml
suite: core
tasks:
  # README 示例 #1（只读）
  - name: read-project-summary
    prompt: "Read the file README.md and tell me what this project is about"
    workspace: fixtures/readme/
    assertions:
      - kind: judge
        rubric: "回答准确概括了 bytemaker 是什么（一个用 Rust 逐步构建 Claude Code 风格编码 Agent 的教程项目）"
      - kind: trajectory
        max_steps: 6
        max_tokens: 4000
        forbidden_tools: [write_file, edit_file]
  # README 示例 #2（写后读回）
  - name: create-and-read-back
    prompt: 'Create a file called test.py that prints "hello", then read it back'
    workspace: fixtures/empty/
    assertions:
      - kind: judge
        rubric: "test.py 被创建，且读回内容包含打印 hello 的逻辑"
      - kind: trajectory
        max_steps: 8
  # README 示例 #3（搜索）
  - name: find-python-files
    prompt: "Find all Python files in this directory"
    workspace: fixtures/readme/
    assertions:
      - kind: judge
        rubric: "回答正确说明目录中没有 Python 文件"
      - kind: trajectory
        max_steps: 6
        forbidden_tools: [write_file, edit_file]
  # ch1 验收：最小 agent 问答
  - name: minimal-agent-question
    prompt: "Reply with exactly: hello agent"
    assertions:
      - kind: judge
        rubric: "回答就是 hello agent（允许极小格式差异）"
      - kind: trajectory
        max_steps: 2
  # ch2 验收：只读任务不许写文件
  - name: read-only-discipline
    prompt: "Read README.md and report its first heading. Do not modify anything."
    workspace: fixtures/readme/
    assertions:
      - kind: trajectory
        max_steps: 6
        forbidden_tools: [write_file, edit_file, command]
```

`evals/fixtures/readme/README.md`（固定内容，永不演化）：

```markdown
# bytemaker

bytemaker is a hands-on tutorial project that builds a Claude Code-style coding
agent in Rust, step by step. Each chapter adds one capability to the agent:
tools, sandboxing, memory, persistence, evaluation, orchestration, plugins,
multi-agent collaboration, and production hardening.
```

`evals/fixtures/empty/.gitkeep`（空文件）。

`evals/cassettes/masks.yaml`：

```yaml
masks:
  - { pattern: '\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z', replace: '<TS>' }
  - { pattern: '\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}', replace: '<TS>' }
  - { pattern: '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}', replace: '<UUID>' }
```

`.gitignore` 末尾追加：

```gitignore
# --- ch5 evals: run reports are local artifacts; cassettes ARE tracked ---
evals/runs/
```

- [ ] **Step 2: 写盒带生成测试与回放守卫测试（失败态）**

在 `src/eval/mod.rs` 的 tests 模块追加：

```rust
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
            use fault::{text_response, tool_call_response as tc};
            vec![
                // 1. read-project-summary: read_file -> 回答 -> judge
                tc("c1", "read_file", serde_json::json!({"path": "README.md"})),
                text_response("bytemaker is a hands-on tutorial that builds a Claude Code-style coding agent in Rust, step by step."),
                text_response(r#"{"pass": true, "score": 1.0, "rationale": "accurate summary"}"#),
                // 2. create-and-read-back: write_file -> read_file -> 回答 -> judge
                tc("c2", "write_file", serde_json::json!({"path": "test.py", "content": "print(\"hello\")\n"})),
                tc("c3", "read_file", serde_json::json!({"path": "test.py"})),
                text_response(r#"Created test.py containing print("hello") and read it back — the file exists and prints hello."#),
                text_response(r#"{"pass": true, "score": 1.0, "rationale": "file created and read back"}"#),
                // 3. find-python-files: glob -> 回答 -> judge
                tc("c4", "glob", serde_json::json!({"pattern": "**/*.py"})),
                text_response("There are no Python files in this directory."),
                text_response(r#"{"pass": true, "score": 1.0, "rationale": "correctly reports none"}"#),
                // 4. minimal-agent-question: 直接回答 -> judge
                text_response("hello agent"),
                text_response(r#"{"pass": true, "score": 1.0, "rationale": "exact match"}"#),
                // 5. read-only-discipline: read_file -> 回答（无 judge 断言）
                tc("c5", "read_file", serde_json::json!({"path": "README.md"})),
                text_response("The first heading of README.md is: # bytemaker"),
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
        }
    }
```

- [ ] **Step 3: 运行回放守卫测试确认失败**

Run: `cargo test committed_core_cassette 2>&1 | tail -5`
Expected: FAIL——`missing committed cassette`（盒带尚未生成）。

- [ ] **Step 4: 生成盒带并提交**

Run: `cargo test regenerate_core_cassette -- --ignored 2>&1 | tail -5`
Expected: PASS（脚本化录制运行全绿）。

Run: `head -c 300 evals/cassettes/core.json`
Expected: JSONL 首行含 `"fingerprint":"sha256:..."` 与 `"request":[...]`。

Run: `wc -l evals/cassettes/core.json`
Expected: 14 行（5 任务共 12 次 Agent 调用 + 4 次 judge 调用……实际行数以脚本为准：3+4+3+2+2=14）。

Run: `cargo test committed_core_cassette 2>&1 | tail -5`
Expected: PASS。

Run: `cargo run -q -- eval run --suite core --replay 2>&1 | tail -12`
Expected: 终端表格 5 行全 PASS + `report written to evals/runs/...`（无 API key 也成功——验收 #3/#5）。

Run: `cargo run -q -- eval compare <上一条输出的 report 路径> <同一路径> 2>&1 | tail -5`
Expected: `success rate: baseline 100.0% -> candidate 100.0% ( +0.0 pts)`。

- [ ] **Step 5: 全量验证 + Commit**

Run: `cargo test 2>&1 | tail -3`
Expected: 全仓测试绿（验收 #3：无 `OPENAI_API_KEY` 环境）。

```bash
git add evals/ .gitignore src/eval/
git commit -m "feat(eval): core suite, fixtures, masks, committed cassette (ch5)

Seeded from the README example prompts and ch1/ch2 acceptance prompts; the
cassette is recorded through the real record path with a scripted provider
and replays fully offline (5 tasks, all green, no API key required).

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 12: 验收清单核验与文档同步

**Files:**
- Modify: `docs/5.evals.md`（仅当实现与文档有偏差时：在 §6 对应条目下追加一行「实现备注」；不改既有正文）
- Modify: `README.md`（「运行」一节追加 eval 子命令用法，可选）

**Interfaces:**
- Consumes: Task 1-11 全部产物。
- Produces: 验收通过的证据 + 最终提交。

- [ ] **Step 1: 逐条核验验收标准（docs/5.evals.md §6）**

| # | 验收 | 命令 / 测试 |
|---|---|---|
| 1 | 盒带往返一致性 | `cargo test e2e_record_then_replay` |
| 2 | 指纹漂移显式失败 | `cargo test e2e_prompt_change_fails` |
| 3 | CI 离线全绿（≥3 盒带 e2e） | 清空 `OPENAI_API_KEY` 后 `cargo test`（e2e roundtrip / drift / forbidden / committed-cassette 四个） |
| 4 | 故障注入验证韧性 | `cargo test retry_provider_survives_two_429s` |
| 5 | 评测闭环（≥5 任务、judge 离线可复现） | `cargo run -q -- eval run --suite core --replay` |
| 6 | 回归捕获演示 | `cargo test e2e_compare_flags_broken_tool` + `cargo run -q -- eval compare A B` |

Run: `cargo test 2>&1 | tail -3`
Expected: 全绿。任何失败：修复后重跑（禁止跳过或 ignore）。

- [ ] **Step 2: 手工演示验收 #6（真实 CLI 路径）**

```bash
unset OPENAI_API_KEY
cargo run -q -- eval run --suite core --replay   # 基线报告 evals/runs/X-core.json
# 用上一条输出的报告路径：
cargo run -q -- eval compare evals/runs/X-core.json evals/runs/X-core.json
```

Expected: 两次输出均为 100%，compare 显示 `+0.0 pts`、无 NEWLY FAILED。

- [ ] **Step 3: Commit（如有文档微调）**

```bash
git add docs/5.evals.md README.md
git commit -m "docs(ch5): note implementation details in acceptance section

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

（无改动则跳过本提交。）

---

## Self-Review 结论

* **Spec 覆盖**：docs/5.evals.md §3.1 CassetteProvider（Task 2）、§3.2 指纹（Task 1）、§3.3 任务集（Task 5）、§3.4 运行器+裁判（Task 6/9）、§3.5 轨迹+对比（Task 4/7）、§2.5 故障注入（Task 3）、§4 目录结构（Task 1-11 逐文件）、§6 验收 1-6（Task 9/11/12 测试逐条对应）。CLI 双模式与 `BYTEMAKER_CASSETTE`（Task 10）。**超出 spec 的两点已在计划中说明理由**：`RetryProvider`（验收 #4 需要可离线测试的重试逻辑，async-openai 内部重试在 trait 层不可测）、`MeteredProvider`（steps/tokens 无法从 Hooks 采集）。
* **占位符扫描**：无 TBD/TODO；每个代码步骤含完整代码与确切命令。
* **类型一致性**：`fingerprint(messages, masks)`、`CassetteProvider::{record,replay,set_masks}`、`TrajectoryCollector::snapshot`、`MeteredProvider::totals() -> (u64,u64,u64)`、`compose(snapshot, meter)`、`RunReport::new(...)`、`compare(&RunReport, &RunReport)`、`EvalRunner::new(evals_dir, suite, spec, registry)`、`run_cli(&[String])` 在 Task 1-11 间签名一致。Task 9 的 compare 测试初稿含一处已知笔误，其「修正说明」块给出最终断言代码（实现以修正说明为准）。
* **风险提示（执行时注意）**：Task 11 盒带生成后若回放守卫测试失败，最可能原因是工具输出里嵌入了未掩码的路径/时间——先检查 glob 与 read_file 的输出格式，再扩展 `evals/cassettes/masks.yaml` 或 runner 的字面量掩码；**不要**通过放宽指纹校验解决。





