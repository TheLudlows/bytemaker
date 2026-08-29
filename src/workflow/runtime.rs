//! Workflow runtime: validate meta, lock, open journal, run script, persist,
//! resume. Mirrors s16 WorkflowTool::call.

use crate::workflow::budget::Budget;
use crate::workflow::ids::{is_valid_name, validate_run_id};
use crate::workflow::journal::WorkflowJournal;
use crate::workflow::registry::{workflows, Meta, Workflow};
use crate::workflow::runner::WorkflowRunner;
use crate::workflow::state::ExecutionState;
use crate::workflow::store::{RunLock, Store};
use crate::workflow::task::{ExecutionLimits, LocalWorkflowTask, TaskStatus};
use crate::workflow::WorkflowError;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

pub struct WorkflowRuntime {
    store: Store,
    runner: Arc<dyn WorkflowRunner>,
}

pub struct RunResult {
    pub launched: Value,
    pub result: Value,
    pub task: LocalWorkflowTask,
}

impl WorkflowRuntime {
    pub fn new(workdir: &Path, runner: Arc<dyn WorkflowRunner>) -> Result<Self, WorkflowError> {
        Ok(Self {
            store: Store::new(workdir)?,
            runner,
        })
    }

    pub async fn run(
        &self,
        meta: &Meta,
        script: Arc<dyn Workflow>,
        args: Value,
        resume_from_run_id: Option<&str>,
    ) -> Result<RunResult, WorkflowError> {
        validate_meta(meta)?;
        check_permission(meta)?;

        let resuming = resume_from_run_id.is_some();
        let run_id = match resume_from_run_id {
            Some(id) => {
                if !validate_run_id(id) {
                    return Err(WorkflowError::InvalidRunId(id.into()));
                }
                id.to_string()
            }
            None => self.store.reserve_run_id(&meta.name)?,
        };

        let _run_lock = RunLock::try_acquire(&self.store.lock_path(&run_id))?;

        let (args, journal) = if resuming {
            let snap = self.store.read_snapshot(&run_id)?;
            let saved_name = snap
                .get("workflowName")
                .and_then(Value::as_str)
                .ok_or_else(|| WorkflowError::ResumeMismatch("snapshot missing workflowName".into()))?;
            if saved_name != meta.name {
                return Err(WorkflowError::ResumeMismatch(format!(
                    "{saved_name} != {}",
                    meta.name
                )));
            }
            let saved_args = snap.get("args").cloned().unwrap_or(Value::Null);
            let args = if args.is_null() {
                saved_args
            } else if args == saved_args {
                args
            } else {
                return Err(WorkflowError::ResumeMismatch(
                    "args differ from original run".into(),
                ));
            };
            (args, WorkflowJournal::new(&self.store, &run_id, true)?)
        } else {
            let args = if args.is_null() {
                Value::Object(Default::default())
            } else {
                args
            };
            (args, WorkflowJournal::new(&self.store, &run_id, false)?)
        };

        let task_id = format!("local_workflow_{run_id}");
        let mut task = LocalWorkflowTask::new(task_id, run_id.clone(), meta.clone());

        let launched = serde_json::json!({
            "status": "async_launched",
            "taskId": task.task_id.as_str(),
            "taskType": "local_workflow",
            "runId": run_id.as_str(),
            "workflowName": meta.name.as_str(),
        });
        task.progress_event(
            "task_started",
            serde_json::json!({"workflow": meta.name.as_str(), "resume": resuming}),
        );

        // Snapshot before execution.
        self.store
            .write_json_atomic(&self.store.snapshot_path(&run_id), &serde_json::json!({
                "runId": run_id.as_str(),
                "workflowName": meta.name.as_str(),
                "args": &args,
                "task": serialize_task(&task),
            }))?;

        // Run the workflow script. ctx is scoped so its &task/&journal borrows
        // drop before we mutate task.status below.
        let limits = ExecutionLimits::new();
        let budget = Budget::new(args.get("budget").and_then(Value::as_u64));
        let run_outcome = {
            let ctx =
                ExecutionState::new(&task, &journal, self.runner.clone(), budget, &args, &limits);
            script.run(&ctx, &args).await
        };
        let result = match run_outcome {
            Ok(v) => {
                task.status = TaskStatus::Completed;
                v
            }
            Err(e) => {
                task.status = TaskStatus::Failed;
                serde_json::json!({"error": e.to_string()})
            }
        };

        self.store
            .write_json_atomic(&self.store.output_path(&run_id), &result)?;
        self.store
            .write_json_atomic(&self.store.snapshot_path(&run_id), &serde_json::json!({
                "runId": run_id.as_str(),
                "workflowName": meta.name.as_str(),
                "args": &args,
                "task": serialize_task(&task),
            }))?;

        let usage = task.usage_snapshot();
        let status_str = match task.status {
            TaskStatus::Running => "running",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
        };
        task.progress_event(
            "task_notification",
            serde_json::json!({
                "status": status_str,
                "agents": usage.agents,
                "tokens": usage.tokens,
                "outputFile": format!(".workflows/{run_id}.output.json"),
            }),
        );

        Ok(RunResult { launched, result, task })
    }
}

/// Resolve a workflow by name from the host registry and run it.
pub async fn run_workflow(
    workdir: &Path,
    runner: Arc<dyn WorkflowRunner>,
    name: &str,
    args: Value,
    resume_from_run_id: Option<&str>,
) -> Result<RunResult, WorkflowError> {
    // Ensure builtins are registered (idempotent).
    crate::workflow::sample::register_sample();
    let (meta, wf) = workflows()
        .get(name)
        .ok_or_else(|| WorkflowError::NotFound(name.to_string()))?;
    let rt = WorkflowRuntime::new(workdir, runner)?;
    rt.run(meta, wf.clone(), args, resume_from_run_id).await
}

fn validate_meta(meta: &Meta) -> Result<(), WorkflowError> {
    if meta.name.is_empty() || meta.description.is_empty() {
        return Err(WorkflowError::InvalidMeta("requires name and description".into()));
    }
    if !is_valid_name(&meta.name) {
        return Err(WorkflowError::InvalidMeta(
            "name must be a 1-64 char slug".into(),
        ));
    }
    if let Some(phases) = &meta.phases {
        if !phases.iter().all(|p| !p.is_empty()) {
            return Err(WorkflowError::InvalidMeta(
                "phases must be non-empty strings".into(),
            ));
        }
    }
    Ok(())
}

fn check_permission(_meta: &Meta) -> Result<(), WorkflowError> {
    // s03 allow/deny gate — stubbed Pass for now (plumb settings in WF-14+ if needed).
    Ok(())
}

/// Serialize a task's identity + status + usage for snapshot/tool output.
pub fn serialize_task(task: &LocalWorkflowTask) -> Value {
    let usage = task.usage_snapshot();
    let status = match task.status {
        TaskStatus::Running => "running",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
    };
    serde_json::json!({
        "taskId": task.task_id.as_str(),
        "taskType": "local_workflow",
        "runId": task.run_id.as_str(),
        "workflowName": task.meta.name.as_str(),
        "status": status,
        "usage": {"agents": usage.agents, "tokens": usage.tokens},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::runner::MockRunner;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn run_completes_with_findings() {
        let td = TempDir::new().unwrap();
        let out = run_workflow(
            td.path(),
            Arc::new(MockRunner),
            "review-changes",
            json!({"changes": "def f(x): return x"}),
            None,
        )
        .await
        .unwrap();
        assert_eq!(out.task.status, TaskStatus::Completed);
        assert!(out.result.get("confirmed").unwrap().is_array());
        assert!(out.task.usage_snapshot().agents > 0);
    }

    #[tokio::test]
    async fn resume_hits_all_cache() {
        let td = TempDir::new().unwrap();
        let args = json!({"changes": "def f(x): return x"});
        let first = run_workflow(
            td.path(),
            Arc::new(MockRunner),
            "review-changes",
            args.clone(),
            None,
        )
        .await
        .unwrap();
        let run_id = first.task.run_id.clone();
        // Snapshot + journal now exist; resume should hit every cache.
        let second = run_workflow(
            td.path(),
            Arc::new(MockRunner),
            "review-changes",
            args,
            Some(&run_id),
        )
        .await
        .unwrap();
        let u = second.task.usage_snapshot();
        assert_eq!(u.agents, 0);
        assert_eq!(u.tokens, 0);
    }

    #[tokio::test]
    #[ignore = "needs API key + network"]
    async fn smoke_review_changes_real_api() {
        let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
        let base_url = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let model = std::env::var("OPENAI_MODEL")
            .unwrap_or_else(|_| "gpt-4.1".into());
        let client = std::sync::Arc::new(crate::providers::openai::OpenAiProvider::new(api_key, base_url, model));
        let td = TempDir::new().unwrap();
        let out = run_workflow(
            td.path(),
            std::sync::Arc::new(crate::workflow::runner::OpenAIRunner::new(client)),
            "review-changes",
            json!({"changes": crate::workflow::sample::DEMO_CHANGES}),
            None,
        )
        .await
        .unwrap();
        eprintln!(
            "smoke status={:?} agents={} tokens={}\nresult={}",
            out.task.status,
            out.task.usage_snapshot().agents,
            out.task.usage_snapshot().tokens,
            serde_json::to_string_pretty(&out.result).unwrap()
        );
        assert_eq!(out.task.status, TaskStatus::Completed);
        assert!(out.task.usage_snapshot().tokens > 0);
        println!(
            "confirmed: {}",
            serde_json::to_string_pretty(&out.result).unwrap()
        );
    }
}
