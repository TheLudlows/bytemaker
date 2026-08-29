/*
task.rs - background task data structures (s11)

Pure data, no business deps. BackgroundTask + TaskStatus.
Background commands run on a worker thread; state machine: running -> completed | failed | cancelled.
*/

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Background task status.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Worker still running.
    Running,
    /// exit_code == 0.
    Completed,
    /// exit_code != 0, timeout, or error.
    Failed,
    /// Cancelled by TaskStop.
    Cancelled,
}

/// Background task record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackgroundTask {
    /// Task id, format bg_[0-9a-f]{8}.
    pub id: String,
    /// bash command string.
    pub command: String,
    /// Current status.
    pub status: TaskStatus,
    /// Original tool_call id (linkage only; notifications don't reuse it).
    pub call_id: String,
    /// Start timestamp (Unix seconds).
    pub started_at: u64,
    /// On-disk output file (TaskOutput reads here, stays out of memory).
    pub output_file: PathBuf,
    /// Set on completion; None while Running.
    pub exit_code: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_serializes_snake_case() {
        for (status, expected) in [
            (TaskStatus::Running, "\"running\""),
            (TaskStatus::Completed, "\"completed\""),
            (TaskStatus::Failed, "\"failed\""),
            (TaskStatus::Cancelled, "\"cancelled\""),
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, expected);
            let back: TaskStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }

    #[test]
    fn background_task_serializes_roundtrip() {
        let task = BackgroundTask {
            id: "bg_a1b2c3d4".to_string(),
            command: "npm install".to_string(),
            status: TaskStatus::Completed,
            call_id: "toolu_01".to_string(),
            started_at: 1700000000,
            output_file: PathBuf::from(".task_outputs/background/bg_a1b2c3d4.log"),
            exit_code: Some(0),
        };
        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains("\"status\":\"completed\""));
        assert!(json.contains("\"exit_code\":0"));
        let back: BackgroundTask = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "bg_a1b2c3d4");
        assert_eq!(back.status, TaskStatus::Completed);
        assert_eq!(back.exit_code, Some(0));

        // exit_code: None branch (Running state, serializes to null).
        let running = BackgroundTask {
            id: "bg_00000000".to_string(),
            command: "sleep 1".to_string(),
            status: TaskStatus::Running,
            call_id: "toolu_02".to_string(),
            started_at: 1700000001,
            output_file: PathBuf::from(".task_outputs/background/bg_00000000.log"),
            exit_code: None,
        };
        let json_none = serde_json::to_string(&running).unwrap();
        assert!(json_none.contains("\"exit_code\":null"));
        assert!(json_none.contains("\"status\":\"running\""));
        let back_none: BackgroundTask = serde_json::from_str(&json_none).unwrap();
        assert_eq!(back_none.exit_code, None);
        assert_eq!(back_none.status, TaskStatus::Running);
    }
}
