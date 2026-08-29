//! Orchestration primitives injected into workflow scripts (ExecutionState).
//! Mirrors s16's ExecutionState: `agent` (journal cache + schema retry),
//! `phase`, `log`, plus `parallel`/`pipeline` primitives.

use crate::workflow::budget::Budget;
use crate::workflow::journal::WorkflowJournal;
use crate::workflow::runner::{RunnerOutput, WorkflowRunner};
use crate::workflow::schema::SimpleJsonSchema;
use crate::workflow::task::{ExecutionLimits, LocalWorkflowTask};
use crate::workflow::WorkflowError;
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A boxed async workflow step result, borrowing an `ExecutionState` for `'a`.
pub type BoxFut<'a> = Pin<Box<dyn Future<Output = Result<Value, WorkflowError>> + Send + 'a>>;

pub struct ExecutionState<'a> {
    pub task: &'a LocalWorkflowTask,
    pub journal: &'a WorkflowJournal,
    pub runner: Arc<dyn WorkflowRunner>,
    pub budget: std::sync::Mutex<Budget>,
    pub args: &'a Value,
    pub limits: &'a ExecutionLimits,
    pub depth: usize,
    phase: std::sync::Mutex<Option<String>>,
    phases_seen: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl<'a> ExecutionState<'a> {
    pub fn new(
        task: &'a LocalWorkflowTask,
        journal: &'a WorkflowJournal,
        runner: Arc<dyn WorkflowRunner>,
        budget: Budget,
        args: &'a Value,
        limits: &'a ExecutionLimits,
    ) -> Self {
        Self {
            task,
            journal,
            runner,
            budget: std::sync::Mutex::new(budget),
            args,
            limits,
            depth: 0,
            phase: std::sync::Mutex::new(None),
            phases_seen: std::sync::Mutex::new(Default::default()),
        }
    }

    /// Start a phase; subsequent agent()s group under it. Upsert: emitting the
    /// same phase again does not re-announce it.
    pub fn phase(&self, title: &str) {
        *self.phase.lock().unwrap() = Some(title.to_string());
        let mut seen = self.phases_seen.lock().unwrap();
        if seen.insert(title.to_string()) {
            self.task
                .progress_event("workflow_phase", json!({"title": title}));
        }
    }

    /// Emit a workflow_log progress line.
    pub fn log(&self, message: &str) {
        self.task
            .progress_event("workflow_log", json!({"message": message}));
    }

    /// Spawn one subagent. With a schema, force structured output + validate
    /// (retry once). On resume, a cached key short-circuits the run.
    pub async fn agent(
        &self,
        prompt: &str,
        schema: Option<&Value>,
        label: Option<&str>,
        phase: Option<&str>,
    ) -> Result<Value, WorkflowError> {
        let default: String = prompt.chars().take(24).collect();
        let label = label.unwrap_or(default.as_str()).to_string();

        self.limits.claim_agent()?;
        {
            let b = self.budget.lock().unwrap();
            if b.remaining() == 0 {
                return Err(WorkflowError::BudgetExceeded {
                    spent: b.spent(),
                    total: b.limit().unwrap_or(0),
                });
            }
        }

        let key = WorkflowJournal::key(&label, prompt, schema);
        if let Some(cached) = self.journal.cached(&key) {
            if let Some(s) = schema {
                SimpleJsonSchema::new(s.clone())
                    .validate(&cached)
                    .map_err(|e| WorkflowError::SchemaInvalid(format!("cached: {e}")))?;
            }
            self.task.progress_event(
                "workflow_agent",
                json!({"label": label.as_str(), "phase": phase, "status": "cached"}),
            );
            return Ok(cached);
        }

        let _permit = self
            .limits
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| WorkflowError::Validation(e.to_string()))?;
        let out: RunnerOutput = self.runner.run(prompt, schema, Some(&label)).await?;
        let mut result = out.value;
        let mut tokens = out.tokens;

        if let Some(s) = schema {
            if SimpleJsonSchema::new(s.clone()).validate(&result).is_err() {
                let retry = self
                    .runner
                    .run(&format!("{prompt}\n\nReturn valid JSON."), schema, Some(&label))
                    .await?;
                result = retry.value;
                tokens += retry.tokens;
            }
            SimpleJsonSchema::new(s.clone())
                .validate(&result)
                .map_err(WorkflowError::SchemaInvalid)?;
        }

        self.budget.lock().unwrap().add(tokens)?;
        self.task.add_usage(1, tokens);
        self.journal.record(&key, &result)?;
        self.task.progress_event(
            "workflow_agent",
            json!({"label": label.as_str(), "phase": phase, "status": "done"}),
        );
        Ok(result)
    }

    /// BARRIER: run all thunks concurrently; fail on first error.
    pub async fn parallel(&'a self, thunks: Vec<BoxFut<'a>>) -> Result<Vec<Value>, WorkflowError> {
        futures_util::future::try_join_all(thunks).await
    }

    /// Per-item staged flow, NO barrier between stages: item A can be in stage 3
    /// while item B is still in stage 1. Each stage gets (prev, original_item, idx).
    pub async fn pipeline(
        &'a self,
        items: Vec<Value>,
        stages: Vec<Arc<dyn PipelineStage>>,
    ) -> Result<Vec<Value>, WorkflowError> {
        let futs: Vec<BoxFut<'a>> = items
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                let item = item.clone();
                let stages = stages.clone();
                Box::pin(async move {
                    let mut value = item.clone();
                    for stage in &stages {
                        value = stage.run(self, value, item.clone(), idx).await?;
                    }
                    Ok::<_, WorkflowError>(value)
                }) as BoxFut<'a>
            })
            .collect();
        futures_util::future::try_join_all(futs).await
    }
}

/// One stage of a pipeline. `prev` is the prior stage's output (or the original
/// item for stage 0), `item` is the original item, `idx` its index.
#[async_trait::async_trait]
pub trait PipelineStage: Send + Sync {
    async fn run(
        &self,
        ctx: &ExecutionState<'_>,
        prev: Value,
        item: Value,
        idx: usize,
    ) -> Result<Value, WorkflowError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::registry::Meta;
    use crate::workflow::runner::MockRunner;
    use crate::workflow::store::Store;
    use serde_json::json;
    use tempfile::TempDir;

    fn meta() -> Meta {
        Meta {
            name: "review-changes".into(),
            description: "d".into(),
            phases: Some(vec![]),
        }
    }

    fn ctx<'a>(
        task: &'a LocalWorkflowTask,
        journal: &'a WorkflowJournal,
        limits: &'a ExecutionLimits,
        args: &'a Value,
    ) -> ExecutionState<'a> {
        ExecutionState::new(task, journal, Arc::new(MockRunner), Budget::new(None), args, limits)
    }

    #[tokio::test]
    async fn agent_caches_on_second_call() {
        let td = TempDir::new().unwrap();
        let store = Store::new(td.path()).unwrap();
        let run_id = store.reserve_run_id("review-changes").unwrap();
        let journal = WorkflowJournal::new(&store, &run_id, false).unwrap();
        let task = LocalWorkflowTask::new("t".into(), run_id, meta());
        let limits = ExecutionLimits::new();
        let args = json!({});
        let c = ctx(&task, &journal, &limits, &args);
        let schema = json!({"type":"object","required":["isReal"],"properties":{"isReal":{"type":"boolean"}}});
        let r1 = c.agent("prompt", Some(&schema), Some("v"), None).await.unwrap();
        let r2 = c.agent("prompt", Some(&schema), Some("v"), None).await.unwrap();
        assert_eq!(r1, r2);
        assert_eq!(task.usage_snapshot().agents, 1); // second hit cached
    }

    #[tokio::test]
    async fn agent_no_schema_returns_text() {
        let td = TempDir::new().unwrap();
        let store = Store::new(td.path()).unwrap();
        let run_id = store.reserve_run_id("review-changes").unwrap();
        let journal = WorkflowJournal::new(&store, &run_id, false).unwrap();
        let task = LocalWorkflowTask::new("t".into(), run_id, meta());
        let limits = ExecutionLimits::new();
        let args = json!({});
        let c = ctx(&task, &journal, &limits, &args);
        let r = c.agent("hello world", None, Some("l"), None).await.unwrap();
        assert!(r.is_string());
    }

    #[tokio::test]
    async fn parallel_runs_all_and_orders_results() {
        let td = TempDir::new().unwrap();
        let store = Store::new(td.path()).unwrap();
        let run_id = store.reserve_run_id("review-changes").unwrap();
        let journal = WorkflowJournal::new(&store, &run_id, false).unwrap();
        let task = LocalWorkflowTask::new("t".into(), run_id, meta());
        let limits = ExecutionLimits::new();
        let args = json!({});
        let c = ctx(&task, &journal, &limits, &args);
        let thunks: Vec<BoxFut<'_>> = vec![
            Box::pin(c.agent("aaa", None, Some("a"), None)),
            Box::pin(c.agent("bbb", None, Some("b"), None)),
            Box::pin(c.agent("ccc", None, Some("c"), None)),
        ];
        let out = c.parallel(thunks).await.unwrap();
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(Value::is_string));
    }

    #[tokio::test]
    async fn agent_truncates_non_ascii_label_without_panic() {
        let td = TempDir::new().unwrap();
        let store = Store::new(td.path()).unwrap();
        let run_id = store.reserve_run_id("review-changes").unwrap();
        let journal = WorkflowJournal::new(&store, &run_id, false).unwrap();
        let task = LocalWorkflowTask::new("t".into(), run_id, meta());
        let limits = ExecutionLimits::new();
        let args = json!({});
        let c = ctx(&task, &journal, &limits, &args);
        // 23 ASCII + a 3-byte char (中) => byte index 24 splits 中.
        // Byte-slicing [..24] would panic; char-based take(24) must not.
        let prompt = format!("{}中", "x".repeat(23));
        let r = c.agent(&prompt, None, None, None).await.unwrap();
        assert!(r.is_string());
    }
}
