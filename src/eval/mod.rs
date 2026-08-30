//! eval —— 评测与确定性测试基建（docs/5.evals.md）。
//!
//! 本章门面：各子模块（cassette/fingerprint/fault/suite/judge/trajectory/report）
//! 逐步声明；`EvalRunner` 与 `eval` CLI 子命令见后续任务。

pub mod cassette;
pub mod fault;
pub mod fingerprint;
pub mod trajectory;
