/*
tools.rs - background task tools and hook (s11)

TaskOutputTool / TaskStopTool expose BackgroundManager to the model;
BackgroundStopHook actively injects finished notifications before loop exit (wakes the loop);
collect_and_inject drains passively at the top of the loop.
BackgroundManager is held by the Agent (Arc) and passed down via ToolContext.agent.bg_manager;
BackgroundStopHook receives it via constructor DI instead of a process-global.
*/

use crate::background_tasks::manager::BackgroundManager;
use crate::hooks::StopHook;
use crate::tools::trait_def::{AgentKind, PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// Active wake-up: if ready is non-empty before loop exit, return notifications to force
/// another turn (mirrors hooks.rs StopHook semantics). Holds bg_manager via constructor DI;
/// Agent::build_hooks injects it.
pub struct BackgroundStopHook {
    bg_manager: Arc<BackgroundManager>,
}

impl BackgroundStopHook {
    pub fn new(bg_manager: Arc<BackgroundManager>) -> Self {
        Self { bg_manager }
    }
}

#[async_trait]
impl StopHook for BackgroundStopHook {
    async fn on_stop(&self, _messages: &[crate::domain::message::Message]) -> Option<String> {
        let notifications = self.bg_manager.collect();
        if notifications.is_empty() {
            None
        } else {
            Some(notifications.join("\n"))
        }
    }
}

/// TaskOutput tool: poll or block for a background task's output and status.
pub struct TaskOutputTool;

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "task_output"
    }

    fn description(&self) -> &str {
        "Get the status and output of a background task. Set block=true to wait (with timeout) for it to finish."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" },
                "block": { "type": "boolean", "default": false },
                "timeout_ms": { "type": "integer", "default": 30000 }
            },
            "required": ["task_id"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(task_id) = input.get("task_id").and_then(|v| v.as_str()) else {
            return "Error: task_id required".to_string();
        };
        let block = input.get("block").and_then(|v| v.as_bool()).unwrap_or(false);
        let timeout_ms = input
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(30_000);
        ctx.agent.bg_manager.output(task_id, block, timeout_ms).await
    }

    /// Background-task tools stay available to Lead and subagents, but are
    /// withheld from teammates (s13: "do not bring s11 background tasks into
    /// teammate logic").
    fn available_for(&self, kind: AgentKind) -> bool {
        kind != AgentKind::Teammate
    }
}

/// TaskStop tool: cancel a background task and kill its process tree.
pub struct TaskStopTool;

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "task_stop"
    }

    fn description(&self) -> &str {
        "Stop a running background task by cancelling it and killing its process tree."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" }
            },
            "required": ["task_id"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(task_id) = input.get("task_id").and_then(|v| v.as_str()) else {
            return "Error: task_id required".to_string();
        };
        ctx.agent.bg_manager.stop(task_id)
    }

    fn available_for(&self, kind: AgentKind) -> bool {
        kind != AgentKind::Teammate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn task_output_tool_metadata() {
        let t = TaskOutputTool;
        assert_eq!(t.name(), "task_output");
        assert!(t.description().contains("background"));
        let s = t.input_schema();
        assert_eq!(s["required"][0], "task_id");
        assert_eq!(s["properties"]["block"]["type"], "boolean");
        assert_eq!(t.check_permission(&json!({})), PermissionCheck::Pass);
        assert!(t.available_for(AgentKind::Subagent));
        assert!(!t.available_for(AgentKind::Teammate));
    }

    #[test]
    fn task_stop_tool_metadata() {
        let t = TaskStopTool;
        assert_eq!(t.name(), "task_stop");
        let s = t.input_schema();
        assert_eq!(s["required"][0], "task_id");
        assert_eq!(t.check_permission(&json!({})), PermissionCheck::Pass);
        assert!(t.available_for(AgentKind::Subagent));
        assert!(!t.available_for(AgentKind::Teammate));
    }

    #[test]
    fn collect_on_fresh_manager_is_empty() {
        // Use a standalone manager to verify the empty-collect contract (no global pollution).
        let mgr = Arc::new(BackgroundManager::new(
            std::env::temp_dir().join("bg_test_empty_collect"),
        ));
        assert!(mgr.collect().is_empty());
    }
}
