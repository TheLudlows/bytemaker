//! hooks.rs — hook system (s04).
//!
//! The loop keeps extension logic out of its body and fires callbacks at four fixed points:
//! - `UserPromptSubmit`: after the user submits input, before the LLM call.
//! - `PreToolCall`: before tool execution (permission checks live here).
//! - `PostToolCall`: after tool execution.
//! - `Stop`: when the loop is about to exit.
//!
//! Return-value semantics:
//! - `PreToolCall` → `Some(reason)` blocks the tool; `reason` becomes the `tool_output`.
//! - `PostToolCall` → `Some(msg)` is injected by the loop as a **separate user message** (does not overwrite `tool_output`).
//! - `Stop` → `Some(msg)` injects msg and **continues** the loop instead of exiting.
//! - `UserPromptSubmit` return value does not affect control flow.
//!
//! Callbacks are trait objects (`Box<dyn TraitX>`, each trait `Send + Sync`): one trait per event,
//! hooks are structs registered by boxing. One extra heap alloc buys hooks that carry owned state
//! (e.g. `TodoReminderHook`'s counter, no longer relying on a static global), and `Send + Sync` lets
//! `Box<dyn>` cross async boundaries. The loop only calls `trigger_*`; all logic lives in callbacks —
//! the core point of s04. See `docs/modules/hooks.md`.

use async_trait::async_trait;
use std::sync::Arc;

use crate::domain::message::{ContentBlock, Message};
use crate::tools::registry::ToolRegistry;

/// Hook runtime context: the I/O abstraction `pre_tool` needs (output + input merged).
pub struct HookContext {
    /// I/O combo (output + input)
    pub io: Arc<crate::io::IO>,
}

impl HookContext {
    /// Build context from an existing I/O combo (Arc clone is the caller's job).
    pub fn new(io: Arc<crate::io::IO>) -> Self {
        Self { io }
    }

    /// Test-only: context backed by in-memory I/O.
    #[cfg(test)]
    pub(crate) fn test_noop() -> Self {
        Self::new(Arc::new(crate::io::IO::memory()))
    }
}

// ---- callback traits ----
#[async_trait]
pub trait PromptHook: Send + Sync {
    async fn on_prompt(&self, query: &str);
}
#[async_trait]
pub trait PreToolHook: Send + Sync {
    async fn on_pre_tool(
        &self,
        registry: &ToolRegistry,
        ctx: &HookContext,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<String>;
}
#[async_trait]
pub trait PostToolHook: Send + Sync {
    async fn on_post_tool(&self, name: &str, input: &serde_json::Value, output: &str) -> Option<String>;
}
#[async_trait]
pub trait StopHook: Send + Sync {
    async fn on_stop(&self, messages: &[Message]) -> Option<String>;
}

/// Hook registry: event → callback list.
#[derive(Default)]
pub struct Hooks {
    user_prompt: Vec<Box<dyn PromptHook>>,
    pre_tool: Vec<Box<dyn PreToolHook>>,
    post_tool: Vec<Box<dyn PostToolHook>>,
    stop: Vec<Box<dyn StopHook>>,
}

impl Hooks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_prompt<H: PromptHook + 'static>(&mut self, h: H) {
        self.user_prompt.push(Box::new(h));
    }
    pub fn on_pre_tool<H: PreToolHook + 'static>(&mut self, h: H) {
        self.pre_tool.push(Box::new(h));
    }
    pub fn on_post_tool<H: PostToolHook + 'static>(&mut self, h: H) {
        self.post_tool.push(Box::new(h));
    }
    pub fn on_stop<H: StopHook + 'static>(&mut self, h: H) {
        self.stop.push(Box::new(h));
    }

    /// Fires after user input, before the LLM call. Return value does not affect control flow.
    pub async fn trigger_prompt(&self, query: &str) {
        for f in &self.user_prompt {
            f.on_prompt(query).await;
        }
    }

    /// Fires before tool execution. First callback returning `Some(reason)` short-circuits → the tool is blocked.
    pub async fn trigger_pre_tool(
        &self,
        registry: &ToolRegistry,
        ctx: &HookContext,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<String> {
        for f in &self.pre_tool {
            if let Some(reason) = f.on_pre_tool(registry, ctx, name, input).await {
                return Some(reason);
            }
        }
        None
    }

    /// Fires after tool execution. `Some(msg)` → caller injects as a separate user message (does not overwrite `tool_output`).
    pub async fn trigger_post_tool(&self, name: &str, input: &serde_json::Value, output: &str) -> Option<String> {
        for f in &self.post_tool {
            if let Some(msg) = f.on_post_tool(name, input, output).await {
                return Some(msg);
            }
        }
        None
    }

    /// Fires when the loop is about to exit. `Some(msg)` → inject msg and continue, do not exit.
    pub async fn trigger_stop(&self, messages: &[Message]) -> Option<String> {
        for f in &self.stop {
            if let Some(msg) = f.on_stop(messages).await {
                return Some(msg);
            }
        }
        None
    }
}

/// Assemble this turn's tool results and PostToolCall reminders into user messages to append.
///
/// `tool_output` is always the real tool output (not overwritten by reminders); if PostToolCall
/// returned a reminder, it is appended as a separate user message — matching how the Stop hook
/// (`agent_loop` / `run_subagent_loop`) injects messages.
pub fn assemble_post_tool_messages(
    tool_outputs: Vec<ContentBlock>,
    reminders: Vec<String>,
) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();

    if !tool_outputs.is_empty() {
        out.push(Message::user_blocks(tool_outputs));
    }

    if !reminders.is_empty() {
        out.push(Message::user_blocks(
            reminders.into_iter().map(ContentBlock::text).collect(),
        ));
    }

    // Fallback: when both are empty (finish_reason reported as tool_call but no ToolCall block in content,
    // and no PostToolCall reminder), still feed back a non-empty user message — otherwise the Anthropic
    // API returns 400 "content cannot be empty".
    if out.is_empty() {
        out.push(Message::user_text("(no tool calls to execute)"));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::Message;

    struct AlwaysBlock;
    #[async_trait::async_trait]
    impl PreToolHook for AlwaysBlock {
        async fn on_pre_tool(&self, _r: &ToolRegistry, _ctx: &HookContext, _n: &str, _i: &serde_json::Value) -> Option<String> {
            Some("nope".to_string())
        }
    }
    struct NeverBlock;
    #[async_trait::async_trait]
    impl PreToolHook for NeverBlock {
        async fn on_pre_tool(&self, _r: &ToolRegistry, _ctx: &HookContext, _n: &str, _i: &serde_json::Value) -> Option<String> {
            None
        }
    }
    struct PanicIfCalled;
    #[async_trait::async_trait]
    impl PreToolHook for PanicIfCalled {
        async fn on_pre_tool(&self, _r: &ToolRegistry, _ctx: &HookContext, _n: &str, _i: &serde_json::Value) -> Option<String> {
            panic!("second hook must not run after a block")
        }
    }

    #[tokio::test]
    async fn empty_registry_allows() {
        let h = Hooks::new();
        let ctx = HookContext::test_noop();
        let registry = ToolRegistry::new();
        assert!(h.trigger_pre_tool(&registry, &ctx, "command", &serde_json::json!({})).await.is_none());
    }

    #[tokio::test]
    async fn pre_tool_first_some_short_circuits() {
        let mut h = Hooks::new();
        h.on_pre_tool(AlwaysBlock);
        h.on_pre_tool(PanicIfCalled); // would panic without short-circuit
        let ctx = HookContext::test_noop();
        let registry = ToolRegistry::new();
        assert_eq!(
            h.trigger_pre_tool(&registry, &ctx, "command", &serde_json::json!({})).await,
            Some("nope".to_string())
        );
    }

    #[tokio::test]
    async fn none_passes_through() {
        let mut h = Hooks::new();
        h.on_pre_tool(NeverBlock);
        h.on_pre_tool(NeverBlock);
        let ctx = HookContext::test_noop();
        let registry = ToolRegistry::new();
        assert!(h.trigger_pre_tool(&registry, &ctx, "command", &serde_json::json!({})).await.is_none());
    }

    #[test]
    fn post_tool_reminder_is_separate_user_message_not_tool_output() {
        let tool_outputs = vec![ContentBlock::tool_output("t1", "real command output")];
        let msgs = assemble_post_tool_messages(
            tool_outputs,
            vec!["<reminder>Update your todos.</reminder>".to_string()],
        );

        // Reminder must be a separate user message, not folded into tool_output
        assert_eq!(
            msgs.len(),
            2,
            "reminder must be a separate user message, not folded into tool_output"
        );

        // tool_output message kept as-is: still the real output
        match &msgs[0].content[0] {
            ContentBlock::ToolOutput { content, .. } => {
                assert_eq!(content, "real command output");
            }
            _ => panic!("first message must still hold the real tool_output"),
        }

        // Reminder is a new user message, a Text block (not a tool_output)
        assert_eq!(msgs[1].role, "user");
        match &msgs[1].content[0] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "<reminder>Update your todos.</reminder>");
            }
            _ => panic!("reminder must be a Text block, not a tool_output"),
        }
    }

    #[test]
    fn no_reminder_yields_single_tool_outputs_message() {
        let tool_outputs = vec![ContentBlock::tool_output("t1", "out")];
        let msgs = assemble_post_tool_messages(tool_outputs, vec![]);
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn empty_results_and_no_reminder_yields_placeholder_message() {
        // C8 regression: when finish_reason is reported as tool_call but no ToolCall block exists, must not
        // produce an empty content message (otherwise API 400 "content cannot be empty").
        let msgs = assemble_post_tool_messages(vec![], vec![]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        assert!(!msgs[0].content.is_empty(), "must not emit empty content");
        match &msgs[0].content[0] {
            ContentBlock::Text { text } => assert!(!text.is_empty()),
            _ => panic!("placeholder must be a Text block"),
        }
    }

    #[test]
    fn empty_results_with_reminder_yields_only_reminder_message() {
        // No tool_output but a reminder exists: must not add an extra empty tool_output message.
        let msgs = assemble_post_tool_messages(
            vec![],
            vec!["<reminder>Update your todos.</reminder>".to_string()],
        );
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        match &msgs[0].content[0] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "<reminder>Update your todos.</reminder>");
            }
            _ => panic!("must be the reminder Text block"),
        }
    }

    #[tokio::test]
    async fn stop_some_forces_continue() {
        struct Force;
        #[async_trait::async_trait]
        impl StopHook for Force {
            async fn on_stop(&self, _m: &[Message]) -> Option<String> {
                Some("keep going".to_string())
            }
        }
        let mut h = Hooks::new();
        h.on_stop(Force);
        assert_eq!(h.trigger_stop(&[]).await, Some("keep going".to_string()));
    }
}
