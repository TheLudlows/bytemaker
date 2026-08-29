//! task_system — persistent task graph (s10).
//!
//! File-backed tracking (create/list/get/claim/complete), state Pending→InProgress→Completed
//! with `blocked_by` deps and optional worktree binding (s13); persists to `.tasks/<id>.json`.
//! Details: `docs/modules/task_system.md`.

pub mod task;
pub mod store;
pub mod tools;

pub use task::{Task, TaskStatus};
pub use store::{TaskStore, TaskStoreError};
pub use tools::CreateTaskTool;
pub use tools::ListTasksTool;
pub use tools::GetTaskTool;
pub use tools::ClaimTaskTool;
pub use tools::CompleteTaskTool;
pub use tools::claim_task;
pub use tools::complete_task;
