//! Workflow task state, usage, and progress events + run-wide limits.

use crate::workflow::registry::Meta;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Semaphore;

pub const AGENT_CAP: usize = 1000;
pub const CONCURRENCY: usize = 8;

/// Shared run-wide limits, including across nested workflows.
pub struct ExecutionLimits {
    pub agents: std::sync::atomic::AtomicUsize,
    pub semaphore: Arc<Semaphore>,
}

impl ExecutionLimits {
    pub fn new() -> Self {
        Self {
            agents: 0.into(),
            semaphore: Arc::new(Semaphore::new(CONCURRENCY)),
        }
    }

    pub fn claim_agent(&self) -> Result<(), crate::workflow::WorkflowError> {
        let n = self.agents.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n > AGENT_CAP {
            return Err(crate::workflow::WorkflowError::AgentCap(AGENT_CAP));
        }
        Ok(())
    }
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub agents: u64,
    pub tokens: u64,
}

pub struct LocalWorkflowTask {
    pub task_id: String,
    pub run_id: String,
    pub meta: Meta,
    pub status: TaskStatus,
    pub usage: std::sync::Mutex<Usage>,
    pub progress: std::sync::Mutex<Vec<Value>>,
}

impl LocalWorkflowTask {
    pub fn new(task_id: String, run_id: String, meta: Meta) -> Self {
        Self {
            task_id,
            run_id,
            meta,
            status: TaskStatus::Running,
            usage: Default::default(),
            progress: Default::default(),
        }
    }

    pub fn progress_event(&self, ptype: &str, data: Value) {
        let mut entry = serde_json::Map::new();
        entry.insert("type".into(), Value::String(ptype.to_string()));
        if let Value::Object(m) = data {
            entry.extend(m);
        }
        let entry = Value::Object(entry);
        tracing::debug!(target: "workflow", "progress {ptype}: {entry}");
        self.progress.lock().unwrap().push(entry);
    }

    pub fn add_usage(&self, agents: u64, tokens: u64) {
        let mut u = self.usage.lock().unwrap();
        u.agents += agents;
        u.tokens += tokens;
    }

    pub fn usage_snapshot(&self) -> Usage {
        self.usage.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Meta {
        Meta {
            name: "review-changes".into(),
            description: "d".into(),
            phases: Some(vec!["Review".into()]),
        }
    }

    #[test]
    fn claim_until_cap() {
        let lim = ExecutionLimits::new();
        for _ in 0..AGENT_CAP {
            lim.claim_agent().unwrap();
        }
        assert!(lim.claim_agent().is_err());
    }

    #[test]
    fn progress_event_records() {
        let t = LocalWorkflowTask::new("t1".into(), "wf_x".into(), meta());
        t.progress_event("workflow_log", serde_json::json!({"message":"hi"}));
        assert_eq!(t.progress.lock().unwrap().len(), 1);
    }

    #[test]
    fn usage_accumulates() {
        let t = LocalWorkflowTask::new("t".into(), "wf".into(), meta());
        t.add_usage(1, 50);
        t.add_usage(1, 25);
        let u = t.usage_snapshot();
        assert_eq!((u.agents, u.tokens), (2, 75));
    }
}
