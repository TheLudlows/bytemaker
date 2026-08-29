//! s16 Workflow Runtime — run a saved orchestration through one tool call.
//!
//! Pipeline: registry resolve → schema validate → runner → runtime (acquire RunLock,
//! load/create journal, snapshot, build `ExecutionState`, run script, persist output +
//! final snapshot). Resume replays cached `agent()` calls by stable key (journal cache).
//!
//! Known caveats (stated, not fixed here):
//! - `journal::record` writes a cloned file handle **outside** the cache mutex; concurrent
//!   `agent()` calls from `parallel()` can interleave JSONL lines → corrupt → not resumable
//!   (mitigated by `RunLock` cross-process, not intra-process).
//! - `ids::agent_key` is `stable_hash % 10^10` (≈33-bit) — collisions silently return the
//!   wrong cached value; no detection.
//! - `runtime::check_permission` is a stub (always `Ok(())`).
//! - `AGENT_CAP`/`CONCURRENCY` are per-`run()` constants, not shared across Workflow calls;
//!   budget `spent` is not persisted → over-budget calls not resumable.
//!
//! See docs/superpowers/specs/2026-08-24-workflow-runtime-design.md and `docs/modules/workflow.md`.

pub mod budget;
pub mod error;
pub mod ids;
pub mod journal;
pub mod registry;
pub mod runner;
pub mod runtime;
pub mod sample;
pub mod schema;
pub mod state;
pub mod store;
pub mod task;

pub use error::WorkflowError;
pub use runtime::{run_workflow, serialize_task, RunResult, WorkflowRuntime};
