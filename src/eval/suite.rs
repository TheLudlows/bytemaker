//! 任务集定义（YAML）与运行器布景（docs/5.evals.md §3.3）。
//!
//! 断言分两类：`judge`（语义正确性，由裁判打分）与 `trajectory`（行为约束，纯规则、
//! 零成本）。`workspace` 指向 `evals/fixtures/` 下的目录，运行前复制到临时目录——
//! 任务之间互不污染，也不动真实仓库。

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;

/// 任务集（`evals/suites/<name>.yaml`）。
#[derive(Debug, Clone, Deserialize)]
pub struct EvalSuite {
    pub suite: String,
    pub tasks: Vec<EvalTask>,
}

/// 单条评测任务。
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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
