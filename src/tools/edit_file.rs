/*
edit_file.rs - Edit File Tool Implementation

This module implements:
- EditFileTool: Tool trait implementation for editing file contents
- run_edit_file(): Text replacement with safety checks
*/

use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;
use std::fs;

/// Edit a file (text replacement).
pub(crate) fn run_edit_file(path: &str, old_text: &str, new_text: &str, base: &std::path::Path) -> String {
    match crate::tools::safe_path_in(base, path) {
        Ok(abs_path) => match fs::read_to_string(&abs_path) {
            Ok(content) => {
                if !content.contains(old_text) {
                    return format!("Error: text not found in {}", path);
                }
                let new_content = content.replacen(old_text, new_text, 1);
                match fs::write(&abs_path, &new_content) {
                    Ok(_) => format!("Edited {}", path),
                    Err(e) => format!("Error: {}", e),
                }
            }
            Err(e) => format!("Error: {}", e),
        },
        Err(e) => e,
    }
}

/// Edit File Tool for editing file contents
///
/// This tool allows the AI agent to edit file contents safely by replacing specific text.
/// It includes path safety checks to ensure files are edited within the workspace.
pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing specific text. Requires the old text to match exactly."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit (relative to workspace root)"
                },
                "old_text": {
                    "type": "string",
                    "description": "The text to be replaced in the file"
                },
                "new_text": {
                    "type": "string",
                    "description": "The text to replace the old text with"
                }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    fn check_permission(&self, input: &Value) -> PermissionCheck {
        if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
            if crate::tools::escapes_workspace_lexical(path) {
                return PermissionCheck::NeedsApproval(
                    "This file path appears to escape the workspace boundary. Editing files outside the project could be dangerous."
                );
            }
        }

        PermissionCheck::Pass
    }

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: No file path provided".to_string(),
        };

        let old_text = match input.get("old_text").and_then(|v| v.as_str()) {
            Some(o) => o,
            None => return "Error: No old text provided".to_string(),
        };

        let new_text = match input.get("new_text").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return "Error: No new text provided".to_string(),
        };

        let cwd = match crate::tools::ctx_cwd(ctx) {
            Ok(p) => p,
            Err(e) => return format!("Error: {}", e),
        };
        run_edit_file(path, old_text, new_text, &cwd)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_edit_file_tool_name() {
        let tool = EditFileTool;
        assert_eq!(tool.name(), "edit_file");
    }

    #[test]
    fn test_edit_file_tool_description() {
        let tool = EditFileTool;
        assert!(tool.description().contains("Edit a file"));
    }

    #[test]
    fn test_edit_file_tool_schema() {
        let tool = EditFileTool;
        let schema = tool.input_schema();

        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].is_object());
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["old_text"]["type"], "string");
        assert_eq!(schema["properties"]["new_text"]["type"], "string");

        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 3);
        assert_eq!(required[0], "path");
        assert_eq!(required[1], "old_text");
        assert_eq!(required[2], "new_text");
    }

    #[test]
    fn test_permission_check_safe_paths() {
        let tool = EditFileTool;

        let safe_paths = vec![
            json!({"path": "src/main.rs", "old_text": "// old", "new_text": "// new"}),
            json!({"path": "Cargo.toml", "old_text": "[package]", "new_text": "[package]"}),
            json!({"path": "tests/test.rs", "old_text": "fn old() {}", "new_text": "fn new() {}"}),
            json!({"path": "data/config.json", "old_text": "old_value", "new_text": "new_value"}),
            json!({"path": "./file.txt", "old_text": "hello", "new_text": "world"}),
            json!({"path": "file.txt", "old_text": "old", "new_text": "new"}),
        ];

        for path in safe_paths {
            match tool.check_permission(&path) {
                PermissionCheck::Pass => {}
                PermissionCheck::NeedsApproval(reason) => {
                    panic!("Safe path was rejected: {:?} - {}", path, reason);
                }
            }
        }
    }

    #[test]
    fn test_permission_check_escape_paths() {
        let tool = EditFileTool;

        let escape_paths = vec![
            json!({"path": "../secret.txt", "old_text": "data", "new_text": "new_data"}),
            json!({"path": "../../etc/passwd", "old_text": "root:x:0:0", "new_text": "new_root:x:0:0"}),
            json!({"path": "../../config.ini", "old_text": "old_config", "new_text": "new_config"}),
            json!({"path": "../../../data/file.txt", "old_text": "old", "new_text": "new"}),
            json!({"path": "../.gitconfig", "old_text": "[user]", "new_text": "[user]"}),
            json!({"path": "..\\system32\\config", "old_text": "old", "new_text": "new"}),
        ];

        for path in escape_paths {
            match tool.check_permission(&path) {
                PermissionCheck::NeedsApproval(reason) => {
                    assert!(reason.contains("approval") || reason.contains("explicit approval") || reason.contains("workspace boundary") || reason.contains("dangerous"),
                           "Escape path should mention approval or workspace boundary: {:?} - {}", path, reason);
                }
                PermissionCheck::Pass => {
                    panic!("Escape path was approved: {:?}", path);
                }
            }
        }
    }

    #[test]
    fn test_permission_case_insensitive() {
        let tool = EditFileTool;

        let escape_path = json!({"path": "..\\SECRET.TXT", "old_text": "old", "new_text": "new"});
        match tool.check_permission(&escape_path) {
            PermissionCheck::NeedsApproval(_) => {}
            PermissionCheck::Pass => panic!("Case insensitive check failed"),
        }
    }

    #[test]
    fn test_path_normalization() {
        let tool = EditFileTool;

        let normalized_paths = vec![
            json!({"path": "././file.txt", "old_text": "old", "new_text": "new"}),
            json!({"path": "src/../file.txt", "old_text": "old", "new_text": "new"}),
            json!({"path": "src/../../file.txt", "old_text": "old", "new_text": "new"}),
        ];

        for path in normalized_paths {
            match tool.check_permission(&path) {
                PermissionCheck::Pass => {
                    let path_str = path["path"].as_str().unwrap();
                    assert!(!crate::tools::escapes_workspace_lexical(path_str),
                           "Path should not escape: {:?}", path_str);
                }
                PermissionCheck::NeedsApproval(_) => {
                    let path_str = path["path"].as_str().unwrap();
                    assert!(crate::tools::escapes_workspace_lexical(path_str),
                           "Path should escape: {:?}", path_str);
                }
            }
        }
    }

    #[test]
    fn test_permission_validation() {
        let tool = EditFileTool;

        let no_path = json!({"old_text": "old", "new_text": "new"});
        let no_old_text = json!({"path": "file.txt", "new_text": "new"});
        let no_new_text = json!({"path": "file.txt", "old_text": "old"});

        match tool.check_permission(&no_path) {
            PermissionCheck::Pass => {}
            _ => panic!("No path should be allowed in permission check"),
        }

        match tool.check_permission(&no_old_text) {
            PermissionCheck::Pass => {}
            _ => panic!("No old_text should be allowed in permission check"),
        }

        match tool.check_permission(&no_new_text) {
            PermissionCheck::Pass => {}
            _ => panic!("No new_text should be allowed in permission check"),
        }
    }
}
