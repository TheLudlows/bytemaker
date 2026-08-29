//! todo.rs — TodoWrite (s05).
//!
//! `TodoManager` holds the todo list with validation and rendering; `SharedTodoManager`
//! (`Mutex<TodoManager>`) is held by `Agent` via `Arc` and passed through `ToolContext`.
//! Invariants: max 20 todos, at most 1 `in_progress`, case-insensitive status, non-empty content. See `docs/modules/todo.md`.

use std::sync::Mutex;

/// A single todo item.
#[derive(Clone, Debug)]
struct TodoItem {
    content: String,
    status: TodoStatus,
}

/// Todo item status.
#[derive(Clone, Copy, Debug, PartialEq)]
enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// TodoManager - manages the todo list.
pub struct TodoManager {
    items: Vec<TodoItem>,
}

impl Default for TodoManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoManager {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn render(&self) -> String {
        if self.items.is_empty() {
            return "No todos.".to_string();
        }

        let mut lines = Vec::new();
        for todo in &self.items {
            let marker = match todo.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[>]",
                TodoStatus::Completed => "[x]",
            };
            lines.push(format!("{} {}", marker, todo.content));
        }

        let completed = self.items.iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();
        lines.push(format!("\n({}/{}) completed", completed, self.items.len()));

        lines.join("\n")
    }

    /// Parse a status string.
    fn parse_status(s: &str) -> Result<TodoStatus, String> {
        match s.to_lowercase().as_str() {
            "pending" => Ok(TodoStatus::Pending),
            "in_progress" => Ok(TodoStatus::InProgress),
            "completed" => Ok(TodoStatus::Completed),
            other => Err(format!("Invalid status '{}'", other)),
        }
    }

    /// Validate a todo item.
    fn validate_item(index: usize, item: &serde_json::Value) -> Result<TodoItem, String> {
        if !item.is_object() {
            return Err(format!("todos[{}] must be an object", index));
        }

        let content = item.get("content")
            .and_then(|c| c.as_str())
            .map(|s| s.trim())
            .ok_or_else(|| format!("todos[{}] requires content", index))?;

        if content.is_empty() {
            return Err(format!("todos[{}] requires non-empty content", index));
        }

        let status_str = item.get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("pending");
        let status = Self::parse_status(status_str)?;

        Ok(TodoItem {
            content: content.to_string(),
            status,
        })
    }

    pub fn update(&mut self, todos: &serde_json::Value) -> Result<String, String> {
        let todos_array = todos.as_array()
            .ok_or_else(|| "todos must be an array".to_string())?;

        if todos_array.len() > 20 {
            return Err("Max 20 todos allowed".to_string());
        }

        let mut validated = Vec::new();
        let mut in_progress_count = 0;

        for (index, item) in todos_array.iter().enumerate() {
            let todo = Self::validate_item(index, item)?;
            if todo.status == TodoStatus::InProgress {
                in_progress_count += 1;
            }
            validated.push(todo);
        }

        if in_progress_count > 1 {
            return Err("Only one todo can be in_progress at a time".to_string());
        }

        self.items = validated;
        Ok(self.render())
    }
}

/// Thread-safe wrapper: `Mutex<TodoManager>`. Held by `Agent` via `Arc`, replacing the global OnceLock.
///
/// `Mutex<T: Send>` satisfies `Sync` automatically; no `unsafe impl Sync` needed (the old
/// `RefCell` version forced `Sync` on `RefCell`, which panics or triggers UB under multi-threading).
pub struct SharedTodoManager(Mutex<TodoManager>);

impl SharedTodoManager {
    pub fn new(manager: TodoManager) -> Self {
        SharedTodoManager(Mutex::new(manager))
    }

    /// Read-only render of the current todo list (for hook injection).
    pub fn render(&self) -> String {
        let guard = self.0.lock().expect("todo mutex poisoned");
        guard.render()
    }

    /// `todo_write` tool handler: update + render under the lock.
    pub fn run_todo_write(&self, output: &dyn crate::io::Output, todos: &serde_json::Value) -> String {
        let mut guard = self.0.lock().expect("todo mutex poisoned");
        let result = guard.update(todos);

        match result {
            Ok(rendered) => {
                output.heading("Current Tasks", &rendered);
                rendered
            }
            Err(e) => format!("Error: {}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_render() {
        let tm = TodoManager::new();
        assert_eq!(tm.render(), "No todos.");
    }

    #[test]
    fn test_render_with_items() {
        let mut tm = TodoManager::new();
        tm.items = vec![
            TodoItem { content: "Task 1".to_string(), status: TodoStatus::Pending },
            TodoItem { content: "Task 2".to_string(), status: TodoStatus::Completed },
        ];
        let rendered = tm.render();
        assert!(rendered.contains("[ ] Task 1"));
        assert!(rendered.contains("[x] Task 2"));
        assert!(rendered.contains("(1/2) completed"));
    }

    #[test]
    fn test_max_items() {
        let mut tm = TodoManager::new();
        let large_array = serde_json::json!(vec!["{}"; 21]);
        assert!(tm.update(&large_array).is_err());
    }

    #[test]
    fn test_empty_content_rejected() {
        let mut tm = TodoManager::new();
        let invalid = serde_json::json!([{"content": "", "status": "pending"}]);
        assert!(tm.update(&invalid).is_err());
    }

    #[test]
    fn test_invalid_status() {
        let mut tm = TodoManager::new();
        let invalid = serde_json::json!([{"content": "Task", "status": "invalid"}]);
        assert!(tm.update(&invalid).is_err());
    }

    #[test]
    fn test_multiple_in_progress_rejected() {
        let mut tm = TodoManager::new();
        let invalid = serde_json::json!([
            {"content": "Task 1", "status": "in_progress"},
            {"content": "Task 2", "status": "in_progress"}
        ]);
        assert!(tm.update(&invalid).is_err());
    }

    #[test]
    fn test_successful_update() {
        let mut tm = TodoManager::new();
        let valid = serde_json::json!([
            {"content": "Task 1", "status": "pending"},
            {"content": "Task 2", "status": "in_progress"}
        ]);
        assert!(tm.update(&valid).is_ok());
        assert_eq!(tm.items.len(), 2);
    }
}