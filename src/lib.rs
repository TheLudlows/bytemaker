//! bytemaker — a Claude Code-style agent loop in Rust (course s01–s17 reference).
//!
//! Unified `run_loop` (Lead/Subagent/Teammate by `AgentKind`) with hooks, `Tool`/`ToolRegistry`,
//! `Arc`-shared infra + I/O traits; plus compaction, memory recall, persistent tasks, background
//! tasks, cron, team, MCP, workflow. Entry: `main.rs`; core: `agent`; refs: `docs/modules/`, `AGENTS.md`.

pub mod agent;
pub mod background_tasks;
pub mod builtins;
pub mod config;
pub mod domain;
pub mod providers;
pub mod compact;
pub mod cron_scheduler;
pub mod error;
pub mod goal;
pub mod hooks;
pub mod io;
pub mod mcp;
pub mod memory;
pub mod render;
pub mod skills;
pub mod task_system;
pub mod team;
pub mod todo;
pub mod tools;
pub mod workflow;

