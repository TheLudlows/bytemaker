//! builtins.rs — built-in hook set (s03/s04).
//!
//! The 5 hooks `Agent` registers by default:
//!
//! | Hook | Event | Responsibility |
//! |---|---|---|
//! | `ContextInjectHook` | `UserPromptSubmit` | log working directory |
//! | `LargeOutputHook` | `PostToolCall` | warn on oversized output |
//! | `SummaryHook` | `PostToolCall` + `Stop` | tally tool-call count (accumulator, compaction-safe) |
//! | `TodoReminderHook` | `PostToolCall` | inject a todo reminder every 3 turns |
//! | `PermissionHook` | `PreToolCall` | three-gate permission pipeline |
//!
//! Each implements the matching trait in `hooks.rs` and is registered via `hooks.on_*` (see `agent::build_hooks`).
//!
//! Registration order invariant: `SummaryHook` must register before `TodoReminderHook`. `PostToolCall`
//! short-circuits on the first `Some(msg)`; `TodoReminderHook` returns `Some` every 3 turns while
//! `SummaryHook` always returns `None`, so placing it first ensures every turn is counted and the reminder
//! is never blocked (see `agent::build_hooks`).
//!
//! `SummaryHook` uses an accumulator instead of recounting `messages`, because compaction rewrites/erases
//! `ToolOutput` blocks. See `docs/modules/builtins.md`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::message::Message;
use crate::hooks::{HookContext, PostToolHook, PreToolHook, PromptHook, StopHook};
use crate::todo::SharedTodoManager;
use crate::tools::registry::ToolRegistry;
use crate::tools::trait_def::PermissionCheck;
use crate::tools::workdir;

/// PostToolCall: inject a todo reminder every 3 turns (aligns with Python s15).
/// Holds a counter: `todo_write` resets it, otherwise it increments each turn and fires/zeroes at 3.
pub struct TodoReminderHook {
    #[allow(dead_code)]  // kept for API compatibility; may need to access the todo list later
    todo_manager: Arc<SharedTodoManager>,
    counter: Arc<std::sync::Mutex<usize>>,
}

impl TodoReminderHook {
    pub fn new(todo_manager: Arc<SharedTodoManager>) -> Self {
        Self {
            todo_manager,
            counter: Arc::new(std::sync::Mutex::new(0)),
        }
    }

    /// Reset the counter (used at subagent isolation boundaries to prevent cross-boundary leaks).
    pub fn reset_counter(&self) {
        *self.counter.lock().unwrap() = 0;
    }
}

#[async_trait]
impl PostToolHook for TodoReminderHook {
    async fn on_post_tool(&self, name: &str, _input: &serde_json::Value, _output: &str) -> Option<String> {
        // Reset counter after todo_write
        if name == "todo_write" {
            *self.counter.lock().unwrap() = 0;
            return None;
        }

        // Increment counter
        let mut counter = self.counter.lock().unwrap();
        *counter += 1;

        // Remind every 3 turns
        if *counter >= 3 {
            *counter = 0;
            Some(format!("<reminder>Update your todos.</reminder>"))
        } else {
            None
        }
    }
}

/// UserPromptSubmit: log the current working directory.
pub struct ContextInjectHook;

#[async_trait]
impl PromptHook for ContextInjectHook {
    async fn on_prompt(&self, _query: &str) {
        tracing::info!("[HOOK] UserPromptSubmit: working in {}", workdir().display());
    }
}

/// PostToolCall: warn on oversized output.
pub struct LargeOutputHook;

#[async_trait]
impl PostToolHook for LargeOutputHook {
    async fn on_post_tool(&self, name: &str, _input: &serde_json::Value, output: &str) -> Option<String> {
        if output.len() > 100_000 {
            tracing::warn!("[HOOK] Large output from {}: {} chars", name, output.len());
        }
        None
    }
}

/// PostToolCall + Stop: tally tool-call count for the session.
///
/// Uses an accumulator (`Arc<AtomicUsize>`) incremented after each tool call, rather than
/// recounting `messages` — `compact_history` / `reactive_compact` / `snip_compact` rewrite
/// `messages` and erase `ToolOutput` blocks, which would make a recount read 0 or partial.
/// `on_stop` reads the accumulator directly.
///
/// `on_post_tool` and `on_stop` each register an instance sharing the same Arc, so `Clone`
/// is a cheap ref copy. `on_post_tool` always returns `None`, never short-circuiting later PostToolCall hooks.
#[derive(Clone)]
pub struct SummaryHook {
    tool_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl SummaryHook {
    pub fn new() -> Self {
        Self {
            tool_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl PostToolHook for SummaryHook {
    async fn on_post_tool(
        &self,
        _name: &str,
        _input: &serde_json::Value,
        _output: &str,
    ) -> Option<String> {
        self.tool_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        None
    }
}

#[async_trait]
impl StopHook for SummaryHook {
    async fn on_stop(&self, _messages: &[Message]) -> Option<String> {
        let tool_count = self.tool_calls.load(std::sync::atomic::Ordering::Relaxed);
        tracing::info!("[HOOK] Stop: session used {} tool calls", tool_count);
        None
    }
}

// ---- permission hooks ----

/// Gate 1: hard-deny list — always forbidden.
/// Uses regex (not plain substring contains) to prevent encoding bypass.
const DENY_PATTERNS: &[&str] = &[
    r"(?i)\brm\s+-rf\s+/?",           // rm -rf / and variants
    r"(?i)\bsudo\b",                    // sudo command
    r"(?i)\b(shutdown|reboot|halt|poweroff)\b",  // system shutdown
    r"(?i)\b(mkfs|dd\s+if=)\b",        // disk format / direct write
    r"(?i)>?\s*/dev/sd[ab]\d?",         // direct write to block device
    r"(?i)\b(chmod)\s+777",             // insecure permissions
    r"(?i)\b(chown)\s+-R\s+root:",      // recursive owner change
];

/// Command patterns requiring extra approval.
const APPROVAL_PATTERNS: &[&str] = &[
    r"(?i)\b(rm|dd|mkfs)\b",           // delete / block-device write
    r"(?i)\b(sudo|su|doas)\b",          // privilege escalation
    r"(?i)\b(curl|wget)\s+.*\|\s*(sh|bash)",  // pipe downloaded content to shell
    r"(?i)\beval\b",                    // eval command
];

/// Check whether a command matches the hard-deny list.
/// Uses regex matching to prevent encoding bypass.
fn check_deny_patterns(command: &str) -> Option<&'static str> {
    for pattern in DENY_PATTERNS {
        let regex = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if regex.is_match(command) {
            // Return a simplified description, not the full regex pattern
            let simple_reason = if pattern.contains("rm") {
                "rm -rf / (destructive command)"
            } else if pattern.contains("sudo") {
                "sudo (privilege escalation)"
            } else if pattern.contains("shutdown") {
                "system shutdown command"
            } else if pattern.contains("dd") {
                "dd (direct disk write)"
            } else if pattern.contains("chmod") {
                "chmod 777 (insecure permissions)"
            } else {
                "dangerous command"
            };
            return Some(simple_reason);
        }
    }
    None
}

/// Check whether a command requires user approval.
fn requires_approval(command: &str) -> Option<&'static str> {
    for pattern in APPROVAL_PATTERNS {
        let regex = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if regex.is_match(command) {
            return Some("This command may modify system state and requires approval");
        }
    }
    None
}

/// Ask the user to approve via `IO::ask_permission` (renders prompt, then reads y/N). No interactive channel → returns false.
async fn ask_via_input(ctx: &HookContext, name: &str, input: &serde_json::Value, reason: &str) -> bool {
    ctx.io.ask_permission(reason, name, input).await
}

/// PreToolCall hook: three gates in series; returns `Some(reason)` to block, `None` to allow.
///
/// Called by the loop via `hooks.trigger_pre_tool()`. The trailing `None` (not `false`)
/// gives the correct "no gate matched → allow" semantics.
pub struct PermissionHook;

#[async_trait]
impl PreToolHook for PermissionHook {
    async fn on_pre_tool(
        &self,
        registry: &ToolRegistry,
        ctx: &HookContext,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<String> {
        // Gate 1: hard-deny (regex match)
        if name == "command" {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(reason) = check_deny_patterns(cmd) {
                ctx.io.output.blocked(reason);
                return Some(format!("Permission denied: {}", reason));
            }
        }

        // Gate 2: check whether user approval is needed
        if name == "command" {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(reason) = requires_approval(cmd) {
                // Gate 3: ask the user to confirm
                if !ask_via_input(ctx, name, input, reason).await {
                    return Some("Permission denied by user".to_string());
                }
            }
        }

        // Gate 4: check tool permission via the registry
        if let Some(permission_check) = registry.check_permission(name, input) {
            match permission_check {
                PermissionCheck::Pass => {
                    // Permission check passed, continue
                }
                PermissionCheck::NeedsApproval(reason) => {
                    // Gate 3: ask the user to confirm
                    if !ask_via_input(ctx, name, input, reason).await {
                        return Some("Permission denied by user".to_string());
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{HookContext, PostToolHook, PreToolHook};
    use crate::todo::TodoManager;
    use crate::tools::registry::ToolRegistry;

    #[tokio::test]
    async fn todo_reminder_counter_every_three_turns() {
        let todo_manager = Arc::new(SharedTodoManager::new(TodoManager::new()));
        let hook = TodoReminderHook::new(Arc::clone(&todo_manager));
        let input = serde_json::json!({});

        // First 2 calls should not remind
        assert!(hook.on_post_tool("command", &input, "").await.is_none());
        assert!(hook.on_post_tool("read_file", &input, "").await.is_none());

        // 3rd call should remind
        let reminder = hook.on_post_tool("write_file", &input, "").await;
        assert!(reminder.is_some());
        assert!(reminder.unwrap().contains("Update your todos"));

        // After reset, 3 more calls needed to trigger
        assert!(hook.on_post_tool("command", &input, "").await.is_none());
        assert!(hook.on_post_tool("read_file", &input, "").await.is_none());
        assert!(hook.on_post_tool("write_file", &input, "").await.is_some());
    }

    #[tokio::test]
    async fn todo_reminder_resets_on_todo_write() {
        let todo_manager = Arc::new(SharedTodoManager::new(TodoManager::new()));
        let hook = TodoReminderHook::new(Arc::clone(&todo_manager));
        let input = serde_json::json!({});

        // Call 2 ordinary tools
        assert!(hook.on_post_tool("command", &input, "").await.is_none());
        assert!(hook.on_post_tool("read_file", &input, "").await.is_none());

        // Calling todo_write should reset the counter
        assert!(hook.on_post_tool("todo_write", &input, "").await.is_none());

        // 3 more ordinary tool calls needed to trigger
        assert!(hook.on_post_tool("command", &input, "").await.is_none());
        assert!(hook.on_post_tool("read_file", &input, "").await.is_none());
        assert!(hook.on_post_tool("write_file", &input, "").await.is_some());
    }

    #[tokio::test]
    async fn permission_hook_gate1_denies_destructive() {
        let hook = PermissionHook;
        let ctx = HookContext::test_noop();
        let registry = ToolRegistry::new();
        let result = hook
            .on_pre_tool(&registry, &ctx, "command", &serde_json::json!({"command": "rm -rf /"}))
            .await;
        assert!(
            matches!(result, Some(ref r) if r.contains("Permission denied")),
            "destructive command must be denied at gate 1, got {:?}", result
        );
    }

    #[tokio::test]
    async fn permission_hook_non_interactive_denies_approval_gated() {
        // ctx.ask = None (no InputTask wired yet) → approval-gated command is denied, never hangs.
        let hook = PermissionHook;
        let ctx = HookContext::test_noop();
        let registry = ToolRegistry::new();
        let result = hook
            .on_pre_tool(&registry, &ctx, "command", &serde_json::json!({"command": "rm foo"}))
            .await;
        assert_eq!(
            result,
            Some("Permission denied by user".to_string()),
            "non-interactive approval must deny, got {:?}", result
        );
    }
}


