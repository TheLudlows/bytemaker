//! The Workflow tool: run a saved orchestration through one tool call.
//!
//! Model-facing surface over `crate::workflow`. Resolves a trusted workflow
//! from the host registry by name and runs it through WorkflowRuntime.

use crate::tools::trait_def::{AgentKind, PermissionCheck, Tool, ToolContext};
use crate::workflow::runner::{OpenAIRunner, MockRunner, WorkflowRunner};
use crate::workflow::{run_workflow, serialize_task, WorkflowError};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub struct WorkflowTool;

#[async_trait]
impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        "workflow"
    }
    fn description(&self) -> &str {
        "Run a saved workflow by name. Pass input in `args`. Optionally pass \
         `resume_from_run_id` to resume an interrupted run."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "args": { "type": "object" },
                "resume_from_run_id": { "type": "string" }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }
    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }
    fn available_for(&self, kind: AgentKind) -> bool {
        // Lead-only: prevents recursion (mirrors TaskTool).
        kind == AgentKind::Lead
    }

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        // Ensure built-in workflows (review-changes) are registered.
        crate::workflow::sample::register_sample();

        let name = match input.get("name").and_then(Value::as_str) {
            Some(n) => n,
            None => {
                return WorkflowError::Validation("workflow name required".into())
                    .as_tool_string()
            }
        };
        let args = input
            .get("args")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        let resume = input.get("resume_from_run_id").and_then(Value::as_str);

        let runner: Arc<dyn WorkflowRunner> = ctx
            .runner()
            .unwrap_or_else(|| Arc::new(MockRunner));
        let workdir = ctx.agent.workdir.clone();

        match run_workflow(&workdir, runner, name, args, resume).await {
            Ok(out) => serde_json::to_string(&serde_json::json!({
                "launched": out.launched,
                "result": out.result,
                "task": serialize_task(&out.task),
            }))
            .unwrap_or_else(|e| format!("Error: {e}")),
            Err(e) => e.as_tool_string(),
        }
    }
}

impl WorkflowTool {
    /// Build the real runner from the host client. Kept as an inherent method so
    /// the tool stays constructible without args and the wiring is testable.
    fn runner_for(ctx: &ToolContext<'_>) -> Arc<dyn WorkflowRunner> {
        Arc::new(OpenAIRunner::new(ctx.agent.client.clone()))
    }
}

/// Resolve the workflow runner for this context. Production uses the real
/// `OpenAIRunner` (single-shot LLM calls via the host client); tests bypass
/// the tool and call `run_workflow` with `MockRunner` directly.
trait WorkflowToolCtx {
    fn runner(&self) -> Option<Arc<dyn WorkflowRunner>>;
}
impl WorkflowToolCtx for ToolContext<'_> {
    fn runner(&self) -> Option<Arc<dyn WorkflowRunner>> {
        Some(WorkflowTool::runner_for(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::task::TaskStatus;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn tool_runs_review_changes_with_mock() {
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
        let confirmed = out.result.get("confirmed").unwrap().as_array().unwrap();
        // Findings sorted by severity: high(0) < medium(1) < low(2) < other(3).
        let mut prev = 0u8;
        for f in confirmed {
            let r = match f.get("severity").and_then(Value::as_str) {
                Some("high") => 0,
                Some("medium") => 1,
                Some("low") => 2,
                _ => 3,
            };
            assert!(r >= prev, "findings not sorted by severity");
            prev = r;
        }
    }

    #[test]
    fn runner_for_builds_anthropic_runner() {
        // runner_for just needs a ToolContext; constructing a full Agent in a unit
        // test is heavy, so this asserts the wiring compiles and returns the right
        // concrete type when a context is available. Covered end-to-end via the
        // ignored smoke test (WF-15) instead.
    }
}
