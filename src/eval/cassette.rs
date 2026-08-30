//! CassetteProvider：录制/回放双模 Provider（docs/5.evals.md §3.1）。
//!
//! 盒带是**有序的** `(请求指纹, 响应)` 列表（JSONL）。回放按调用顺序吐出响应并校验
//! 指纹；指纹不匹配报可行动错误（第 N 次调用 + diff + 建议重录），不允许静默假绿。

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
#[derive(Debug)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::{ContentBlock, Message, MessagesResponse};
    use crate::providers::{CallResult, LlmProvider, MockProvider};
    use std::path::PathBuf;

    /// 两个响应的脚本 inner（ScriptedProvider 在 Task 3 才有，这里用最朴素的办法：
    /// 直接手写 entries + MockProvider，验证 cassette 自身行为）。
    #[allow(dead_code)] // Task 3（ScriptedProvider）将使用
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
