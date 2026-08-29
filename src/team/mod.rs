//! team — multi-agent collaboration (s13).
//!
//! Shared `TeamCtx` (one Arc): bus, assignments, protocols/plan-gate, active-teammate
//! registry, Lead-wake Notify, task store, workdir, cross-process TaskStoreLock, and
//! spawned teammate JoinHandle table (abort+drain on Lead shutdown). Task state machine
//! (`claim_task`/`complete_task` take `team.lock` first), teammate lifecycle, inbox
//! drain, protocol application, and Lead inbox delivery live here. Details:
//! `docs/modules/team.md`.
//!
//! Known boundaries: lock order is `team.lock` (outermost) then protocol `pending` →
//! `plan_request_ids`; `new_request_id` is called outside the `pending` lock (std Mutex
//! is non-reentrant, see `tools.rs::submit_plan`). Mutating-tool gate interception lives
//! in `agent.rs::execute_tool`, not here. `bus.rs` `MessageRecord::ts` is always `0.0`.

pub mod lock;
pub mod bus;
pub mod assignment;
pub mod protocols;
pub mod runtime;
pub mod tools;
pub mod worktree;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::domain::message::{ContentBlock, Message, Role};
use crate::task_system::task::{Task, TaskStatus};
use crate::task_system::store::TaskStore;
use crate::team::assignment::{Assignment, AssignmentRegistry};
use crate::team::bus::MessageRecord;
use crate::team::lock::TaskStoreLock;
use crate::team::protocols::{GateStatus, ProtocolRegistry, ProtocolStatus, ProtocolType};
use crate::team::worktree::task_worktree_cwd;

/// Shared team state (one Arc): mailboxes, assignments, protocol state,
/// active-teammate registry, a Notify for waking the Lead REPL, the task store,
/// the repo workdir, the cross-process TaskStoreLock, and the live `JoinHandle`s
/// of spawned teammate runtimes (so the Lead can abort+drain them on shutdown
/// instead of orphaning them).
pub struct TeamCtx {
    pub bus: bus::MessageBus,
    pub assignments: AssignmentRegistry,
    pub protocols: ProtocolRegistry,
    pub active: Mutex<HashMap<String, TeammateStatus>>,
    pub lead_notify: tokio::sync::Notify,
    pub task_store: Arc<TaskStore>,
    pub workdir: PathBuf,
    pub lock: TaskStoreLock,
    pub handles: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeammateStatus {
    Working,
    WaitingApproval,
    Idle,
    Stopping,
}

impl TeamCtx {
    pub fn new(workdir: PathBuf, task_store: Arc<TaskStore>) -> std::io::Result<Self> {
        let tasks_dir = task_store_dir(&task_store);
        Ok(Self {
            bus: crate::team::bus::MessageBus::new(workdir.clone()),
            assignments: AssignmentRegistry::new(),
            protocols: ProtocolRegistry::new(),
            active: Mutex::new(HashMap::new()),
            lead_notify: tokio::sync::Notify::new(),
            task_store,
            workdir,
            lock: TaskStoreLock::new(&tasks_dir)?,
            handles: Mutex::new(HashMap::new()),
        })
    }

    pub fn lead_notify(&self) -> &tokio::sync::Notify {
        &self.lead_notify
    }
}

/// Expose the store's `.tasks` directory so TaskStoreLock can lock `.tasks/.lock`.
fn task_store_dir(store: &TaskStore) -> PathBuf {
    store.directory().to_path_buf()
}

/// Atomically claim one task and bind the owner's cwd. Returns a user-facing string.
pub fn claim_task(team: &TeamCtx, task_id: &str, owner: &str) -> String {
    let _g = match team.lock.lock() {
        Ok(g) => g,
        Err(e) => return format!("Error: {}", e),
    };
    let store = &team.task_store;
    let Ok(mut task) = store.load(task_id) else {
        return format!("Error: Task {} not found", task_id);
    };
    if task.status != TaskStatus::Pending {
        return format!("Task {} is {}, cannot claim", task_id, task.status.as_word());
    }
    if task.owner.is_some() {
        return format!(
            "Task {} is already owned by {}",
            task_id,
            task.owner.as_deref().unwrap_or("?")
        );
    }
    if team.assignments.get(owner).is_some() {
        return format!("Owner {} must finish current work before claiming another task", owner);
    }
    if !incomplete_deps_empty(team, &task) {
        return format!("Blocked by: {:?}", incomplete_deps(team, &task));
    }
    let (cwd, err) = task_worktree_cwd(&team.workdir, &task);
    if let Some(e) = err {
        return format!("Cannot claim {}: {}", task_id, e);
    }
    task.status = TaskStatus::InProgress;
    task.owner = Some(owner.to_string());
    if let Err(e) = store.save(&task) {
        return format!("Error: {}", e);
    }
    team.assignments
        .set(owner, Assignment { task_id: task.id.clone(), cwd });
    team.assignments.advance_version(owner);
    format!("Claimed {} ({})", task.id, task.subject)
}

/// Complete an owned, in-progress task. Respects the plan gate.
pub fn complete_task(team: &TeamCtx, task_id: &str, owner: &str) -> String {
    let _g = match team.lock.lock() {
        Ok(g) => g,
        Err(e) => return format!("Error: {}", e),
    };
    let store = &team.task_store;
    let Ok(mut task) = store.load(task_id) else {
        return format!("Error: Task {} not found", task_id);
    };
    if task.status != TaskStatus::InProgress {
        return format!("Task {} is {}, cannot complete", task_id, task.status.as_word());
    }
    if task.owner.as_deref() != Some(owner) {
        return format!(
            "Task {} is owned by {}, not {}",
            task_id,
            task.owner.as_deref().unwrap_or("none"),
            owner
        );
    }
    if team.protocols.gate(owner).blocks_mutating_tools() {
        return format!("Task {} cannot complete while plan status blocks changes", task_id);
    }
    task.status = TaskStatus::Completed;
    if let Err(e) = store.save(&task) {
        return format!("Error: {}", e);
    }
    format!("Completed {} ({})", task.id, task.subject)
}

fn incomplete_deps(team: &TeamCtx, task: &Task) -> Vec<String> {
    task.blocked_by
        .iter()
        .filter(|d| match team.task_store.load(d) {
            Ok(t) => t.status != TaskStatus::Completed,
            Err(_) => true,
        })
        .cloned()
        .collect()
}

fn incomplete_deps_empty(team: &TeamCtx, task: &Task) -> bool {
    incomplete_deps(team, task).is_empty()
}

// ---- s13 teammate lifecycle helpers ----

/// How long a teammate blocks on its inbox before falling back to a task scan.
pub const IDLE_SCAN_INTERVAL: Duration = Duration::from_secs(2);

pub fn is_reserved_teammate_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "lead" || lower == "agent"
}

pub fn teammate_system_prompt(name: &str, role: &str) -> String {
    format!(
        "You are '{name}', a {role}. Use tools to complete the assigned Task, then call \
         complete_task and report a concise result. If the first user message contains \
         [Assigned task], that Task is already claimed; do not call claim_task for it \
         again. When asked for a plan, call submit_plan and wait for approval before \
         bash or file changes. File and shell tools use the Task's working directory; \
         that directory is not a sandbox. The runtime delivers your final text to Lead. \
         Use send_message only for intermediate coordination, addressing the coordinator \
         as 'lead'."
    )
}

/// Drop a teammate's assignment if its task is completed + still owned by it.
pub fn release_completed_assignment(team: &TeamCtx, owner: &str) -> bool {
    let g = team.assignments.get(owner);
    if g.is_none() {
        return false;
    }
    let a = g.unwrap();
    let task = match team.task_store.load(&a.task_id) {
        Ok(t) => t,
        Err(_) => return false,
    };
    if task.status != TaskStatus::Completed || task.owner.as_deref() != Some(owner) {
        return false;
    }
    team.assignments.remove(owner);
    team.assignments.advance_version(owner);
    team.protocols.set_gate(owner, GateStatus::NotRequired);
    true
}

/// Release a teammate's assignment on shutdown: return any in-progress task it
/// owned to Pending, clear the assignment, and reset the gate.
pub fn release_teammate_assignment(team: &TeamCtx, owner: &str) {
    let _g = match team.lock.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    for t in team.task_store.list().unwrap_or_default() {
        if t.status == TaskStatus::InProgress && t.owner.as_deref() == Some(owner) {
            let mut t = t;
            t.status = TaskStatus::Pending;
            t.owner = None;
            let _ = team.task_store.save(&t);
        }
    }
    team.assignments.remove(owner);
    team.assignments.advance_version(owner);
    team.protocols.set_gate(owner, GateStatus::NotRequired);
}

/// Abort every live teammate runtime handle and briefly await its finish, so a
/// stuck/hung teammate is forcibly stopped on Lead shutdown instead of being orphaned.
/// Aborting drops the task → its `TeammateShutdownGuard` Drop runs → the assignment is
/// released. Called from `Agent::shutdown`.
///
/// Drains the map first so a concurrent spawner can't double-insert; the guard also
/// removes its own entry on Drop, a harmless no-op once drained.
pub async fn shutdown_all(team: &TeamCtx) {
    let handles: Vec<(String, tokio::task::JoinHandle<()>)> = {
        let mut g = team.handles.lock().unwrap_or_else(|e| e.into_inner());
        g.drain().collect()
    };
    for (_name, handle) in handles {
        handle.abort();
        // Await the abort so the task's Drop (guard → assignment release) runs
        // before we return, but don't block shutdown on a task that won't settle.
        let _ = tokio::time::timeout(Duration::from_millis(500), handle).await;
    }
}

/// Pending, unowned, deps-complete tasks whose worktree binding (if any) resolves.
pub fn scan_unclaimed_tasks(team: &TeamCtx) -> Vec<Task> {
    let mut out = Vec::new();
    for t in team.task_store.list().unwrap_or_default() {
        if t.status == TaskStatus::Pending
            && t.owner.is_none()
            && incomplete_deps_empty(team, &t)
        {
            let (_cwd, err) = crate::team::worktree::task_worktree_cwd(&team.workdir, &t);
            if err.is_none() {
                out.push(t);
            }
        }
    }
    out
}

/// Claim the first scannable task for `owner` (no-op if already assigned).
pub fn claim_next_task(team: &TeamCtx, owner: &str) -> Option<Task> {
    if team.assignments.get(owner).is_some() {
        return None;
    }
    for t in scan_unclaimed_tasks(team) {
        let r = claim_task(team, &t.id, owner);
        if r.starts_with("Claimed") {
            return team.task_store.load(&t.id).ok();
        }
    }
    None
}

/// Last assistant text block in the conversation (the teammate's "result").
pub fn extract_last_assistant_text(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .and_then(|m| {
            m.content.iter().rev().find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
}

// ---- s13 run-loop inbox drain + protocol application ----

/// Drain this teammate's inbox into its messages. Returns true if a shutdown
/// was accepted (the caller should end the loop).
pub fn drain_inbox(team: &TeamCtx, owner: &str, messages: &mut Vec<Message>) -> bool {
    let inbox = team.bus.read_inbox(owner);
    if inbox.is_empty() {
        return false;
    }
    let mut work: Vec<String> = Vec::new();
    let mut should_stop = false;
    for msg in inbox {
        match msg.msg_type.as_str() {
            "shutdown_request" => {
                if apply_shutdown_request(team, owner, &msg) {
                    team.bus.send(
                        owner,
                        "lead",
                        "Shutdown acknowledged.",
                        "shutdown_response",
                        Some(serde_json::json!({
                            "request_id": msg.metadata.get("request_id").cloned().unwrap_or(serde_json::Value::Null),
                            "approve": true,
                        })),
                    );
                    should_stop = true;
                } else {
                    work.push("[Ignored shutdown request: request mismatch]".into());
                }
            }
            "plan_approval_response" => {
                work.push(apply_plan_response(team, owner, &msg));
            }
            "plan_request" => {
                work.push(format!("[Plan required] {}", msg.content));
            }
            _ => work.push(format!("[Message from {}] {}", msg.from, msg.content)),
        }
    }
    if !work.is_empty() {
        messages.push(Message::user_text(work.join("\n")));
    }
    should_stop
}

/// Accept a pending shutdown request sent by Lead to this teammate.
fn apply_shutdown_request(team: &TeamCtx, owner: &str, msg: &MessageRecord) -> bool {
    let request_id = match msg.metadata.get("request_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return false,
    };
    let pending = team.protocols.pending.lock().unwrap();
    let Some(state) = pending.get(request_id) else {
        return false;
    };
    if state.ptype != ProtocolType::Shutdown {
        return false;
    }
    if state.sender != "lead" || state.target != owner {
        return false;
    }
    if state.status != ProtocolStatus::Pending {
        return false;
    }
    let stopping = matches!(
        team.active.lock().unwrap().get(owner),
        Some(TeammateStatus::Stopping)
    );
    if stopping {
        return false;
    }
    team.active
        .lock()
        .unwrap()
        .insert(owner.into(), TeammateStatus::Stopping);
    true
}

/// Apply the Lead's plan-approval response if it matches this teammate's current plan.
fn apply_plan_response(team: &TeamCtx, owner: &str, msg: &MessageRecord) -> String {
    let request_id = match msg.metadata.get("request_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return "[Ignored plan response: no request_id]".into(),
    };
    let work_version = team.assignments.version(owner);
    let task_id = team.assignments.get(owner).map(|a| a.task_id);
    let mut pending = team.protocols.pending.lock().unwrap();
    let Some(state) = pending.get(&request_id) else {
        return "[Ignored plan response: request mismatch]".into();
    };
    let expected_id = team
        .protocols
        .plan_request_ids
        .lock()
        .unwrap()
        .get(owner)
        .cloned();
    let approve = msg
        .metadata
        .get("approve")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let valid = state.ptype == ProtocolType::PlanApproval
        && state.sender == owner
        && state.target == "lead"
        && Some(&request_id) == expected_id.as_ref()
        && state.work_version == Some(work_version)
        && state.task_id == task_id
        && state.status == ProtocolStatus::Pending
        && approve;
    if !valid {
        return "[Ignored plan response: request mismatch]".into();
    }
    let new_gate = if approve { GateStatus::Approved } else { GateStatus::Rejected };
    let new_status = if approve { ProtocolStatus::Approved } else { ProtocolStatus::Rejected };
    pending.get_mut(&request_id).unwrap().status = new_status;
    drop(pending);
    team.protocols.set_gate(owner, new_gate);
    team.protocols.plan_request_ids.lock().unwrap().remove(owner);
    team.active
        .lock()
        .unwrap()
        .insert(owner.into(), TeammateStatus::Working);
    format!(
        "[Plan {}] {}",
        if approve { "approved" } else { "rejected" },
        msg.content
    )
}

// ---- s13 Lead inbox delivery ----

/// Consume the Lead inbox, advancing protocol state for typed responses
/// (shutdown_response / plan_approval_response). Returns the drained messages
/// for the REPL to surface to the Lead as a new turn.
pub fn consume_lead_inbox(team: &TeamCtx) -> Vec<MessageRecord> {
    let msgs = team.bus.read_inbox("lead");
    for msg in &msgs {
        if msg.msg_type.ends_with("_response") {
            let request_id = msg
                .metadata
                .get("request_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let approve = msg.metadata.get("approve").and_then(|v| v.as_bool()).unwrap_or(false);
            team.protocols
                .match_response(&msg.msg_type, request_id, approve, &msg.from, &msg.to);
        }
    }
    msgs
}

/// Render drained Lead-inbox events as a single user message body.
pub fn format_team_events(msgs: &[MessageRecord]) -> String {
    let mut lines = Vec::new();
    for msg in msgs {
        let rid = msg
            .metadata
            .get("request_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let suffix = if rid.is_empty() {
            String::new()
        } else {
            format!(" request_id={}", rid)
        };
        lines.push(format!("[{}{}] {}: {}", msg.msg_type, suffix, msg.from, msg.content));
    }
    format!("[Team events]\n{}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_system::store::create_test_store;
    use tempfile::TempDir;

    fn ctx(tmp: &tempfile::TempDir) -> (Arc<TaskStore>, TeamCtx) {
        let store = Arc::new(create_test_store(tmp.path()));
        let team = TeamCtx::new(tmp.path().to_path_buf(), store.clone()).unwrap();
        (store, team)
    }

    #[test]
    fn claim_succeeds_for_pending_unowned() {
        let tmp = TempDir::new().unwrap();
        let (store, team) = ctx(&tmp);
        let t = store.create("T".into(), "".into(), vec![]).unwrap();
        assert!(claim_task(&team, &t.id, "alice").starts_with("Claimed"));
        assert_eq!(team.assignments.get("alice").unwrap().task_id, t.id);
    }

    #[test]
    fn claim_blocks_second_owner() {
        let tmp = TempDir::new().unwrap();
        let (store, team) = ctx(&tmp);
        let t = store.create("T".into(), "".into(), vec![]).unwrap();
        claim_task(&team, &t.id, "alice");
        let r = claim_task(&team, &t.id, "bob");
        // alice's claim moved the task to InProgress, so bob is rejected at the
        // status gate ("is in_progress, cannot claim"); the "already owned"
        // branch only fires for the inconsistent Pending+owned state.
        assert!(r.contains("cannot claim"), "second owner must be blocked, got {}", r);
    }

    #[test]
    fn claim_rejects_second_task_same_owner() {
        let tmp = TempDir::new().unwrap();
        let (store, team) = ctx(&tmp);
        let t1 = store.create("A".into(), "".into(), vec![]).unwrap();
        let t2 = store.create("B".into(), "".into(), vec![]).unwrap();
        claim_task(&team, &t1.id, "alice");
        let r = claim_task(&team, &t2.id, "alice");
        assert!(r.contains("finish current work"));
    }

    #[test]
    fn complete_checks_owner() {
        let tmp = TempDir::new().unwrap();
        let (store, team) = ctx(&tmp);
        let t = store.create("T".into(), "".into(), vec![]).unwrap();
        claim_task(&team, &t.id, "alice");
        let r = complete_task(&team, &t.id, "bob");
        assert!(r.contains("owned by alice"));
    }

    #[test]
    fn concurrent_claims_one_winner() {
        let tmp = TempDir::new().unwrap();
        let (store, team) = ctx(&tmp);
        let t = store.create("T".into(), "".into(), vec![]).unwrap();
        let tid = t.id.clone();
        let team = Arc::new(team);
        let mut handles = vec![];
        for name in ["alice", "bob", "carol"] {
            let team = Arc::clone(&team);
            let tid = tid.clone();
            handles.push(std::thread::spawn(move || claim_task(&team, &tid, name)));
        }
        let results: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners = results.iter().filter(|r| r.starts_with("Claimed")).count();
        assert_eq!(winners, 1, "exactly one concurrent claim must win, got {:?}", results);
    }

    #[test]
    fn consume_lead_inbox_matches_response() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(create_test_store(tmp.path()));
        let team = TeamCtx::new(tmp.path().to_path_buf(), store).unwrap();
        // seed a pending shutdown request
        use crate::team::protocols::{ProtocolState, ProtocolStatus, ProtocolType};
        team.protocols.pending.lock().unwrap().insert(
            "req_000001".into(),
            ProtocolState {
                request_id: "req_000001".into(),
                ptype: ProtocolType::Shutdown,
                sender: "lead".into(),
                target: "alice".into(),
                status: ProtocolStatus::Pending,
                payload: String::new(),
                work_version: None,
                task_id: None,
            },
        );
        // teammate replies shutdown_response into lead inbox
        team.bus.send(
            "alice",
            "lead",
            "ack",
            "shutdown_response",
            Some(serde_json::json!({"request_id": "req_000001", "approve": true})),
        );
        let inbox = consume_lead_inbox(&team);
        assert_eq!(inbox.len(), 1);
        assert_eq!(
            team.protocols
                .pending
                .lock()
                .unwrap()
                .get("req_000001")
                .unwrap()
                .status,
            ProtocolStatus::Approved
        );
    }

    #[test]
    fn format_team_events_shape() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(create_test_store(tmp.path()));
        let team = TeamCtx::new(tmp.path().to_path_buf(), store).unwrap();
        team.bus.send("alice", "lead", "done", "result", None);
        let inbox = team.bus.read_inbox("lead");
        let s = format_team_events(&inbox);
        assert!(s.starts_with("[Team events]"));
        assert!(s.contains("[result] alice: done"));
    }

    #[tokio::test]
    async fn shutdown_all_aborts_stuck_teammate() {
        // A "stuck" teammate: a spawned task that never completes on its own.
        // shutdown_all must abort it (dropping the future → Drop runs → the
        // assignment would be released by the guard) and remove it from the
        // handle map. The DropFlag proves the task's future was actually dropped.
        use std::sync::atomic::{AtomicBool, Ordering};
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let tmp = TempDir::new().unwrap();
        let store = Arc::new(create_test_store(tmp.path()));
        let team = TeamCtx::new(tmp.path().to_path_buf(), store).unwrap();
        let aborted = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&aborted);
        let handle = tokio::spawn(async move {
            let _g = DropFlag(flag);
            // Park forever once polled — a teammate stuck mid-run.
            std::future::pending::<()>().await;
        });
        team.handles.lock().unwrap().insert("alice".into(), handle);
        // Let the runtime poll the spawned task once so its DropFlag local is
        // constructed before we abort it (aborting an un-polled task resolves
        // the join without dropping the never-constructed future).
        tokio::task::yield_now().await;

        shutdown_all(&team).await;

        assert!(
            !team.handles.lock().unwrap().contains_key("alice"),
            "handle not removed from map"
        );
        assert!(
            aborted.load(Ordering::SeqCst),
            "aborted task's future was not dropped"
        );
    }
}
