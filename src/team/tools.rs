use async_trait::async_trait;
use serde_json::{json, Value};
use crate::tools::trait_def::{AgentKind, PermissionCheck, Tool, ToolContext};
use crate::team::protocols::{GateStatus, ProtocolState, ProtocolStatus, ProtocolType};
use crate::team::{TeamCtx, TeammateStatus};

/// Teammate-only: submit a plan for Lead approval. Records work_version +
/// task_id so a later claim/complete invalidates the approval.
pub fn submit_plan(team: &TeamCtx, owner: &str, plan: &str) -> String {
    let task_id = team.assignments.get(owner).map(|a| a.task_id.clone());
    let work_version = team.assignments.version(owner);

    // Fast path: if a plan is already pending, refuse before allocating an id.
    if team.protocols.gate(owner) == GateStatus::Pending {
        return "A plan is already waiting for review.".into();
    }
    // Allocate the request id WITHOUT holding the pending lock — new_request_id
    // locks pending internally, so taking it under our own pending lock would
    // deadlock (std Mutex is not reentrant).
    let request_id = team.protocols.new_request_id();
    {
        let mut pending = team.protocols.pending.lock().unwrap();
        // Re-check under the lock in case of a concurrent submit.
        if team.protocols.gate(owner) == GateStatus::Pending {
            return "A plan is already waiting for review.".into();
        }
        pending.insert(
            request_id.clone(),
            ProtocolState {
                request_id: request_id.clone(),
                ptype: ProtocolType::PlanApproval,
                sender: owner.into(),
                target: "lead".into(),
                status: ProtocolStatus::Pending,
                payload: plan.into(),
                work_version: Some(work_version),
                task_id,
            },
        );
    }
    team.protocols
        .plan_request_ids
        .lock()
        .unwrap()
        .insert(owner.into(), request_id.clone());
    team.bus.send(
        owner,
        "lead",
        plan,
        "plan_approval_request",
        Some(json!({ "request_id": request_id })),
    );
    team.protocols.set_gate(owner, GateStatus::Pending);
    team.active
        .lock()
        .unwrap()
        .insert(owner.into(), TeammateStatus::WaitingApproval);
    // Wake the Lead so it sees the plan_approval_request.
    team.lead_notify.notify_one();
    "Plan submitted. Wait for Lead's decision.".into()
}

/// Teammate tool: submit a plan for Lead approval before mutating files/bash.
pub struct SubmitPlanTool;

#[async_trait]
impl Tool for SubmitPlanTool {
    fn name(&self) -> &str {
        "submit_plan"
    }
    fn description(&self) -> &str {
        "Submit a work plan for Lead approval before changing files or running bash."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"plan":{"type":"string"}},"required":["plan"]})
    }
    fn check_permission(&self, _: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }
    fn available_for(&self, kind: AgentKind) -> bool {
        kind == AgentKind::Teammate
    }
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let plan = input.get("plan").and_then(|v| v.as_str()).unwrap_or("");
        let Some(team) = &ctx.agent.team else {
            return "Error: not in team context".into();
        };
        submit_plan(team, ctx.owner(), plan)
    }
}

/// Lead tool: spawn a persistent teammate bound to an optional task.
pub struct SpawnTeammateTool;

#[async_trait]
impl Tool for SpawnTeammateTool {
    fn name(&self) -> &str {
        "spawn_teammate"
    }
    fn description(&self) -> &str {
        "Spawn a persistent teammate. Propose a team and wait for user confirmation first."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "name":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,64}$"},
            "role":{"type":"string"},
            "prompt":{"type":"string"},
            "task_id":{"type":"string","pattern":"^task_[0-9a-f]{8}$"},
            "require_plan":{"type":"boolean"}
        },"required":["name","role","prompt"]})
    }
    fn check_permission(&self, _: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }
    fn available_for(&self, kind: AgentKind) -> bool {
        kind == AgentKind::Lead
    }
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let role = input.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        let task_id = input.get("task_id").and_then(|v| v.as_str());
        let require_plan = input.get("require_plan").and_then(|v| v.as_bool()).unwrap_or(false);
        crate::team::runtime::spawn_teammate_thread(ctx.agent, name, role, prompt, task_id, require_plan)
    }
}

// ---- s13 Lead team tools ----

pub fn list_teammates(team: &crate::team::TeamCtx) -> String {
    let active = team.active.lock().unwrap();
    if active.is_empty() {
        return "No active teammates.".into();
    }
    active
        .iter()
        .map(|(n, s)| format!("{}: {:?}", n, s))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn send_message(team: &crate::team::TeamCtx, to: &str, content: &str) -> String {
    let active = team.active.lock().unwrap();
    if to != "lead"
        && !active.contains_key(to)
        && !active.keys().any(|k| k.eq_ignore_ascii_case(to))
    {
        return format!("Teammate '{}' is not active", to);
    }
    drop(active);
    team.bus.send("lead", to, content, "message", None);
    format!("Sent to {}", to)
}

pub fn request_shutdown(team: &crate::team::TeamCtx, teammate: &str) -> String {
    let active = team.active.lock().unwrap();
    if !active.keys().any(|k| k.eq_ignore_ascii_case(teammate)) {
        return format!("Teammate '{}' is not active", teammate);
    }
    drop(active);
    let request_id = team.protocols.new_request_id();
    team.protocols.pending.lock().unwrap().insert(
        request_id.clone(),
        ProtocolState {
            request_id: request_id.clone(),
            ptype: ProtocolType::Shutdown,
            sender: "lead".into(),
            target: teammate.into(),
            status: ProtocolStatus::Pending,
            payload: String::new(),
            work_version: None,
            task_id: None,
        },
    );
    team.bus.send(
        "lead",
        teammate,
        "Finish the current step and shut down.",
        "shutdown_request",
        Some(json!({ "request_id": request_id })),
    );
    format!("Shutdown requested from {} ({})", teammate, request_id)
}

pub fn request_plan(team: &crate::team::TeamCtx, teammate: &str, task: &str) -> String {
    let active = team.active.lock().unwrap();
    if !active.keys().any(|k| k.eq_ignore_ascii_case(teammate)) {
        return format!("Teammate '{}' is not active", teammate);
    }
    drop(active);
    team.protocols.set_gate(teammate, GateStatus::Required);
    team.bus.send("lead", teammate, task, "plan_request", None);
    format!("Plan requested from {}", teammate)
}

pub fn review_plan(
    team: &crate::team::TeamCtx,
    request_id: &str,
    approve: bool,
    feedback: &str,
) -> String {
    let sender = {
        let pending = team.protocols.pending.lock().unwrap();
        let Some(state) = pending.get(request_id) else {
            return format!("Request {} not found", request_id);
        };
        if state.ptype != ProtocolType::PlanApproval {
            return format!("Request {} is not a plan", request_id);
        }
        if state.status != ProtocolStatus::Pending {
            return format!("Request {} already {:?}", request_id, state.status);
        }
        state.sender.clone()
    };
    let work_version = team.assignments.version(&sender);
    let task_id = team.assignments.get(&sender).map(|a| a.task_id.clone());
    let expected_id = team
        .protocols
        .plan_request_ids
        .lock()
        .unwrap()
        .get(&sender)
        .cloned();
    let mut pending = team.protocols.pending.lock().unwrap();
    let Some(state) = pending.get_mut(request_id) else {
        return format!("Request {} not found", request_id);
    };
    if state.work_version != Some(work_version) || state.task_id != task_id {
        return format!("Request {} belongs to an earlier assignment", request_id);
    }
    if expected_id.as_deref() != Some(request_id) {
        return format!("Request {} is not the current plan", request_id);
    }
    state.status = if approve {
        ProtocolStatus::Approved
    } else {
        ProtocolStatus::Rejected
    };
    drop(pending);
    team.protocols.set_gate(
        &sender,
        if approve {
            GateStatus::Approved
        } else {
            GateStatus::Rejected
        },
    );
    team.protocols.plan_request_ids.lock().unwrap().remove(&sender);
    let content = if !feedback.is_empty() {
        feedback.to_string()
    } else if approve {
        "Plan approved.".to_string()
    } else {
        "Revise the plan and submit it again.".to_string()
    };
    team.bus.send(
        "lead",
        &sender,
        &content,
        "plan_approval_response",
        Some(json!({ "request_id": request_id, "approve": approve })),
    );
    format!(
        "Plan {} ({})",
        if approve { "approved" } else { "rejected" },
        request_id
    )
}

pub struct ListTeammatesTool;
#[async_trait]
impl Tool for ListTeammatesTool {
    fn name(&self) -> &str {
        "list_teammates"
    }
    fn description(&self) -> &str {
        "List active teammates and their status."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    fn check_permission(&self, _: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }
    fn available_for(&self, kind: AgentKind) -> bool {
        kind == AgentKind::Lead
    }
    async fn execute(&self, ctx: &ToolContext<'_>, _input: &Value) -> String {
        let Some(team) = &ctx.agent.team else {
            return "Error: not in team context".into();
        };
        list_teammates(team)
    }
}

pub struct SendMessageTool;
#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }
    fn description(&self) -> &str {
        "Send a message to 'lead' or an active teammate."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"to":{"type":"string"},"content":{"type":"string"}},"required":["to","content"]})
    }
    fn check_permission(&self, _: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(team) = &ctx.agent.team else {
            return "Error: not in team context".into();
        };
        let to = input.get("to").and_then(|v| v.as_str()).unwrap_or("");
        let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
        send_message(team, to, content)
    }
    fn available_for(&self, kind: AgentKind) -> bool {
        kind == AgentKind::Lead
    }
}

pub struct RequestShutdownTool;
#[async_trait]
impl Tool for RequestShutdownTool {
    fn name(&self) -> &str {
        "request_shutdown"
    }
    fn description(&self) -> &str {
        "Ask a teammate to finish its current step and shut down."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"teammate":{"type":"string"}},"required":["teammate"]})
    }
    fn check_permission(&self, _: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }
    fn available_for(&self, kind: AgentKind) -> bool {
        kind == AgentKind::Lead
    }
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(team) = &ctx.agent.team else {
            return "Error: not in team context".into();
        };
        let t = input.get("teammate").and_then(|v| v.as_str()).unwrap_or("");
        request_shutdown(team, t)
    }
}

pub struct RequestPlanTool;
#[async_trait]
impl Tool for RequestPlanTool {
    fn name(&self) -> &str {
        "request_plan"
    }
    fn description(&self) -> &str {
        "Require a teammate to submit a plan before workspace changes."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"teammate":{"type":"string"},"task":{"type":"string"}},"required":["teammate","task"]})
    }
    fn check_permission(&self, _: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }
    fn available_for(&self, kind: AgentKind) -> bool {
        kind == AgentKind::Lead
    }
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(team) = &ctx.agent.team else {
            return "Error: not in team context".into();
        };
        let t = input.get("teammate").and_then(|v| v.as_str()).unwrap_or("");
        let task = input.get("task").and_then(|v| v.as_str()).unwrap_or("");
        request_plan(team, t, task)
    }
}

pub struct ReviewPlanTool;
#[async_trait]
impl Tool for ReviewPlanTool {
    fn name(&self) -> &str {
        "review_plan"
    }
    fn description(&self) -> &str {
        "Approve or reject a teammate's submitted plan."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"request_id":{"type":"string"},"approve":{"type":"boolean"},"feedback":{"type":"string"}},"required":["request_id","approve"]})
    }
    fn check_permission(&self, _: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }
    fn available_for(&self, kind: AgentKind) -> bool {
        kind == AgentKind::Lead
    }
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(team) = &ctx.agent.team else {
            return "Error: not in team context".into();
        };
        let rid = input.get("request_id").and_then(|v| v.as_str()).unwrap_or("");
        let approve = input.get("approve").and_then(|v| v.as_bool()).unwrap_or(false);
        let feedback = input.get("feedback").and_then(|v| v.as_str()).unwrap_or("");
        review_plan(team, rid, approve, feedback)
    }
}

/// Lead tool: create + bind a task worktree. Task must be pending and unowned.
pub struct CreateWorktreeTool;

#[async_trait]
impl Tool for CreateWorktreeTool {
    fn name(&self) -> &str {
        "create_worktree"
    }
    fn description(&self) -> &str {
        "Create and bind a task worktree (Lead only). Task must be pending and unowned."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "name":{"type":"string","pattern":"^(?!.*\\.\\.)[A-Za-z0-9][A-Za-z0-9._-]{0,63}$","maxLength":64},
            "task_id":{"type":"string"}
        },"required":["name","task_id"],"additionalProperties":false})
    }
    fn check_permission(&self, _: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }
    fn available_for(&self, kind: AgentKind) -> bool {
        kind == AgentKind::Lead
    }
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(team) = &ctx.agent.team else {
            return "Error: not in team context".into();
        };
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let task_id = input.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
        crate::team::worktree::create_worktree(team, name, task_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_system::store::create_test_store;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx() -> (tempfile::TempDir, Arc<TeamCtx>) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(create_test_store(tmp.path()));
        let team = Arc::new(TeamCtx::new(tmp.path().to_path_buf(), store).unwrap());
        (tmp, team)
    }

    #[test]
    fn submit_plan_sets_pending_gate() {
        let (_tmp, team) = ctx();
        let r = submit_plan(&team, "alice", "step 1; step 2");
        assert!(r.contains("submitted"));
        assert_eq!(team.protocols.gate("alice"), GateStatus::Pending);
        assert!(team.bus.peek("lead"));
    }

    #[test]
    fn submit_plan_rejects_duplicate() {
        let (_tmp, team) = ctx();
        submit_plan(&team, "alice", "plan a");
        let r = submit_plan(&team, "alice", "plan b");
        assert!(r.contains("already waiting"));
    }

    #[test]
    fn list_teammates_empty_then_with() {
        let (_tmp, team) = ctx();
        assert_eq!(list_teammates(&team), "No active teammates.");
        team.active
            .lock()
            .unwrap()
            .insert("alice".into(), crate::team::TeammateStatus::Working);
        assert!(list_teammates(&team).contains("alice"));
    }

    #[test]
    fn send_message_rejects_inactive() {
        let (_tmp, team) = ctx();
        let r = send_message(&team, "ghost", "hi");
        assert!(r.contains("not active"));
    }

    #[test]
    fn request_shutdown_creates_pending() {
        let (_tmp, team) = ctx();
        team.active
            .lock()
            .unwrap()
            .insert("alice".into(), crate::team::TeammateStatus::Idle);
        let r = request_shutdown(&team, "alice");
        assert!(r.contains("Shutdown requested"));
        assert!(team.protocols.pending.lock().unwrap().len() == 1);
        assert!(team.bus.peek("alice"));
    }

    #[test]
    fn review_plan_approves_matching() {
        let (_tmp, team) = ctx();
        // simulate a submitted plan: submit_plan sets gate=pending + a pending request
        submit_plan(&team, "alice", "plan");
        let rid = team
            .protocols
            .plan_request_ids
            .lock()
            .unwrap()
            .get("alice")
            .cloned()
            .unwrap();
        let r = review_plan(&team, &rid, true, "");
        assert!(r.contains("approved"));
        assert_eq!(
            team.protocols.gate("alice"),
            crate::team::protocols::GateStatus::Approved
        );
        assert!(team.bus.peek("alice"));
    }

    #[test]
    fn review_plan_rejects_stale_version() {
        // If the owner re-claimed (advance_version) after submit, the approval must not apply.
        let (_tmp, team) = ctx();
        submit_plan(&team, "alice", "plan");
        let rid = team
            .protocols
            .plan_request_ids
            .lock()
            .unwrap()
            .get("alice")
            .cloned()
            .unwrap();
        team.assignments.advance_version("alice"); // stale
        let r = review_plan(&team, &rid, true, "");
        assert!(r.contains("earlier assignment"));
    }
}
