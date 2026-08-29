/*
todo_write.rs - Todo Write Tool

- Tool trait impl for todo management
- Delegates to ctx.agent.todo_manager.run_todo_write()
*/

use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;

/// Todo Write Tool — update the todo list with content/status items.
pub struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    /// Returns the tool's name
    fn name(&self) -> &str {
        "todo_write"
    }

    /// Returns a human-readable description
    fn description(&self) -> &str {
        "Update the todo list with new tasks. Accepts an array of todo items with content and status."
    }

    /// Returns the JSON schema for todo_write input
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "Array of todo items to update",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Description of the todo task"
                            },
                            "status": {
                                "type": "string",
                                "description": "Status of the task: 'pending', 'in_progress', or 'completed'",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["content"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    /// Checks permission - always allow todo write operations
    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        // Default: allow todo write operations
        PermissionCheck::Pass
    }

    /// Executes the todo write via ctx.agent.todo_manager.run_todo_write()
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let todos = match input.get("todos") {
            Some(v) if v.is_array() => v,
            _ => return "Error: todos must be an array".to_string(),
        };

        // Pass the output context and the todos array
        ctx.agent.todo_manager.run_todo_write(&*ctx.agent.io.output, todos)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_todo_write_tool_name() {
        let tool = TodoWriteTool;
        assert_eq!(tool.name(), "todo_write");
        assert!(tool.available_for(crate::tools::trait_def::AgentKind::Teammate));
    }


    #[test]
    fn test_permission_check() {
        let tool = TodoWriteTool;
        let test_inputs = vec![
            json!({"todos": []}),
            json!({"todos": [{"content": "Test task"}]}),
            json!({"todos": [{"content": "Test task", "status": "pending"}]}),
            json!({}),
        ];

        for input in test_inputs {
            match tool.check_permission(&input) {
                PermissionCheck::Pass => {} // Expected
                PermissionCheck::NeedsApproval(reason) => {
                    panic!("Todo write should not need approval: {:?} - {}", input, reason);
                }
            }
        }
    }

    #[tokio::test]
    async fn test_exe() {
        use crate::agent::TestAgent;

        let tool = TodoWriteTool;
        let tagent = TestAgent::new();
        let ctx = tagent.context();

        // Valid input: empty todos, returns "No todos."
        let result = tool.execute(&ctx, &json!({"todos": []})).await;
        assert_eq!(result, "No todos.");

        // Valid input: todos with content
        let result = tool.execute(&ctx, &json!({"todos": [{"content": "Test task", "status": "pending"}]})).await;
        assert!(result.contains("Test task"), "should contain task text: {}", result);

        // Invalid input (missing todos field): returns Error
        let result = tool.execute(&ctx, &json!({})).await;
        assert!(result.starts_with("Error"), "missing todos should error: {}", result);
    }
}