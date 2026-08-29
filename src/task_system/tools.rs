/*
tools.rs - Tool implementations for s10 Task System

Implements create_task, list_tasks, get_task, claim_task, complete_task tools.
TaskStore is held by Agent (Arc) and passed via ToolContext.agent.task_store.
*/

use crate::task_system::store::{TaskStore, TaskStoreError};
use crate::task_system::task::{Task, TaskStatus};
use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;

/// Convert TaskStoreError to a tool output string.
fn error_to_output(e: TaskStoreError) -> String {
    error_str(&e)
}

/// Convert by reference (TaskStoreError isn't Clone).
fn error_str(e: &TaskStoreError) -> String {
    format!("Error: {}", e)
}

/// Serialize status as a bare word (`in_progress`) for user-facing messages.
fn status_word(status: TaskStatus) -> String {
    serde_json::to_string(&status)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

/// Return unsatisfied dependencies (missing or not `Completed`).
/// Used by `claim_task` and `complete_task`.
fn incomplete_dependencies(store: &TaskStore, task: &Task) -> Vec<String> {
    let mut incomplete = Vec::new();
    for dep_id in &task.blocked_by {
        match store.load(dep_id) {
            Ok(dep_task) => {
                if dep_task.status != TaskStatus::Completed {
                    incomplete.push(dep_id.clone());
                }
            }
            Err(_) => {
                // Missing dependency counts as incomplete
                incomplete.push(dep_id.clone());
            }
        }
    }
    incomplete
}

/// Claim a task: set Pending → InProgress and record owner.
/// Returns rejection message (no store change) if not Pending or deps incomplete.
pub fn claim_task(store: &TaskStore, task_id: &str, owner: &str) -> String {
    match store.load(task_id) {
        Ok(mut task) => {
            // Check status
            if task.status != TaskStatus::Pending {
                return format!(
                    "Task {} is {}, cannot claim",
                    task_id,
                    status_word(task.status)
                );
            }

            // Check dependencies
            let incomplete = incomplete_dependencies(store, &task);
            if !incomplete.is_empty() {
                return format!("Blocked by: {:?}", incomplete);
            }

            // Claim the task
            task.status = TaskStatus::InProgress;
            task.owner = Some(owner.to_string());

            if let Err(e) = store.save(&task) {
                return error_to_output(e);
            }

            format!("Claimed {} ({})", task.id, task.subject)
        }
        Err(e) => error_to_output(e),
    }
}

/// Complete a task: validate InProgress + owner, set to Completed, report newly unblocked tasks.
/// Newly unblocked = Pending tasks satisfied only after completion.
pub fn complete_task(store: &TaskStore, task_id: &str, owner: &str) -> String {
    use std::collections::HashSet;

    // Snapshot ready tasks before completion to diff out the newly unblocked set.
    let ready_before: HashSet<String> = store
        .list()
        .unwrap_or_default()
        .iter()
        .filter(|t| t.status == TaskStatus::Pending && !t.blocked_by.is_empty())
        .filter(|t| incomplete_dependencies(store, t).is_empty())
        .map(|t| t.id.clone())
        .collect();

    match store.load(task_id) {
        Ok(mut task) => {
            // Check status
            if task.status != TaskStatus::InProgress {
                return format!(
                    "Task {} is {}, cannot complete",
                    task_id,
                    status_word(task.status)
                );
            }

            // Check owner
            if task.owner.as_deref() != Some(owner) {
                return format!(
                    "Task {} is owned by {}, not {}",
                    task_id,
                    task.owner.as_deref().unwrap_or("none"),
                    owner
                );
            }

            // Complete the task
            task.status = TaskStatus::Completed;

            if let Err(e) = store.save(&task) {
                return error_to_output(e);
            }

            // Compute newly unblocked tasks: not ready before, ready after completion.
            let unblocked: Vec<String> = store
                .list()
                .unwrap_or_default()
                .iter()
                .filter(|t| t.status == TaskStatus::Pending && !t.blocked_by.is_empty())
                .filter(|t| !ready_before.contains(&t.id))
                .filter(|t| incomplete_dependencies(store, t).is_empty())
                .map(|t| t.subject.clone())
                .collect();

            let mut msg = format!("Completed {} ({})", task.id, task.subject);
            if !unblocked.is_empty() {
                msg.push_str(&format!("\nUnblocked: {}", unblocked.join(", ")));
            }
            msg
        }
        Err(e) => error_to_output(e),
    }
}

pub struct CreateTaskTool;

#[async_trait]
impl Tool for CreateTaskTool {
    fn name(&self) -> &str {
        "create_task"
    }

    fn description(&self) -> &str {
        "Create a task with optional dependencies. The task is stored in .tasks/{id}.json"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "subject": {
                    "type": "string",
                    "description": "Brief title for the task"
                },
                "description": {
                    "type": "string",
                    "description": "Detailed description of the task"
                },
                "blockedBy": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "List of task IDs this task depends on"
                }
            },
            "required": ["subject"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let subject = input
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let blocked_by = input
            .get("blockedBy")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let store = ctx.agent.task_store.clone();
        match store.create(subject.to_string(), description.to_string(), blocked_by) {
            Ok(task) => {
                let deps_str = if task.blocked_by.is_empty() {
                    String::new()
                } else {
                    format!(" (blockedBy: {})", task.blocked_by.join(", "))
                };
                format!("Created {}: {}{}", task.id, task.subject, deps_str)
            }
            Err(e) => error_to_output(e),
        }
    }
}

pub struct ListTasksTool;

#[async_trait]
impl Tool for ListTasksTool {
    fn name(&self) -> &str {
        "list_tasks"
    }

    fn description(&self) -> &str {
        "List all tasks with their status, owner, and dependencies"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, ctx: &ToolContext<'_>, _input: &Value) -> String {
        let store = ctx.agent.task_store.clone();
        match store.list() {
            Ok(tasks) => {
                if tasks.is_empty() {
                    return "No tasks. Use create_task to add some.".to_string();
                }

                let mut lines = Vec::new();
                for task in tasks {
                    let marker = match task.status {
                        TaskStatus::Pending => "[ ]",
                        TaskStatus::InProgress => "[>]",
                        TaskStatus::Completed => "[x]",
                    };
                    let deps_str = if task.blocked_by.is_empty() {
                        String::new()
                    } else {
                        format!(" (blockedBy: {})", task.blocked_by.join(", "))
                    };
                    let owner_str = task
                        .owner
                        .as_ref()
                        .map_or(String::new(), |o| format!(" [{}]", o));

                    lines.push(format!(
                        "{} {}: {} [{}]{}{}",
                        marker,
                        task.id,
                        task.subject,
                        serde_json::to_string(&task.status).unwrap(),
                        owner_str,
                        deps_str
                    ));
                }
                lines.join("\n")
            }
            Err(e) => error_to_output(e),
        }
    }
}

pub struct GetTaskTool;

#[async_trait]
impl Tool for GetTaskTool {
    fn name(&self) -> &str {
        "get_task"
    }

    fn description(&self) -> &str {
        "Get a task by ID, returning full task details"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task ID to retrieve"
                }
            },
            "required": ["task_id"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let task_id = input
            .get("task_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let store = ctx.agent.task_store.clone();
        match store.load(task_id) {
            Ok(task) => serde_json::to_string_pretty(&task)
                .unwrap_or_else(|_| "Error: serialization failed".to_string()),
            Err(e) => error_to_output(e),
        }
    }
}

pub struct ClaimTaskTool;

#[async_trait]
impl Tool for ClaimTaskTool {
    fn name(&self) -> &str {
        "claim_task"
    }

    fn description(&self) -> &str {
        "Claim a pending task whose dependencies are complete. Sets owner and status to in_progress"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task ID to claim"
                }
            },
            "required": ["task_id"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let task_id = input
            .get("task_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let owner = ctx.agent.owner.as_str();
        if let Some(team) = &ctx.agent.team {
            crate::team::claim_task(team, task_id, owner)
        } else {
            claim_task(&ctx.agent.task_store, task_id, owner)
        }
    }
}

pub struct CompleteTaskTool;

#[async_trait]
impl Tool for CompleteTaskTool {
    fn name(&self) -> &str {
        "complete_task"
    }

    fn description(&self) -> &str {
        "Complete the task claimed by this agent. Returns list of newly unblocked tasks"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task ID to complete"
                }
            },
            "required": ["task_id"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let task_id = input
            .get("task_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let owner = ctx.agent.owner.as_str();
        if let Some(team) = &ctx.agent.team {
            crate::team::complete_task(team, task_id, owner)
        } else {
            complete_task(&ctx.agent.task_store, task_id, owner)
        }
    }
}

#[cfg(test)]
mod tool_tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;
    use crate::task_system::store::create_test_store;

    #[test]
    fn test_create_tool_name_and_description() {
        let tool = CreateTaskTool;
        assert_eq!(tool.name(), "create_task");
        assert!(tool.description().contains("Create a task"));
    }

    #[test]
    fn test_create_tool_schema() {
        let tool = CreateTaskTool;
        let schema = tool.input_schema();

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["subject"]["type"], "string");
        assert_eq!(schema["properties"]["description"]["type"], "string");
        assert_eq!(schema["properties"]["blockedBy"]["type"], "array");

        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "subject");
    }

    #[test]
    fn test_create_tool_passes_permission() {
        let tool = CreateTaskTool;
        let input = json!({"subject": "test"});
        let check = tool.check_permission(&input);
        assert!(matches!(check, PermissionCheck::Pass));
    }

    #[test]
    fn test_list_tool_name_and_schema() {
        let tool = ListTasksTool;
        assert_eq!(tool.name(), "list_tasks");
        assert_eq!(tool.input_schema()["type"], "object");
    }

    #[test]
    fn test_list_tool_passes_permission() {
        let tool = ListTasksTool;
        let check = tool.check_permission(&json!({}));
        assert!(matches!(check, PermissionCheck::Pass));
    }

    #[test]
    fn test_get_tool_name_and_schema() {
        let tool = GetTaskTool;
        assert_eq!(tool.name(), "get_task");
        assert_eq!(
            tool.input_schema()["required"].as_array().unwrap()[0],
            "task_id"
        );
    }

    #[test]
    fn test_incomplete_dependencies_with_no_deps() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());
        let task = Task {
            id: "task_12345678".to_string(),
            subject: "Test".to_string(),
            description: "".to_string(),
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: vec![],
            worktree: None,
        };

        let incomplete = incomplete_dependencies(&store, &task);
        assert!(incomplete.is_empty());
    }

    #[test]
    fn test_incomplete_dependencies_with_completed_deps() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());
        let dep = store
            .create("Dependency".to_string(), "".to_string(), vec![])
            .unwrap();

        // Complete the dependency
        let mut dep_completed = dep.clone();
        dep_completed.status = TaskStatus::Completed;
        store.save(&dep_completed).unwrap();

        let task = Task {
            id: "task_12345678".to_string(),
            subject: "Test".to_string(),
            description: "".to_string(),
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: vec![dep.id.clone()],
            worktree: None,
        };

        let incomplete = incomplete_dependencies(&store, &task);
        assert!(incomplete.is_empty());
    }

    #[test]
    fn test_incomplete_dependencies_with_pending_deps() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());
        let dep = store
            .create("Dependency".to_string(), "".to_string(), vec![])
            .unwrap();

        let task = Task {
            id: "task_12345678".to_string(),
            subject: "Test".to_string(),
            description: "".to_string(),
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: vec![dep.id.clone()],
            worktree: None,
        };

        let incomplete = incomplete_dependencies(&store, &task);
        assert_eq!(incomplete.len(), 1);
        assert_eq!(incomplete[0], dep.id);
    }

    #[test]
    fn test_incomplete_dependencies_with_missing_deps() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());

        let task = Task {
            id: "task_12345678".to_string(),
            subject: "Test".to_string(),
            description: "".to_string(),
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: vec!["task_missing".to_string()],
            worktree: None,
        };

        let incomplete = incomplete_dependencies(&store, &task);
        assert_eq!(incomplete.len(), 1);
        assert_eq!(incomplete[0], "task_missing");
    }

    #[test]
    fn test_claim_tool_name_and_schema() {
        let tool = ClaimTaskTool;
        assert_eq!(tool.name(), "claim_task");
        assert_eq!(
            tool.input_schema()["required"].as_array().unwrap()[0],
            "task_id"
        );
    }

    #[test]
    fn test_claim_blocks_in_progress() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());
        let mut task = store
            .create("Test".to_string(), "".to_string(), vec![])
            .unwrap();
        task.status = TaskStatus::InProgress;
        store.save(&task).unwrap();

        let result = claim_task(&store, &task.id, "agent");
        assert!(result.contains("is in_progress"));
    }

    #[test]
    fn test_claim_blocks_on_dependencies() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());
        let dep = store
            .create("Dependency".to_string(), "".to_string(), vec![])
            .unwrap();
        let task = store
            .create("Test".to_string(), "".to_string(), vec![dep.id])
            .unwrap();

        let result = claim_task(&store, &task.id, "agent");
        assert!(result.contains("Blocked by"));
    }

    #[test]
    fn test_complete_tool_name_and_schema() {
        let tool = CompleteTaskTool;
        assert_eq!(tool.name(), "complete_task");
        assert_eq!(
            tool.input_schema()["required"].as_array().unwrap()[0],
            "task_id"
        );
    }

    #[test]
    fn test_complete_blocks_wrong_owner() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());
        let mut task = store
            .create("Test".to_string(), "".to_string(), vec![])
            .unwrap();
        task.status = TaskStatus::InProgress;
        task.owner = Some("owner1".to_string());
        store.save(&task).unwrap();

        let result = complete_task(&store, &task.id, "owner2");
        assert!(result.contains("owned by owner1, not owner2"));
    }

    #[test]
    fn test_complete_unblocks_downstream_tasks() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());
        let schema = store
            .create("Schema".to_string(), "".to_string(), vec![])
            .unwrap();
        let api = store
            .create("API".to_string(), "".to_string(), vec![schema.id.clone()])
            .unwrap();

        // Claim and complete schema
        claim_task(&store, &schema.id, "agent");
        complete_task(&store, &schema.id, "agent");

        // API should now be claimable
        let claim_result = claim_task(&store, &api.id, "agent");
        assert!(claim_result.contains("Claimed"));
    }
}
