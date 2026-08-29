use serde::{Deserialize, Serialize};

/// Task status
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

/// Task data structure
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    /// Task ID, format: task_xxxxxxxx (8 hex chars)
    pub id: String,
    /// Brief task title
    pub subject: String,
    /// Detailed task description
    pub description: String,
    /// Current status
    pub status: TaskStatus,
    /// Agent owning this task
    pub owner: Option<String>,
    /// Prerequisite task IDs
    pub blocked_by: Vec<String>,
    /// Optional task-bound worktree name (s13). Old JSON without it deserializes to None.
    #[serde(default)]
    pub worktree: Option<String>,
}

impl Task {
    /// Check if the task can be claimed
    pub fn can_claim(&self, incomplete_deps: &[String]) -> bool {
        self.status == TaskStatus::Pending && incomplete_deps.is_empty()
    }

    /// Check if the task can be completed
    pub fn can_complete(&self, owner: &str) -> bool {
        self.status == TaskStatus::InProgress
            && self.owner.as_deref() == Some(owner)
    }
}

impl TaskStatus {
    /// Bare word form (`in_progress`) for user-facing messages.
    pub fn as_word(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}