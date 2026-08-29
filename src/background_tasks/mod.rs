//! background_tasks — background slow-command execution (s11).
//!
//! Slow bash commands return a `bg_id` placeholder `tool_output` immediately; on completion
//! a `<task_notification>` is injected into a later turn. Exports `BackgroundManager`,
//! `BackgroundTask`, `TaskStatus`, `TaskOutputTool`, `TaskStopTool`, `BackgroundStopHook`.
//! In-memory only (persistence is `task_system`'s job); state guarded by `std::sync::Mutex`.
//!
//! See `docs/modules/background_tasks.md`.

pub mod manager;
pub mod task;
pub mod tools;

pub use manager::BackgroundManager;
pub use task::{BackgroundTask, TaskStatus};
pub use tools::{BackgroundStopHook, TaskOutputTool, TaskStopTool};
