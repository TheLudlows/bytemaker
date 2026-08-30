//! 报告生成与回归对比（docs/5.evals.md §3.4 / §3.5）。
//!
//! 双输出：JSON 落盘 `evals/runs/<timestamp>-<suite>.json`（供回归对比），终端表格
//! 给人体读。对比输出三次差：成功率差、Token 涨幅告警、新增死循环模式。

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
            if let Some(j) = &t.judge {
                out.push_str(&format!(
                    "    -> judge({:.2}): {}\n",
                    j.score, j.rationale
                ));
            }
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
    fn find<'a>(r: &'a RunReport, name: &str) -> Option<&'a TaskReport> {
        r.tasks.iter().find(|t| t.name == name)
    }
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
