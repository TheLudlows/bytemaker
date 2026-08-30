//! 请求指纹：三级归一化（结构化剥离 → 掩码规则 → SHA-256）（docs/5.evals.md §3.2）。
//!
//! 指纹只对**请求消息**计算：system prompt 含当前时间与 workspace 路径，刻意不入指纹。
//! 结构化剥离 = tool_call/call_id 统一替换为 `<ID>`（模型生成的 ID 每次录制都不同）；
//! 掩码规则 = 可配置正则（ISO 时间、UUID、workspace 路径等易变字段）；最后对规范化
//! JSON 全文取 SHA-256——真实语义变化必须重录（刻意保守）。

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
                tracing::warn!(
                    "masks file {} invalid ({e}), falling back to defaults",
                    path.display()
                );
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
