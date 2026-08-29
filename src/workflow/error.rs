//! Workflow runtime errors.

use crate::error::AgentError;

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("validation error: {0}")]
    Validation(String),
    #[error("workflow '{0}' not found")]
    NotFound(String),
    #[error("invalid workflow metadata: {0}")]
    InvalidMeta(String),
    #[error("invalid run id: {0}")]
    InvalidRunId(String),
    #[error("workflow run {0} is already active")]
    RunActive(String),
    #[error("agent output failed schema validation: {0}")]
    SchemaInvalid(String),
    #[error("token budget exceeded ({spent} > {total})")]
    BudgetExceeded { spent: u64, total: u64 },
    #[error("agent() cap reached ({0})")]
    AgentCap(usize),
    #[error("journal corrupt: {0}")]
    JournalCorrupt(String),
    #[error("snapshot not found for {0}")]
    SnapshotNotFound(String),
    #[error("resume mismatch: {0}")]
    ResumeMismatch(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("agent error: {0}")]
    Agent(#[from] AgentError),
}

impl WorkflowError {
    /// String form for tool_output content (bytemaker convention: "Error: ...").
    pub fn as_tool_string(&self) -> String {
        format!("Error: {self}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_tool_string_prefixed() {
        let e = WorkflowError::Validation("bad slug".into());
        assert_eq!(e.as_tool_string(), "Error: validation error: bad slug");
    }

    #[test]
    fn budget_exceeded_message() {
        let e = WorkflowError::BudgetExceeded { spent: 10, total: 8 };
        assert!(e.to_string().contains("10 > 8"));
    }
}
