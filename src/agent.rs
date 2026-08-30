//! agent.rs — unified agent loop (s01+).
//!
//! `Agent` holds shared infra (mostly `Arc`) plus per-loop state. `new` builds the Lead,
//! `child_agent` spawns an isolated subagent, `child_teammate` spawns a persistent teammate.
//! `run_loop` is shared by all kinds: collect background results → deliver cron tasks →
//! drain inbox → compaction → stream → render → ack cron → run tools. `execute_tool` also
//! enforces the teammate plan-gate. Timing/invariants in `docs/modules/agent.md`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::background_tasks::manager::BackgroundManager;
use crate::background_tasks::BackgroundStopHook;
use crate::builtins;
use crate::domain::message::{ContentBlock, Message};
use crate::providers::openai::OpenAiProvider;
use crate::providers::{CallResult, LlmProvider};
use crate::compact::{ContextCompactor, MAX_REACTIVE_RETRIES};
use crate::cron_scheduler::{self, CronManager};
use crate::error::AgentError;
use crate::goal::{
    OpenAIGoalEvaluator, GoalAction, GoalController, GoalEvaluator,
};
use crate::hooks::{assemble_post_tool_messages, HookContext, Hooks};
use crate::memory::{build_system, MemoryStore};
use crate::mcp::McpManager;
use crate::skills::SkillLoader;
use crate::task_system::store::TaskStore;
use crate::todo::{SharedTodoManager, TodoManager};
use crate::tools;
use crate::tools::registry::ToolRegistry;
use crate::tools::trait_def::{AgentKind, ToolContext, ToolResult};

/// Shared max_tokens for all stream_messages calls.
pub const MAX_TOKENS: u32 = 8000;

/// Subagent system prompt.
const SUB_SYSTEM: &str = "You are a focused coding agent. Complete your task efficiently. Use tools as needed. Return a concise summary of your work.";

/// Loop termination result.
pub enum LoopOutcome {
    /// Model ended (no tool_call and Stop hook did not force continuation).
    Completed,
    /// max_turns limit reached (subagents only).
    MaxTurnsReached,
    /// User cancelled (Ctrl+C).
    Cancelled,
}

/// Configuration for building an Agent.
pub struct AgentConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub workdir: PathBuf,
    pub skills_dir: PathBuf,
    /// I/O abstraction.
    pub io: Arc<crate::io::IO>,
}

pub struct Agent {
    // ---- shared infra: child_agent Arc-clones ----
    pub(crate) client: Arc<dyn LlmProvider>,
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) mcp_manager: Arc<McpManager>,
    pub(crate) skills: Arc<SkillLoader>,
    pub(crate) task_store: Arc<TaskStore>,
    pub(crate) bg_manager: Arc<BackgroundManager>,
    pub(crate) todo_manager: Arc<SharedTodoManager>,
    /// I/O abstraction.
    pub(crate) io: Arc<crate::io::IO>,
    pub(crate) workdir: PathBuf,
    /// s17: Lead-only goal controller (None for subagents/teammates).
    pub(crate) goal: Option<Arc<tokio::sync::Mutex<GoalController>>>,
    /// s17: session-wide token counter for goal status display.
    pub(crate) total_tokens: Arc<AtomicU64>,

    // ---- per-loop state: refreshed by children ----
    pub(crate) cron_manager: Option<Arc<CronManager>>,
    pub(crate) compactor: ContextCompactor,
    pub(crate) memory: MemoryStore,
    pub(crate) hooks: Hooks,
    pub(crate) base_system: String,
    pub(crate) max_turns: usize,
    pub(crate) kind: AgentKind,
    /// s13: this agent's owner name ("agent" for Lead/subagent; teammate name for teammates).
    pub(crate) owner: String,
    /// s13: shared team context (Lead + teammates have Some; s06 subagents have None).
    pub(crate) team: Option<Arc<crate::team::TeamCtx>>,
    pub(crate) max_tokens: u32,
}

impl Agent {
    /// Build the Lead agent. TaskStore/CronManager init errors propagate here.
    pub async fn new(cfg: AgentConfig) -> Result<Agent, AgentError> {
        let client: Arc<dyn LlmProvider> = Arc::new(OpenAiProvider::new(
            cfg.api_key.clone(),
            cfg.base_url.clone(),
            cfg.model.clone(),
        ));
        let evaluator: Arc<dyn GoalEvaluator> =
            Arc::new(OpenAIGoalEvaluator::new( Arc::clone(&client)));
        let goal_controller = GoalController::new(evaluator)
            .map_err(|e| AgentError::Other(format!("goal controller init: {e}")))?;
        let skills = Arc::new(SkillLoader::scan(cfg.skills_dir.clone()));
        let task_store = Arc::new(
            TaskStore::new(cfg.workdir.clone())
                .map_err(|e| AgentError::Other(format!("task store init: {e}")))?,
        );
        let bg_manager = Arc::new(BackgroundManager::new(
            cfg.workdir.join(".task_outputs").join("background"),
        ));
        let todo_manager = Arc::new(SharedTodoManager::new(TodoManager::new()));

        let cron_manager = Some({
            let cm = Arc::new(
                CronManager::new(cfg.workdir.clone())
                    .await
                    .map_err(|e| AgentError::Other(format!("cron init: {e}")))?,
            );
            let _ = cm.load_durable().await;
            cm
        });

        let compactor = ContextCompactor::new(
            cfg.workdir.join(".transcripts"),
            cfg.workdir.join(".task_outputs").join("tool-results"),
        );
        let memory = MemoryStore::new(cfg.workdir.join(".memory"));

        let registry = Arc::new(tools::build_registry());

        // s14: init MCP manager and register management tools.
        let mcp_manager = Arc::new(McpManager::new(Arc::clone(&registry)));
        registry.register_dynamic(Arc::new(crate::mcp::ConnectMcpTool::new(Arc::clone(&mcp_manager))));
        registry.register_dynamic(Arc::new(crate::mcp::DisconnectMcpTool::new(Arc::clone(&mcp_manager))));
        registry.register_dynamic(Arc::new(crate::mcp::ListMcpTool::new(Arc::clone(&mcp_manager))));

        let base_system = build_base_system(&skills, &cfg.workdir);
        let hooks = Self::build_hooks(&bg_manager, &todo_manager);
        let team = Arc::new(
            crate::team::TeamCtx::new(cfg.workdir.clone(), Arc::clone(&task_store))
                .map_err(|e| AgentError::Other(format!("team init: {e}")))?,
        );

        Ok(Agent {
            client,
            registry,
            mcp_manager,
            skills,
            task_store,
            bg_manager,
            todo_manager,
            io: cfg.io,
            workdir: cfg.workdir,
            goal: Some(Arc::new(tokio::sync::Mutex::new(goal_controller))),
            total_tokens: Arc::new(AtomicU64::new(0)),
            cron_manager,
            compactor,
            memory,
            hooks,
            base_system,
            max_turns: usize::MAX,
            kind: AgentKind::Lead,
            owner: "agent".to_string(),
            team: Some(team),
            max_tokens: MAX_TOKENS,
        })
    }

    /// Spawn a nested subagent: Arc-clone shared infra, refresh per-loop state.
    /// cron_manager is None (subagents don't deliver cron tasks); compactor uses an
    /// isolated subdir (avoids file races with the Lead); memory is read_only (recall
    /// but no writes); max_turns is finite to bound the loop.
    pub fn child_agent(&self, max_turns: usize, sub_system: &str) -> Agent {
        let subagent_id = format!("subagent_{}", fastrand::u64(..));
        let subagent_dir = self.workdir.join(".subagents").join(&subagent_id);

        let compactor = ContextCompactor::new(
            subagent_dir.join(".transcripts"),
            subagent_dir.join(".task_outputs").join("tool-results"),
        );

        let memory = MemoryStore::new_read_only(self.workdir.join(".memory"));

        Agent {
            client: Arc::clone(&self.client),
            registry: Arc::clone(&self.registry),
            mcp_manager: Arc::clone(&self.mcp_manager),
            skills: Arc::clone(&self.skills),
            task_store: Arc::clone(&self.task_store),
            bg_manager: Arc::clone(&self.bg_manager),
            todo_manager: Arc::clone(&self.todo_manager),
            io: Arc::clone(&self.io),
            workdir: self.workdir.clone(),
            goal: None,
            total_tokens: Arc::clone(&self.total_tokens),
            cron_manager: None,
            compactor,
            memory,
            hooks: Self::build_hooks(&self.bg_manager, &self.todo_manager),
            base_system: sub_system.to_string(),
            max_turns,
            kind: AgentKind::Subagent,
            owner: "agent".to_string(),
            team: None,
            max_tokens: self.max_tokens,
        }
    }

    /// Start the cron scheduler.
    pub async fn start_cron_runtime(&self) -> Result<(), AgentError> {
        if let Some(cron) = &self.cron_manager {
            cron.start_scheduler()
                .await
                .map_err(|e| AgentError::Other(format!("cron start: {e}")))?;
        }
        Ok(())
    }

    /// s14: shut down all MCP server processes (call before process exit).
    pub async fn shutdown_mcp(&self) {
        self.mcp_manager.shutdown_all().await;
    }

    /// Centralized cleanup: InputTask + MCP processes. Call once before process exit;
    /// covers all `run_interactive` exit paths (break / q / Cancelled). The cron scheduler
    /// and background tasks clean up best-effort as the process exits (see their modules).
    /// Team: abort+drain all teammate runtimes before tearing down shared I/O, so an
    /// interrupted teammate doesn't race with the io being torn down.
    pub async fn shutdown(&self) {
        if let Some(team) = self.team() {
            crate::team::shutdown_all(team).await;
        }
        self.io.shutdown().await;
        self.shutdown_mcp().await;
    }

    /// Assemble the default hook set.
    /// BackgroundStopHook gets bg_manager via constructor DI (was a global get_manager).
    /// TodoReminderHook gets todo_manager via constructor DI (injects the current todo list).
    fn build_hooks(bg: &Arc<BackgroundManager>, todo: &Arc<SharedTodoManager>) -> Hooks {
        let mut h = Hooks::new();
        h.on_prompt(builtins::ContextInjectHook);
        h.on_pre_tool(builtins::PermissionHook);
        // SummaryHook must be registered before TodoReminderHook: trigger_post_tool
        // short-circuits on the first Some(msg), and TodoReminderHook returns Some every 3
        // turns. SummaryHook always returns None, so placing it first lets every tool call
        // be counted without blocking subsequent reminders.
        let summary = builtins::SummaryHook::new();
        h.on_post_tool(summary.clone());
        h.on_post_tool(builtins::LargeOutputHook);
        h.on_post_tool(builtins::TodoReminderHook::new(Arc::clone(todo)));
        h.on_stop(summary);
        h.on_stop(BackgroundStopHook::new(Arc::clone(bg)));
        h
    }

    /// Produce a persistent teammate: shares infra, kind=Teammate, team=Some, fresh
    /// non-interactive hooks. Does NOT reference the Lead agent, so no TeamCtx → Agent →
    /// TeamCtx Arc cycle. cron_manager is None; compactor uses an isolated subdir; memory
    /// is read_only; max_turns is usize::MAX (teammates have no turn limit).
    pub fn child_teammate(&self, name: &str, system: &str, team: Arc<crate::team::TeamCtx>) -> Agent {
        let teammate_dir = self.workdir.join(".teammates").join(name);

        let compactor = ContextCompactor::new(
            teammate_dir.join(".transcripts"),
            teammate_dir.join(".task_outputs").join("tool-results"),
        );

        let memory = MemoryStore::new_read_only(self.workdir.join(".memory"));

        Agent {
            client: Arc::clone(&self.client),
            registry: Arc::clone(&self.registry),
            mcp_manager: Arc::clone(&self.mcp_manager),
            skills: Arc::clone(&self.skills),
            task_store: Arc::clone(&self.task_store),
            bg_manager: Arc::clone(&self.bg_manager),
            todo_manager: Arc::clone(&self.todo_manager),
            io: Arc::clone(&self.io),
            workdir: self.workdir.clone(),
            goal: None,
            total_tokens: Arc::clone(&self.total_tokens),
            cron_manager: None,
            compactor,
            memory,
            hooks: Self::build_teammate_hooks(),
            base_system: system.to_string(),
            max_turns: usize::MAX,
            kind: AgentKind::Teammate,
            owner: name.to_string(),
            team: Some(team),
            max_tokens: self.max_tokens,
        }
    }

    /// Teammate hook set: non-interactive permission (no stdin) + large-output
    /// reminder. No TodoReminder/Summary — teammates are non-interactive.
    fn build_teammate_hooks() -> Hooks {
        let mut h = Hooks::new();
        h.on_pre_tool(builtins::PermissionHook);
        h.on_post_tool(builtins::LargeOutputHook);
        h
    }

    /// Build an isolated Agent around an injected provider + registry（eval runner
    /// 与 TestAgent 共用的构造法）。无 cron / 无 goal / 无 team；read-only memory
    /// 指向空目录——recall 短路为空、extract/consolidate 返回 0，**不产生任何额外
    /// LLM 调用**（顺序流盒带的前提，docs/5.evals.md §2.2）。
    ///
    /// registry 由调用方注入（eval 的 broken-tool 回归演示需要替换工具）。
    pub(crate) fn isolated(
        workdir: PathBuf,
        client: Arc<dyn LlmProvider>,
        io: Arc<crate::io::IO>,
        hooks: Hooks,
        base_system: String,
        max_turns: usize,
        registry: Arc<ToolRegistry>,
    ) -> Agent {
        let skills = Arc::new(SkillLoader::scan(workdir.join("skills"))); // 空目录 -> 空
        // Tempdir 不在 current_dir 之下，TaskStore::new 的越界校验会拒绝；
        // 直接组装（校验在 dispatch 层仍然生效）。
        let task_store = Arc::new(crate::task_system::store::create_test_store(&workdir));
        let bg_manager = Arc::new(BackgroundManager::new(
            workdir.join(".task_outputs").join("background"),
        ));
        let todo_manager = Arc::new(SharedTodoManager::new(TodoManager::new()));
        let mcp_manager = Arc::new(McpManager::new(Arc::clone(&registry)));
        let compactor = ContextCompactor::new(
            workdir.join(".transcripts"),
            workdir.join(".task_outputs").join("tool-results"),
        );
        let memory = MemoryStore::new_read_only(workdir.join(".memory"));
        Agent {
            client,
            registry,
            mcp_manager,
            skills,
            task_store,
            bg_manager,
            todo_manager,
            io,
            workdir,
            goal: None,
            total_tokens: Arc::new(AtomicU64::new(0)),
            cron_manager: None,
            compactor,
            memory,
            hooks,
            base_system,
            max_turns,
            kind: AgentKind::Lead,
            owner: "agent".to_string(),
            team: None,
            max_tokens: MAX_TOKENS,
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn team(&self) -> Option<&Arc<crate::team::TeamCtx>> {
        self.team.as_ref()
    }
    pub fn lead_notify(&self) -> Option<&tokio::sync::Notify> {
        self.team.as_ref().map(|t| t.lead_notify())
    }

    /// s17: human-readable goal status (for `/goal`).
    pub async fn goal_status(&self) -> String {
        match &self.goal {
            Some(gc) => gc
                .lock()
                .await
                .status(self.total_tokens.load(Ordering::Relaxed)),
            None => "No goal set".to_string(),
        }
    }

    /// s17: set a goal (records tokens_at_start). Does not run the loop.
    pub async fn goal_set(&self, condition: &str) -> Result<(), AgentError> {
        let Some(gc) = &self.goal else {
            return Err(AgentError::Other("goal not available for this agent".into()));
        };
        let tokens = self.total_tokens.load(Ordering::Relaxed);
        gc.lock()
            .await
            .set_goal(condition, tokens)
            .map_err(|e| AgentError::Other(format!("{e}")))?;
        Ok(())
    }

    /// s17: clear the active goal. Returns a user-facing message.
    pub async fn goal_clear(&self) -> String {
        match &self.goal {
            Some(gc) => gc.lock().await.clear("cleared"),
            None => "No goal set".to_string(),
        }
    }

    /// s17: reset the block counter at the start of a user turn.
    pub async fn begin_goal_query(&self) {
        if let Some(gc) = &self.goal {
            gc.lock().await.begin_query();
        }
    }

    /// s17: drain the terminal decision recorded this turn (for `[goal]` report).
    pub async fn goal_take_terminal(&self) -> Option<(GoalAction, String)> {
        let gc = self.goal.as_ref()?;
        gc.lock().await.take_last_terminal()
    }

    /// Trigger UserPromptSubmit hook after user input is submitted.
    pub async fn trigger_prompt(&self, query: &str) {
        self.hooks.trigger_prompt(query).await;
    }

    /// Current working directory (used by main's banner).
    pub fn workdir(&self) -> &PathBuf {
        &self.workdir
    }

    pub fn base_system(&self) -> &str {
        &self.base_system
    }

    /// Number of loaded skills (for main's startup banner).
    pub fn skills_len(&self) -> usize {
        self.skills.len()
    }

    // ---- internal tool execution (shared by Lead and subagents) ----

    /// Single tool call + PreToolCall intercept. Subagents go through here too, so
    /// trigger_pre_tool is no longer bypassed.
    async fn execute_tool(&self, name: &str, input: &serde_json::Value) -> ToolResult {
        let ctx = HookContext::new(Arc::clone(&self.io));
        if let Some(reason) = self.hooks.trigger_pre_tool(&self.registry, &ctx, name, input).await {
            return ToolResult::Denied {
                name: name.to_string(),
                reason,
            };
        }
        // s13 plan gate: teammates cannot run mutating tools until the plan is approved.
        if self.kind == AgentKind::Teammate
            && matches!(name, "command" | "write_file" | "edit_file")
        {
            if let Some(team) = &self.team {
                let gate = team.protocols.gate(&self.owner);
                if gate.blocks_mutating_tools() {
                    return ToolResult::Denied {
                        name: name.to_string(),
                        reason: format!(
                            "Blocked: plan status is {:?}. Submit or revise the plan and wait for approval.",
                            gate
                        ),
                    };
                }
            }
        }
        let ctx = ToolContext { agent: self };
        self.registry.dispatch(name, &ctx, input, self.kind).await
    }

    /// Execute all ToolCall blocks this turn; returns user messages to append (does not
    /// mutate `messages` in place, avoiding the `&self` vs `&mut messages` borrow conflict).
    /// Binds `as_content()` once.
    async fn execute_tool_calls(&self, content: &[ContentBlock]) -> Vec<Message> {
        let mut tool_outputs = Vec::new();
        let mut reminders: Vec<String> = Vec::new();
        for block in content {
            if let ContentBlock::ToolCall { id, name, input } = block {
                let result = self.execute_tool(name, input).await;
                let content_str = result.as_content();
                {
                    self.io.output.render_tool_output(name, &content_str, crate::render::colors_enabled());
                }
                if result.was_executed() {
                    if let Some(msg) = self.hooks.trigger_post_tool(name, input, &content_str).await {
                        reminders.push(msg);
                    }
                }
                tool_outputs.push(ContentBlock::tool_output(id.clone(), content_str));
            }
        }
        assemble_post_tool_messages(tool_outputs, reminders)
    }

    /// Unified loop. cron_manager stays Option (Lead-only); compactor/memory/max_turns
    /// always have values. recalled and index are loaded once outside the loop to avoid
    /// re-querying the LLM each turn.
    pub async fn run_loop(
        &self,
        messages: &mut Vec<Message>,
        active_request: &str,
        recalled: &str,
        index: &str,
    ) -> Result<LoopOutcome, AgentError> {
        let mut reactive_retries = 0u32;
        let max = self.max_turns;

        for _turn in 1..=max {
            tracing::info!("[req] messages detail:\n{}", format_messages_for_log(messages));

            // Top of loop: passively collect finished background tasks (subagents don't, matching current behavior).
            if self.kind == AgentKind::Lead {
                let _ = self.bg_manager.collect_and_inject(messages);
            }

            // s12: collect cron tasks to deliver at top of loop (subagents skip, cron_manager=None).
            let mut waiting_for_ack: Vec<cron_scheduler::CronJob> = Vec::new();
            let scheduled_start = messages.len();
            if let Some(cron) = &self.cron_manager {
                let jobs = cron.consume_queue();
                for job in &jobs {
                    messages.push(Message::user_text(format!("[Scheduled] {}", job.prompt)));
                    let preview: String = job.prompt.chars().take(60).collect();
                    println!("  [cron] delivered {}: {}", job.id, preview);
                }
                waiting_for_ack = jobs;
            }

            // s13: teammates drain their own inbox each turn (Lead's inbox is
            // drained by main.rs outside run_loop). An accepted shutdown ends the loop.
            if self.kind == AgentKind::Teammate {
                if let Some(team) = &self.team {
                    if crate::team::drain_inbox(team, &self.owner, messages) {
                        return Ok(LoopOutcome::Completed);
                    }
                }
            }

            // s08: run compaction pipeline before each model call.
            self.compactor.prepare(self.client.as_ref(), messages, active_request).await?;

            // Rebuild the system prompt each turn: current time, MCP status, memory recall
            // (recall and index are loaded once by the caller).
            let current_time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let mcp_suffix = self.mcp_manager.system_prompt_suffix();

            // Assemble base + MCP + time + memory.
            let base_with_time_and_mcp = if mcp_suffix.is_empty() {
                format!("{}\n\nCurrent time: {}", self.base_system, current_time)
            } else {
                format!(
                    "{}\n\nCurrent time: {}\n\n{}",
                    self.base_system, current_time, mcp_suffix
                )
            };
            let system = build_system(&base_with_time_and_mcp, &index, &recalled);

            let defs = self.registry.definitions_for(self.kind);

            let cancel = tokio_util::sync::CancellationToken::new();

            // During streaming, listen for Ctrl+C and cancel this turn. The input thread
            // is blocked in blocking_recv (terminal cooked mode), so reedline can't catch
            // Ctrl+C — use tokio::signal::ctrl_c() as the interrupt source.
            //
            // Note: `_join` is a plain JoinHandle; tokio detaches (not aborts) on Drop, so
            // the per-turn ctrl_c listener is not reclaimed and accumulates across turns
            // (potential leak). InterruptGuard (Drop calls abort) is test-only; full
            // reclamation is an outstanding audit item.
            let _join = spawn_interrupt_listener(cancel.clone());

            // Client returns after collecting all SSE events; render here (no incremental streaming prints).
            let response = match self
                .client
                .stream_messages(&system, messages, &defs, self.max_tokens, cancel)
                .await
            {
                CallResult::Success(r) => {
                    reactive_retries = 0;
                    r
                }
                // prompt_too_long with retry budget left: compact and retry.
                CallResult::PromptTooLong(_) if reactive_retries < MAX_REACTIVE_RETRIES => {
                    self.io.output.status("[reactive compact]");
                    self.compactor
                        .reactive_compact(self.client.as_ref(), messages, active_request)
                        .await?;
                    reactive_retries += 1;
                    continue;
                }
                // prompt_too_long out of retries / no compactor / other error: restore cron tasks then return.
                CallResult::PromptTooLong(e) | CallResult::Failure(e) => {
                    if !waiting_for_ack.is_empty() {
                        messages.truncate(scheduled_start);
                        if let Some(cron) = &self.cron_manager {
                            cron.restore_jobs(&waiting_for_ack);
                        }
                    }
                    return Err(e.into());
                }
                CallResult::Cancelled => {
                    if !waiting_for_ack.is_empty() {
                        messages.truncate(scheduled_start);
                        if let Some(cron) = &self.cron_manager {
                            cron.restore_jobs(&waiting_for_ack);
                        }
                    }
                    return Ok(LoopOutcome::Cancelled);
                }
            };

            // After client returns, render this turn's content uniformly (text emitted line by line; tool_call shows ⚙ + JSON).
            for block in &response.content {
                match block {
                    ContentBlock::Text { text } => {
                        self.io.output.emit(text);
                    }
                    ContentBlock::ToolCall { name, input, .. } => {
                        self.io.output.emit(&format!("⚙ {}", name));
                        if *input != serde_json::Value::Null {
                            let input_str = serde_json::to_string_pretty(input).unwrap_or_default();
                            for line in input_str.lines() {
                                self.io.output.emit(&format!("  {}", line));
                            }
                        }
                    }
                    _ => {}
                }
            }

            // Append assistant response (text and tool_call blocks, passed as-is to next turn).
            messages.push(Message::assistant_content(response.content.clone()));

            // s17: accumulate real token usage for goal status display.
            if let Some(u) = &response.usage {
                self.total_tokens
                    .fetch_add(u.input_tokens + u.output_tokens, Ordering::Relaxed);
            }

            // Acknowledge cron tasks after a successful model call.
            if !waiting_for_ack.is_empty() {
                if let Some(cron) = &self.cron_manager {
                    if let Err(e) = cron.acknowledge_jobs(&waiting_for_ack).await {
                        println!("  [cron] acknowledgement failed: {}", e);
                    }
                }
                waiting_for_ack.clear();
            }

            // Check whether to call tools.
            if response.finish_reason != "tool_call" {
                if let Some(force) = self.hooks.trigger_stop(messages).await {
                    messages.push(Message::user_text(force));
                    continue;
                }
                // s17: Goal Stop judgment (Lead only; children have goal=None).
                // Runs after trigger_stop so fresh background results are
                // already injected into messages before the evaluator sees them.
                if let Some(gc) = &self.goal {
                    let decision = gc
                        .lock()
                        .await
                        .evaluate_after_turn(messages, self.bg_manager.has_running())
                        .await;
                    if decision.action == GoalAction::Block {
                        let condition = decision.condition.unwrap_or_default();
                        messages.push(Message::user_text(format!(
                            "[Goal still active]\nCondition: {condition}\n\
                             Evaluator: {}\n\
                             Continue working and surface the missing evidence.",
                            decision.reason
                        )));
                        continue;
                    }
                }
                // read_only instances return 0 from extract/consolidate internally, no extra check needed.
                if self.memory.extract_memories(self.client.as_ref(), messages).await > 0 {
                    let _ = self.memory.consolidate_memories(self.client.as_ref()).await;
                }
                return Ok(LoopOutcome::Completed);
            }

            // Execute this turn's tool calls + PostToolCall reminders (shared helper).
            messages.extend(self.execute_tool_calls(&response.content).await);
        }

        Ok(LoopOutcome::MaxTurnsReached)
    }
    pub async fn run_interactive(&self) -> Result<(), AgentError> {
        let mut messages: Vec<Message> = Vec::new();

        loop {
            // Ask InputTask to read a line (reedline renders the ` >> ` prompt itself).
            let line = match self.io.read_line().await {
                Some(l) => l,
                _ => break, // EOF / Ctrl+C: InputTask has exited
            };
            let query = line.trim().to_string();
            if query.is_empty() {
                continue; // Empty line: resend ReadLine
            }
            if query.eq_ignore_ascii_case("q") || query == "exit" {
                return Ok(());
            }
            // Defer wake: drain the Lead inbox at the start of each user turn.
            // run_team_wake calls consume_lead_inbox internally; an empty inbox is a no-op
            // returning false. Don't pre-consume outside, or it would double-drain.
            if self.run_team_wake(&mut messages).await {
                return Ok(());
            }
            if self.run_user_turn(&mut messages, &query).await {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Run one user turn. Returns true to exit (Cancelled); false to continue.
    async fn run_user_turn(&self, messages: &mut Vec<Message>, query: &str) -> bool {
        let trimmed = query.trim();

        // s17: /goal command dispatch (bytemaker has no other slash command routing).
        if trimmed == "/goal" {
            self.io.output.banner(&self.goal_status().await);
            return false;
        }
        if let Some(arg) = trimmed.strip_prefix("/goal ") {
            let arg = arg.trim();
            let lower = arg.to_ascii_lowercase();
            if crate::goal::CLEAR_ALIASES.contains(&lower.as_str()) {
                self.io.output.banner(&self.goal_clear().await);
                return false;
            }
            if let Err(e) = self.goal_set(arg).await {
                self.io.output.error(&format!("Error: {}", e));
                return false;
            }
            // Set: seed the turn with the condition as the user message.
            self.trigger_prompt(arg).await;
            messages.push(Message::user_text(arg.to_string()));
            return self.run_loop_and_report(messages, arg).await;
        }

        // Default path
        self.trigger_prompt(trimmed).await;
        messages.push(Message::user_text(trimmed.to_string()));
        self.run_loop_and_report(messages, trimmed).await
    }

    /// Run run_loop, then print a [goal] terminal report. Returns true to exit.
    /// The report prints only when the evaluator produces a terminal decision
    /// (achieved/failed/error/limit/defer); no goal or a normal Allow exit prints nothing
    /// (preserves s01 behavior). Loads memory once outside the loop.
    async fn run_loop_and_report(
        &self,
        messages: &mut Vec<Message>,
        active_request: &str,
    ) -> bool {
        self.begin_goal_query().await;

        // Load memory once before the loop.
        let recalled = self.memory.load_memories(self.client.as_ref(), messages).await;
        let index = self.memory.read_memory_index();

        match self.run_loop(messages, active_request, &recalled, &index).await {
            Ok(LoopOutcome::Cancelled) => true,
            Ok(_) => {
                if let Some((action, reason)) = self.goal_take_terminal().await {
                    self.io.output.banner(&format!("[goal] {:?}: {}", action, reason));
                }
                self.io.output.blank();
                false
            }
            Err(e) => {
                self.io.output.error(&format!("Error: {}", e));
                false
            }
        }
    }

    /// Handle a team wake: drain typed events from the Lead inbox and feed them back
    /// as a new turn. Returns true to exit (Cancelled); false to continue.
    /// Loads memory once outside the loop.
    async fn run_team_wake(&self, messages: &mut Vec<Message>) -> bool {
        let inbox = crate::team::consume_lead_inbox(self.team().expect("team initialized"));
        if inbox.is_empty() {
            return false;
        }
        let text = crate::team::format_team_events(&inbox);
        messages.push(Message::user_text(text));
        self.io.output.banner(&format!(
            "[wake: {} team event(s) -> new turn]",
            inbox.len()
        ));

        // Load memory once before the loop.
        let recalled = self.memory.load_memories(self.client.as_ref(), messages).await;
        let index = self.memory.read_memory_index();

        match self.run_loop(messages, "[team events]", &recalled, &index).await {
            Ok(LoopOutcome::Cancelled) => true,
            Ok(_) => false,
            Err(e) => {
                self.io.output.error(&format!("Error: {}", e));
                false
            }
        }
    }

    /// Subagent entry. Spawns a child agent (shared infra, refreshed state), runs
    /// run_loop, and extracts the final text. Loads memory once outside the loop.
    pub async fn run_subagent(&self, prompt: &str, max_turns: usize) -> Result<String, AgentError> {
        let max_turns = max_turns.clamp(1, 50);
        let child = self.child_agent(max_turns, SUB_SYSTEM);
        self.io.output.status("[Subagent started]");

        let mut messages: Vec<Message> = vec![Message::user_text(prompt)];

        // Load memory once before the loop.
        let recalled = child.memory.load_memories(child.client.as_ref(), &messages).await;
        let index = child.memory.read_memory_index();

        let outcome = child.run_loop(&mut messages, prompt, &recalled, &index).await?;

        let result = match outcome {
            LoopOutcome::Completed => {
                let last_assistant = messages
                    .iter()
                    .rev()
                    .find(|m| m.role.as_str() == "assistant")
                    .map(|m| m.content.as_slice())
                    .unwrap_or(&[]);
                match extract_final_text(last_assistant) {
                    Some(text) => {
                        self.io.output.status("[Subagent done]");
                        text
                    }
                    None => {
                        self.io.output.status("[Subagent done - no text]");
                        "(no summary)".to_string()
                    }
                }
            }
            LoopOutcome::MaxTurnsReached => {
                self.io.output.status(&format!(
                    "[Subagent stopped after {} turns without final answer]",
                    max_turns
                ));
                format!(
                    "Subagent stopped after {} turns without a final answer.",
                    max_turns
                )
            }
            LoopOutcome::Cancelled => {
                self.io.output.status("[Subagent cancelled]");
                "(cancelled)".to_string()
            }
        };
        Ok(result)
    }
}

/// Extract final text from a response (no tool_call). Returns None if there are no Text blocks.
fn extract_final_text(content: &[ContentBlock]) -> Option<String> {
    let texts: Vec<String> = content
        .iter()
        .filter_map(|block| {
            if let ContentBlock::Text { text } = block {
                Some(text.clone())
            } else {
                None
            }
        })
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

/// 从消息列表提取最后一条 assistant 消息的文本（eval runner 取最终回答用）。
pub(crate) fn extract_final_text_from(messages: &[Message]) -> Option<String> {
    let last_assistant = messages
        .iter()
        .rev()
        .find(|m| m.role.as_str() == "assistant")
        .map(|m| m.content.as_slice())
        .unwrap_or(&[]);
    extract_final_text(last_assistant)
}

/// Spawn a listener task that calls `cancel.cancel()` when `ctrl_c()` completes.
///
/// During streaming the input thread is blocked in `blocking_recv` (terminal cooked
/// mode), so reedline can't catch Ctrl+C — use `tokio::signal::ctrl_c()` as the
/// interrupt source to cancel this turn.
///
/// Returns a plain `JoinHandle`; tokio detaches (not aborts) on Drop, so the listener
/// lives until process exit unless explicitly aborted. `InterruptGuard` (Drop calls
/// `abort()`) is test-only; production (`let _join = ...` in run_loop) doesn't wire it,
/// so listeners accumulate across turns (known outstanding audit item).
///
/// Note: this hardcodes `ctrl_c()` with no injectable interrupt parameter — the test
/// `interrupt_listener_cancels_token_when_interrupt_fires` expects a oneshot driver it
/// can't plug in, which is why that test fails.
fn spawn_interrupt_listener(
    cancel: tokio_util::sync::CancellationToken
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel.cancel();
    })
}

/// Drop calls `abort()`, so no matter which path this turn exits (Success / `continue`
/// retry / `return Err` / `return Ok(Cancelled)`) the ctrl_c listener is torn down and
/// doesn't leak across turns.
struct InterruptGuard(tokio::task::JoinHandle<()>);
impl Drop for InterruptGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Assemble the system prompt.
fn build_base_system(skills: &SkillLoader, workdir: &std::path::Path) -> String {
    let catalog = skills.catalog();
    let os = std::env::consts::OS;
    let base = if catalog.is_empty() {
        format!(
            "You are a coding agent at {} on {}. Before starting any multi-step task, use todo_write to plan your steps. Update status as you go. You can use tools as needed.",
            workdir.display(),
            os
        )
    } else {
        format!(
            "You are a coding agent at {} on {}. Before starting any multi-step task, use todo_write to plan your steps. Update status as you go. You can use tools as needed.\n\n\
             Skills available:\n{}\n\n\
             Use load_skill to read the full instructions when a skill applies.",
            workdir.display(),
            os,
            catalog
        )
    };
    // s17: ask the worker to surface verification results so the independent
    // evaluator can judge completion from the transcript.
    format!(
        "{base}\n\nWhen you run a verification command, write the command and its \
         result into the conversation so an independent evaluator can check \
         completion."
    )
}

/// Test-only: build an isolated Agent inside a tempdir (no cron/compactor/memory),
/// replacing the old `TestToolContext`. Global singletons are gone, so multiple
/// non-polluting instances can be built in parallel in one process.
#[cfg(test)]
pub struct TestAgent {
    // Keep tempdir alive to isolate task/bg/skills files.
    _tmp: tempfile::TempDir,
    agent: Agent,
}

#[cfg(test)]
impl TestAgent {
    pub fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let workdir = tmp.path().to_path_buf();
        let client: Arc<dyn LlmProvider> = Arc::new(OpenAiProvider::new(
            "test-key".into(),
            "http://localhost".into(),
            "test-model".into(),
        ));
        // TaskStore::new compares the passed dir against current_dir to block out-of-bounds
        // access; tempdir isn't in the workspace, so use create_test_store (cfg(test) gate
        // lifted in ch5 — Agent::isolated stages eval agents in temp workspaces the same way)
        // to assemble directly (bypassing validation).
        let task_store = Arc::new(crate::task_system::store::create_test_store(&workdir));
        let bg_manager = Arc::new(BackgroundManager::new(
            workdir.join(".task_outputs").join("background"),
        ));
        let todo_manager = Arc::new(SharedTodoManager::new(TodoManager::new()));
        let registry = Arc::new(tools::build_registry());
        // In-memory I/O for tests.
        let io = Arc::new(crate::io::IO::memory());

        let mut agent = Agent::isolated(
            workdir.clone(),
            client,
            io,
            Agent::build_hooks(&bg_manager, &todo_manager),
            "test system".into(),
            usize::MAX,
            registry,
        );
        // isolated 内部会新建 bg/todo/task_store 实例；重新指回上面构造的实例，
        // 保证 hooks（BackgroundStopHook / TodoReminderHook）、TeamCtx 与 agent 字段
        // 共享同一 Arc —— 与重构前行为完全一致。
        agent.task_store = Arc::clone(&task_store);
        agent.bg_manager = Arc::clone(&bg_manager);
        agent.todo_manager = Arc::clone(&todo_manager);
        agent.team = Some(Arc::new(
            crate::team::TeamCtx::new(workdir.clone(), Arc::clone(&task_store)).unwrap(),
        ));
        Self { _tmp: tmp, agent }
    }

    pub fn context(&self) -> ToolContext<'_> {
        ToolContext { agent: &self.agent }
    }

    pub fn agent(&self) -> &Agent {
        &self.agent
    }
}

/// Format the request message list into a readable multi-line string for the `[req]` log.
/// Over-long text / tool_output / tool_call.input are previewed (truncated by chars, UTF-8
/// safe) so a single huge tool result doesn't flood the log.
pub fn format_messages_for_log(messages: &[Message]) -> String {
    const PREVIEW: usize = 100;
    fn preview(s: &str) -> String {
        let n = s.chars().count();
        if n <= PREVIEW {
            return s.to_string();
        }
        let head: String = s.chars().take(PREVIEW).collect();
        format!("{}… (+{} chars)", head, n - PREVIEW)
    }
    let mut out = String::new();
    for (i, m) in messages.iter().enumerate() {
        out.push_str(&format!(
            " [{}] role={} blocks={}\n",
            i,
            m.role,
            m.content.len()
        ));
        for b in &m.content {
            match b {
                ContentBlock::Text { text } => {
                    out.push_str(&format!(" text: {}\n", preview(text)));
                }
                ContentBlock::ToolCall { id, name, input } => {
                    out.push_str(&format!(
                        " tool_call({}: {}): {}\n",
                        id,
                        name,
                        preview(&input.to_string())
                    ));
                }
                ContentBlock::ToolOutput { call_id, content } => {
                    out.push_str(&format!(
                        "   tool_output({}: {} chars): {}\n",
                        call_id,
                        content.chars().count(),
                        preview(content)
                    ));
                }
            }
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
impl Default for TestAgent {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::trait_def::ToolResult;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn test_agent_constructs_isolated() {
        // Global singletons removed: multiple non-polluting Agents can be built in one
        // process (old OnceLock was not parallel-safe).
        let a = TestAgent::new();
        assert!(a.agent().kind == AgentKind::Lead);
        assert_eq!(a.agent().max_turns, usize::MAX);
        assert!(a.agent().cron_manager.is_none()); // TestAgent skips cron
        let _b = TestAgent::new(); // a second one, non-polluting
    }

    #[test]
    fn child_agent_shares_infra_and_scopes_per_loop_state() {
        let a = TestAgent::new();
        let child = a.agent().child_agent(30, "sub");
        // Shared infra: Arc pointers equal.
        assert!(Arc::ptr_eq(&a.agent().client, &child.client));
        assert!(Arc::ptr_eq(&a.agent().registry, &child.registry));
        assert!(Arc::ptr_eq(&a.agent().task_store, &child.task_store));
        assert!(Arc::ptr_eq(&a.agent().bg_manager, &child.bg_manager));
        // per-loop state refreshed.
        assert!(child.kind == AgentKind::Subagent);
        assert_eq!(child.max_turns, 30);
        assert!(child.cron_manager.is_none()); // subagents don't deliver cron tasks
        // compactor/memory always have values, but subagents use isolated dirs and read_only mode.
        assert_eq!(child.base_system, "sub");
    }

    #[test]
    // Note: isolated agents also get a read-only MemoryStore, but MemoryStore exposes
    // no mode accessor to assert on (read-only behavior is covered in memory tests).
    fn isolated_agent_has_no_cron_goal_or_team() {
        let tmp = tempfile::TempDir::new().unwrap();
        let client: Arc<dyn LlmProvider> = Arc::new(crate::providers::MockProvider::new("x"));
        let io = Arc::new(crate::io::IO::memory());
        let agent = Agent::isolated(
            tmp.path().to_path_buf(),
            client,
            io,
            Agent::build_hooks(
                &Arc::new(BackgroundManager::new(tmp.path().join("bg"))),
                &Arc::new(SharedTodoManager::new(TodoManager::new())),
            ),
            "isolated system".into(),
            5,
            Arc::new(tools::build_registry()),
        );
        assert_eq!(agent.kind, AgentKind::Lead);
        assert_eq!(agent.max_turns, 5);
        assert!(agent.cron_manager.is_none());
        assert!(agent.goal.is_none());
        assert!(agent.team.is_none());
        assert_eq!(agent.base_system, "isolated system");
    }

    #[test]
    fn extract_final_text_from_finds_last_assistant_text() {
        let messages = vec![
            Message::user_text("hi"),
            Message::assistant_content(vec![ContentBlock::text("first")]),
            Message::assistant_content(vec![ContentBlock::text("final answer")]),
        ];
        assert_eq!(extract_final_text_from(&messages).as_deref(), Some("final answer"));
        assert_eq!(extract_final_text_from(&[]), None);
    }

    #[tokio::test]
    async fn subagent_execute_tool_runs_pre_tool_denies_destructive() {
        // S2 regression: a child agent's execute_tool must go through trigger_pre_tool.
        // Old subagent.rs called registry.dispatch(for_subagent=true) directly, bypassing pre_tool.
        let a = TestAgent::new();
        let child = a.agent().child_agent(30, "sub");
        let result = child.execute_tool("command", &json!({"command": "rm -rf /"})).await;
        assert!(
            matches!(result, ToolResult::Denied { .. }),
            "destructive command must be denied via pre_tool, got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn execute_tool_plan_gate_blocks_command_for_teammate() {
        // s13: a teammate with gate=Pending cannot run command; the gate sits
        // after pre_tool, so a non-destructive command is blocked by the gate.
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::task_system::store::create_test_store(tmp.path()));
        let team = Arc::new(crate::team::TeamCtx::new(tmp.path().to_path_buf(), store).unwrap());
        team.protocols
            .set_gate("alice", crate::team::protocols::GateStatus::Pending);
        let a = TestAgent::new();
        let child = a.agent().child_teammate("alice", "sub", Arc::clone(&team));
        let r = child
            .execute_tool("command", &serde_json::json!({"command": "ls"}))
            .await;
        assert!(
            matches!(r, ToolResult::Denied { .. }),
            "teammate command must be gated, got {:?}",
            r
        );
    }

    #[tokio::test]
    async fn execute_tool_allows_command_when_approved() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::task_system::store::create_test_store(tmp.path()));
        let team = Arc::new(crate::team::TeamCtx::new(tmp.path().to_path_buf(), store).unwrap());
        team.protocols
            .set_gate("alice", crate::team::protocols::GateStatus::Approved);
        let a = TestAgent::new();
        let child = a.agent().child_teammate("alice", "sub", Arc::clone(&team));
        // child has no assignment -> ctx.cwd() would error for a teammate, but
        // the gate passes; command runs (via workdir()) and is not Denied.
        let r = child
            .execute_tool("command", &serde_json::json!({"command": "echo hi"}))
            .await;
        assert!(
            !matches!(r, ToolResult::Denied { .. }) || r.as_content().contains("Claim a Task"),
            "should not be gated when approved, got {:?}",
            r
        );
    }

    #[test]
    fn loop_outcome_cancelled_variant_exists() {
        assert!(matches!(LoopOutcome::Cancelled, LoopOutcome::Cancelled));
    }

    /// After the interrupt source completes, the listener task must call `cancel.cancel()`.
    /// Uses a oneshot-driven future in place of a real OS signal to verify the wiring.
    #[tokio::test]
    async fn interrupt_listener_cancels_token_when_interrupt_fires() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let _guard = InterruptGuard(spawn_interrupt_listener(cancel.clone()));
        assert!(!cancel.is_cancelled(), "precondition: not yet cancelled");
        tx.send(()).expect("send interrupt");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !cancel.is_cancelled() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("interrupt fired but token not cancelled (P0-2)");
    }
}
