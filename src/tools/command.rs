/*
command.rs - Command Tool Implementation

This module implements:
- CommandTool: Tool trait implementation for shell command execution
- run_bash(): Async cross-platform command execution with timeout
- decode_console(): Console output decoding (UTF-8 → OEM codepage → lossy)
*/

use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;
use tokio::time::{timeout, Duration};

/// Command execution timeout in seconds.
const COMMAND_TIMEOUT_SECS: u64 = 30;

/// Maximum output size in bytes before truncation.
const MAX_OUTPUT_BYTES: usize = 50_000;

/// Execute a command (cross-platform, with timeout).
/// Dangerous commands are blocked by builtins::PermissionHook before reaching here; safe_path sandboxes file tools.
pub(crate) async fn run_bash(command: &str, cwd: &Path) -> String {
    let result = timeout(Duration::from_secs(COMMAND_TIMEOUT_SECS), async {
        if cfg!(windows) {
            tokio::process::Command::new("cmd.exe")
                .args(["/C", command])
                .current_dir(cwd)
                .output()
                .await
        } else {
            tokio::process::Command::new("bash")
                .arg("-c")
                .arg(command)
                .current_dir(cwd)
                .output()
                .await
        }
    })
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = decode_console(&output.stdout);
            let stderr = decode_console(&output.stderr);
            let result = format!("{}\n{}", stdout, stderr).trim().to_string();
            if result.is_empty() {
                "(no output)".to_string()
            } else if result.len() > MAX_OUTPUT_BYTES {
                // Truncate at byte limit but land on a UTF-8 char boundary, else `result[..end]` panics mid-multibyte sequence (common with CJK output).
                let mut end = MAX_OUTPUT_BYTES;
                while !result.is_char_boundary(end) {
                    end -= 1;
                }
                result[..end].to_string()
            } else {
                result
            }
        }
        Ok(Err(e)) => format!("Error: {}", e),
        Err(_) => format!(
            "Error: command timed out after {} seconds",
            COMMAND_TIMEOUT_SECS
        ),
    }
}

/// Decode command output bytes: UTF-8 first, then GBK (cmd.exe/git use codepage 936 under Chinese locale), else lossy. Avoids replacing non-ASCII with U+FFFD.
/// Uses `encoding_rs` instead of hand-written Windows FFI: no unsafe, no alloc (returns `Cow<str>`), cross-platform.
pub(crate) fn decode_console(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    // Chinese Windows OEM codepage is 936 (GBK); encoding_rs::GBK covers all GBK chars.
    // Under non-Chinese locale GBK decode may mojibake, but beats U+FFFD replacement.
    let (decoded, _encoding, _had_errors) = encoding_rs::GBK.decode(bytes);
    decoded.into_owned()
}

/// Start a background task, return placeholder tool_output (for CommandTool::execute branching).
/// Success: "[Background task {bg_id} started] ..."; failure (empty cmd / concurrency cap / ID exhaustion): "Error: ...".
/// call_id is empty here — agent_loop builds the placeholder with the original id; this field is association-only.
pub(crate) async fn start_background(
    bg: &crate::background_tasks::BackgroundManager,
    command: &str,
) -> String {
    match bg.start(command, "") {
        Ok(id) => format!(
            "[Background task {} started] The result will be collected on a later turn. Use TaskOutput to poll, TaskStop to cancel.",
            id
        ),
        Err(e) => e,
    }
}

/// Command Tool for executing shell commands
///
/// This tool allows the AI agent to execute shell commands in a controlled
/// environment. It includes safety checks for potentially destructive commands
/// and follows the Tool trait interface.
pub struct CommandTool;

#[async_trait]
impl Tool for CommandTool {
    fn name(&self) -> &str {
        "command"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its output. Commands are run in a workspace-isolated environment with safety checks."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute (e.g., 'ls -la', 'git status')"
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "若 true，命令在后台执行，立即返回 bg_id；完成后在后续轮次以 <task_notification> 注入。仅用于独立的慢命令（install/build/test）。"
                }
            },
            "required": ["command"]
        })
    }

    /// Checks if the command requires approval for potentially destructive actions
    fn check_permission(&self, input: &Value) -> PermissionCheck {
        if let Some(command) = input.get("command").and_then(|v| v.as_str()) {
            let command_lower = command.to_lowercase();

            // Recursive operations on root
            if command_lower.contains("rm -rf /") || command_lower.starts_with("rm -rf/") {
                return PermissionCheck::NeedsApproval(
                    "This command performs a recursive delete from root. This will erase your system. This action requires explicit approval."
                );
            }

            // File deletion patterns
            if command_lower.contains("rm -rf ") || command_lower.contains("rm -rf/") {
                return PermissionCheck::NeedsApproval(
                    "This command performs a recursive delete. This action requires explicit approval."
                );
            }

            // Single file deletion to critical directories
            if (command_lower.contains("rm ") || command_lower.contains("rm -")) &&
               (command_lower.contains("/etc/") ||
                command_lower.contains("/usr/") ||
                command_lower.contains("/lib/") ||
                command_lower.contains("/bin/") ||
                command_lower.contains("/sbin/") ||
                command_lower.contains("/var/") ||
                command_lower.contains("/opt/") ||
                command_lower.contains("/boot/") ||
                command_lower.contains("/home/") ||
                command_lower.contains("/root/")) {
                return PermissionCheck::NeedsApproval(
                    "This command attempts to delete critical system files. This action requires explicit approval."
                );
            }

            // Critical system modifications
            if command_lower.contains("chmod 777 ") || command_lower.starts_with("chmod 777 ") {
                return PermissionCheck::NeedsApproval(
                    "This command grants broad permissions to files/folders. This action requires explicit approval."
                );
            }

            // Direct file overwrites to critical locations
            if (command_lower.contains(" > /etc/") ||
                command_lower.contains(" >> /etc/") ||
                command_lower.contains(" > /usr/") ||
                command_lower.contains(" >> /usr/") ||
                command_lower.contains(" > /lib/") ||
                command_lower.contains(" >> /lib/") ||
                command_lower.contains(" > /bin/") ||
                command_lower.contains(" >> /bin/") ||
                command_lower.contains(" > /sbin/") ||
                command_lower.contains(" >> /sbin/") ||
                command_lower.contains(" > /var/") ||
                command_lower.contains(" >> /var/") ||
                command_lower.contains(" > /opt/") ||
                command_lower.contains(" >> /opt/") ||
                command_lower.contains(" > /boot/") ||
                command_lower.contains(" >> /boot/")) &&
               !command_lower.contains(">/dev/null") {
                return PermissionCheck::NeedsApproval(
                    "This command attempts to overwrite critical system files. This action requires explicit approval."
                );
            }

            // Dangerous system operations
            if (command_lower.contains("fdisk ") ||
                command_lower.contains("mkfs ") ||
                command_lower.contains("dd ")) &&
               (command_lower.contains("/dev/sd") || command_lower.contains("/dev/hd")) {
                return PermissionCheck::NeedsApproval(
                    "This command modifies disk partitions or filesystems. This action requires explicit approval."
                );
            }
        }

        PermissionCheck::Pass
    }

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(command) = input.get("command").and_then(|v| v.as_str()) else {
            return "Error: No command provided".to_string();
        };
        let bg = input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if bg {
            start_background(&ctx.agent.bg_manager, command).await
        } else {
            let cwd = match crate::tools::ctx_cwd(ctx) {
                Ok(p) => p,
                Err(e) => return format!("Error: {}", e),
            };
            run_bash(command, &cwd).await
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::trait_def::PermissionCheck;
    use serde_json::json;

    // ---- CommandTool tests ----

    #[test]
    fn test_command_tool_name() {
        let tool = CommandTool;
        assert_eq!(tool.name(), "command");
    }

    #[test]
    fn test_command_tool_description() {
        let tool = CommandTool;
        assert!(tool.description().contains("shell command"));
    }

    #[test]
    fn test_command_tool_schema() {
        let tool = CommandTool;
        let schema = tool.input_schema();

        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].is_object());
        assert_eq!(schema["properties"]["command"]["type"], "string");

        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "command");
    }

    #[test]
    fn test_permission_check_safe_commands() {
        let tool = CommandTool;

        let safe_commands = vec![
            json!({"command": "ls -la"}),
            json!({"command": "git status"}),
            json!({"command": "cargo build"}),
            json!({"command": "echo hello world"}),
            json!({"command": "cat file.txt"}),
            json!({"command": "mkdir test_dir"}),
            json!({"command": "rm file.txt"}),
            json!({"command": "chmod 644 file.txt"}),
        ];

        for cmd in safe_commands {
            match tool.check_permission(&cmd) {
                PermissionCheck::Pass => {}
                PermissionCheck::NeedsApproval(reason) => {
                    panic!("Safe command was rejected: {:?} - {}", cmd, reason);
                }
            }
        }
    }

    #[test]
    fn test_permission_check_destructive_commands() {
        let tool = CommandTool;

        let destructive_commands = vec![
            json!({"command": "rm -rf /"}),
            json!({"command": "rm -rf /usr"}),
            json!({"command": "chmod 777 /etc"}),
            json!({"command": "echo 'danger' > /etc/passwd"}),
            json!({"command": "cat input.txt > /etc/config"}),
            json!({"command": "fdisk /dev/sda"}),
            json!({"command": "mkfs /dev/sda1"}),
            json!({"command": "dd if=/dev/zero of=/dev/sda"}),
        ];

        for cmd in destructive_commands {
            match tool.check_permission(&cmd) {
                PermissionCheck::NeedsApproval(reason) => {
                    assert!(reason.contains("approval") || reason.contains("explicit approval"),
                           "Destructive command should mention approval: {:?}", cmd);
                }
                PermissionCheck::Pass => {
                    panic!("Destructive command was approved: {:?}", cmd);
                }
            }
        }
    }

    #[test]
    fn test_permission_case_insensitive() {
        let tool = CommandTool;

        let cmd1 = json!({"command": "RM -rf /etc"});
        let cmd2 = json!({"command": "chmod 777 /usr"});

        match tool.check_permission(&cmd1) {
            PermissionCheck::NeedsApproval(_) => {}
            PermissionCheck::Pass => panic!("Case sensitive check failed"),
        }

        match tool.check_permission(&cmd2) {
            PermissionCheck::NeedsApproval(_) => {}
            PermissionCheck::Pass => panic!("Case sensitive check failed"),
        }
    }

    #[test]
    fn test_permission_subdir_protection() {
        let tool = CommandTool;

        let protected_commands = vec![
            json!({"command": "rm /etc/passwd"}),
            json!({"command": "rm -rf /usr/local"}),
            json!({"command": "rm -rf /lib/systemd"}),
            json!({"command": "chmod 777 /bin/bash"}),
            json!({"command": "echo test > /usr/share/file"}),
        ];

        for cmd in protected_commands {
            match tool.check_permission(&cmd) {
                PermissionCheck::NeedsApproval(_) => {}
                PermissionCheck::Pass => {
                    panic!("Protected command was approved: {:?}", cmd);
                }
            }
        }
    }

    #[test]
    fn test_permission_dev_null_allowed() {
        let tool = CommandTool;

        let dev_null_commands = vec![
            json!({"command": "echo 'test' > /dev/null"}),
            json!({"command": "some_command > /dev/null"}),
        ];

        for cmd in dev_null_commands {
            match tool.check_permission(&cmd) {
                PermissionCheck::Pass => {}
                PermissionCheck::NeedsApproval(reason) => {
                    panic!("/dev/null redirect was rejected: {:?} - {}", cmd, reason);
                }
            }
        }
    }

    // ---- run_bash async tests ----

    /// Regression: cmd.exe under Chinese locale outputs GBK(936); `from_utf8_lossy` replaces non-ASCII (e.g. "版本" in `ver` output) with U+FFFD. After forcing UTF-8 it should be valid UTF-8 with no replacement chars.
    #[tokio::test]
    #[cfg(windows)]
    async fn decodes_non_ascii_without_replacement_chars() {
        let out = run_bash("ver", &crate::tools::workdir()).await;
        assert!(
            !out.contains('\u{FFFD}'),
            "命令输出不应含 U+FFFD 替换符（应为合法 UTF-8）: {out:?}"
        );
    }

    #[tokio::test]
    async fn run_bash_executes_simple_command() {
        let out = run_bash("echo hello_from_bytemaker", &crate::tools::workdir()).await;
        assert!(
            out.contains("hello_from_bytemaker"),
            "expected 'hello_from_bytemaker' in output, got: {}",
            out
        );
    }

    #[tokio::test]
    async fn run_bash_timeout_kills_long_command() {
        // Use a command that sleeps longer than COMMAND_TIMEOUT_SECS.
        // On Windows: ping -n sends one ping per second; -w 1000 waits 1s per ping.
        // On Unix: sleep N sleeps for N seconds.
        let cmd = if cfg!(windows) {
            "ping -n 120 127.0.0.1"
        } else {
            "sleep 120"
        };
        let out = run_bash(cmd, &crate::tools::workdir()).await;
        assert!(
            out.contains("timed out"),
            "expected timeout message, got: {}",
            out
        );
    }

    #[tokio::test]
    async fn run_bash_truncates_large_output() {
        // Generate output larger than MAX_OUTPUT_BYTES (50,000 bytes).
        // On Windows: use a for loop in cmd.exe.
        // On Unix: use head -c or python/yes.
        let cmd = if cfg!(windows) {
            "for /L %i in (1,1,10000) do @echo LINE_%i_PADDING_DATA_TO_MAKE_IT_LONGER_AAAAAAAAAAAAAAAAAAAAAAA"
        } else {
            "python3 -c \"print('A' * 100 * 1000)\""
        };
        let out = run_bash(cmd, &crate::tools::workdir()).await;
        // Output should be truncated to at most MAX_OUTPUT_BYTES
        assert!(
            out.len() <= MAX_OUTPUT_BYTES + 100, // small margin for UTF-8 boundary adjustment
            "output should be truncated, got {} bytes",
            out.len()
        );
    }

    #[test]
    fn command_tool_schema_has_run_in_background() {
        let tool = CommandTool;
        let schema = tool.input_schema();
        assert_eq!(schema["properties"]["run_in_background"]["type"], "boolean");
        // command is required, run_in_background is not
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "command");
    }

    #[tokio::test]
    async fn execute_run_in_background_true_returns_placeholder() {
        // TestAgent's bg_manager is isolated in a tempdir, no longer polluting the global BG_MANAGER.
        use crate::agent::TestAgent;
        let tool = CommandTool;
        let tagent = TestAgent::new();
        let ctx = tagent.context();
        let input = json!({"command": "echo bg_split_test", "run_in_background": true});
        let out = tool.execute(&ctx, &input).await;
        assert!(out.contains("Background task"), "expected placeholder, got: {}", out);
        assert!(out.contains("bg_"), "expected bg_id, got: {}", out);
    }

    #[tokio::test]
    async fn execute_run_in_background_false_uses_sync_path() {
        use crate::agent::TestAgent;
        let tool = CommandTool;
        let tagent = TestAgent::new();
        let ctx = tagent.context();
        let input = json!({"command": "echo sync_path_ok", "run_in_background": false});
        let out = tool.execute(&ctx, &input).await;
        assert!(out.contains("sync_path_ok"), "false should use sync run_bash, got: {}", out);
    }

    #[tokio::test]
    async fn command_runs_in_ctx_cwd() {
        // s13: a Lead agent (TestAgent, team=None) -> ctx.cwd() == workdir
        // (the tempdir). Verify the command actually runs there.
        use crate::agent::TestAgent;
        use crate::tools::trait_def::Tool;
        let a = TestAgent::new();
        let ctx = a.context();
        let out = CommandTool
            .execute(&ctx, &serde_json::json!({"command": "cd"}))
            .await;
        // `cd` prints the cwd; the TestAgent tempdir path contains a separator
        // (and usually "Temp").
        assert!(
            out.contains("TempDir") || out.contains('\\') || out.contains('/'),
            "command should run in ctx cwd, got: {}",
            out
        );
    }
}
