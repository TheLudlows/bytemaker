use std::sync::Arc;

use crate::agent::Agent;
use crate::domain::message::{ContentBlock, Message};
use crate::team::protocols::GateStatus;
use crate::team::{
    claim_next_task, drain_inbox, extract_last_assistant_text,
    release_completed_assignment, release_teammate_assignment, teammate_system_prompt,
    TeamCtx, TeammateStatus, IDLE_SCAN_INTERVAL,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Continue,
    Idle,
    Stop,
}

/// RAII cleanup for a teammate: on Drop — whether the run loop returns, panics,
/// or the task is aborted by `shutdown_all` — release the assignment and
/// deregister it, so a panicked/aborted teammate never leaves its task stuck
/// `InProgress`. Because Drop runs during unwind, this covers panics that a
/// discarded `JoinHandle` would otherwise silently swallow.
struct TeammateShutdownGuard {
    team: Arc<TeamCtx>,
    name: String,
}

impl Drop for TeammateShutdownGuard {
    fn drop(&mut self) {
        // Idempotent: release_teammate_assignment only acts on InProgress tasks owned by
        // this name, so a normal run (which already released) is a no-op here. Recover on
        // poison so a panic while holding one of these locks doesn't cascade.
        release_teammate_assignment(&self.team, &self.name);
        self.team
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.name);
        self.team
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.name);
    }
}

/// One persistent teammate: its agent, conversation, and shared team context.
/// Moved into `tokio::spawn`; must be `Send` (Agent/Hooks are Send+Sync, verified).
pub struct TeammateRuntime {
    pub name: String,
    pub agent: Agent,
    pub messages: Vec<Message>,
    pub team: Arc<TeamCtx>,
}

impl TeammateRuntime {
    /// Build the initial messages, including an [Assigned task] block if the
    /// teammate was spawned with an already-claimed task. `child_agent` is built
    /// by the caller via `lead_agent.child_teammate(...)` — the runtime does NOT
    /// reference the Lead agent, avoiding a TeamCtx → Agent → TeamCtx Arc cycle.
    pub fn new(name: String, _role: &str, prompt: String, team: Arc<TeamCtx>, child_agent: Agent) -> Self {
        let mut messages = vec![Message::user_text(prompt)];
        if let Some(a) = team.assignments.get(&name) {
            if let Ok(task) = team.task_store.load(&a.task_id) {
                let block = format!(
                    "\n\n[Assigned task {}] {}\n{}\nWork directory: {}",
                    task.id,
                    task.subject,
                    task.description,
                    a.cwd.display()
                );
                if let Some(ContentBlock::Text { text }) = messages[0].content.get_mut(0) {
                    text.push_str(&block);
                }
            }
        }
        Self { name, agent: child_agent, messages, team }
    }

    /// Run the teammate until it stops (shutdown accepted or work done + idle exit).
    pub async fn run(mut self) {
        // Cleanup on any exit — return, break, panic, or task abort — is the guard's
        // Drop: release the assignment + deregister. One source of truth, and unlike a
        // tail call it also covers panics mid-turn.
        let _guard = TeammateShutdownGuard {
            team: Arc::clone(&self.team),
            name: self.name.clone(),
        };
        let mut phase = Phase::Continue;
        while phase != Phase::Stop {
            if phase == Phase::Idle {
                if !self.wait_for_work().await {
                    break;
                }
            }
            phase = self.work().await;
        }
    }

    /// Run one model turn. Returns the next phase (Stop on shutdown ack, Idle while a
    /// plan is pending approval, Idle after delivering a result).
    ///
    /// Note: called every turn; reloads memory each turn (see `load_memories` +
    /// `read_memory_index` below) — unlike the Lead's load-once-per-user-turn
    /// convention, this repeats the query every turn. Known behavior.
    async fn work(&mut self) -> Phase {
        // Clone `name` so the cross-`await` borrows are two disjoint fields
        // (self.agent immutable, self.messages mutable) rather than three.
        let name = self.name.clone();

        // Load memory every turn (unlike the Lead's load-once; see work() doc).
        let recalled = self.agent.memory.load_memories(self.agent.client.as_ref(), &self.messages).await;
        let index = self.agent.memory.read_memory_index();

        let _ = self.agent.run_loop(&mut self.messages, &name, &recalled, &index).await;

        if matches!(
            self.team.active.lock().unwrap().get(&self.name).copied(),
            Some(TeammateStatus::Stopping)
        ) {
            return Phase::Stop;
        }
        let gate = self.team.protocols.gate(&self.name);
        if gate == GateStatus::Pending {
            self.team
                .active
                .lock()
                .unwrap()
                .insert(self.name.clone(), TeammateStatus::WaitingApproval);
            return Phase::Idle;
        }
        if let Some(summary) = extract_last_assistant_text(&self.messages) {
            self.team.bus.send(&self.name, "lead", &summary, "result", None);
            self.team.lead_notify.notify_one();
        }
        release_completed_assignment(&self.team, &self.name);
        self.team
            .active
            .lock()
            .unwrap()
            .insert(self.name.clone(), TeammateStatus::Idle);
        self.team
            .bus
            .send(&self.name, "lead", "Waiting for more work.", "idle_notification", None);
        self.team.lead_notify.notify_one();
        Phase::Idle
    }

    /// Block until there is work: a mailbox message or a claimable task.
    /// Returns false if a shutdown was accepted (caller should exit).
    async fn wait_for_work(&mut self) -> bool {
        loop {
            let inbox = self.team.bus.wait_for_messages(&self.name, IDLE_SCAN_INTERVAL).await;
            if !inbox.is_empty() {
                let before = self.messages.len();
                let stop = drain_inbox(&self.team, &self.name, &mut self.messages);
                if stop {
                    return false;
                }
                if self.messages.len() > before {
                    return true;
                }
                continue;
            }
            if let Some(task) = claim_next_task(&self.team, &self.name) {
                let cwd = self
                    .team
                    .assignments
                    .get(&self.name)
                    .map(|a| a.cwd.clone())
                    .unwrap_or_else(|| self.team.workdir.clone());
                let text = format!(
                    "[Auto-claimed task {}] {}\n{}\nWork directory: {}",
                    task.id,
                    task.subject,
                    task.description,
                    cwd.display()
                );
                self.messages.push(Message::user_text(text));
                return true;
            }
        }
    }
}

/// Validate + (optionally) claim an initial task, then spawn one persistent teammate.
///
/// Note: the `tokio::spawn` below is unconditional (no `#[cfg]` gate), so a plain
/// `#[test]` without a tokio reactor panics with "there is no reactor running".
pub fn spawn_teammate_thread(
    lead_agent: &Agent,
    name: &str,
    role: &str,
    prompt: &str,
    task_id: Option<&str>,
    require_plan: bool,
) -> String {
    use crate::team::bus::is_valid_agent_name;
    use crate::team::{claim_task, is_reserved_teammate_name};

    if !is_valid_agent_name(name) {
        return "Invalid teammate name: use 1-64 letters, digits, underscores, or dashes".into();
    }
    if is_reserved_teammate_name(name) {
        return format!("Invalid teammate name: '{}' is reserved by the runtime", name);
    }
    let Some(team) = lead_agent.team() else {
        return "Error: team not initialized".into();
    };
    {
        let active = team.active.lock().unwrap();
        if active.keys().any(|k| k.eq_ignore_ascii_case(name)) {
            return format!("Teammate '{}' already exists", name);
        }
    }
    team.active
        .lock()
        .unwrap()
        .insert(name.into(), TeammateStatus::Working);
    team.protocols.set_gate(
        name,
        if require_plan { GateStatus::Required } else { GateStatus::NotRequired },
    );

    if let Some(tid) = task_id {
        let r = claim_task(team, tid, name);
        if !r.starts_with("Claimed") {
            team.active.lock().unwrap().remove(name);
            team.protocols.set_gate(name, GateStatus::NotRequired);
            return format!("Cannot spawn teammate '{}': {}", name, r);
        }
    }
    let system = teammate_system_prompt(name, role);
    let agent = lead_agent.child_teammate(name, &system, Arc::clone(team));
    let rt = TeammateRuntime::new(name.into(), role, prompt.into(), Arc::clone(team), agent);
    // Spawn a live runtime. No `#[cfg]` gate here — a plain `#[test]` without a
    // reactor panics (see the doc above).

    let handle = tokio::spawn(async move { rt.run().await });
    team.handles
        .lock()
        .unwrap()
        .insert(name.to_string(), handle);

    format!(
        "Teammate '{}' spawned as {}. End this turn; the runtime will deliver its events.",
        name, role
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::TestAgent;
    use crate::team::is_reserved_teammate_name;

    #[test]
    fn reserved_names_rejected() {
        assert!(is_reserved_teammate_name("lead"));
        assert!(is_reserved_teammate_name("Lead"));
        assert!(is_reserved_teammate_name("agent"));
        assert!(!is_reserved_teammate_name("alice"));
    }

    #[test]
    fn spawn_rejects_invalid_and_reserved() {
        let a = TestAgent::new();
        let r = spawn_teammate_thread(a.agent(), "../x", "r", "p", None, false);
        assert!(r.contains("Invalid"));
        let r = spawn_teammate_thread(a.agent(), "lead", "r", "p", None, false);
        assert!(r.contains("reserved"));
    }

    #[test]
    fn spawn_valid_teammate_does_not_require_runtime() {
        // A plain #[test] (no tokio reactor). spawn_teammate_thread must NOT call
        // tokio::spawn in unit-test builds without the smoke feature — otherwise
        // this panics: "no reactor running, must be called from the context of a
        // Tokio 1.x runtime" (the cfg gate the doc above promises was never written).
        let a = TestAgent::new();
        let r = spawn_teammate_thread(a.agent(), "alice", "r", "p", None, false);
        assert!(r.contains("spawned"), "got {}", r);
        // teammate registered for dedup even without a live runtime
        assert!(a.agent().team().unwrap().active.lock().unwrap().contains_key("alice"));
    }

    #[test]
    fn spawn_rejects_duplicate() {
        let a = TestAgent::new();
        let r1 = spawn_teammate_thread(a.agent(), "alice", "r", "p", None, false);
        assert!(r1.contains("spawned"));
        let r2 = spawn_teammate_thread(a.agent(), "alice", "r", "p", None, false);
        assert!(r2.contains("already exists"));
    }

    #[test]
    fn spawn_with_bad_task_does_not_spawn() {
        let a = TestAgent::new();
        let r = spawn_teammate_thread(a.agent(), "alice", "r", "p", Some("task_00000000"), false);
        assert!(r.contains("Cannot spawn"), "bad task_id must prevent spawn, got {}", r);
        // no active teammate left
        let team = a.agent().team().unwrap();
        assert!(!team.active.lock().unwrap().contains_key("alice"));
    }

    #[tokio::test]
    async fn guard_releases_assignment_on_panic() {
        use crate::task_system::store::create_test_store;
        use crate::task_system::task::TaskStatus;
        use crate::team::{claim_task, TeamCtx};

        let tmp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(create_test_store(tmp.path()));
        let team = Arc::new(TeamCtx::new(tmp.path().to_path_buf(), store.clone()).unwrap());

        // alice claims a task → InProgress + owner = alice.
        let task = store.create("T".into(), "".into(), vec![]).unwrap();
        let r = claim_task(&team, &task.id, "alice");
        assert!(r.contains("Claimed"), "{}", r);

        // Spawn a task that holds the guard and panics. The spawned future's
        // Drop runs during unwind, so the guard must release the assignment
        // even though the run loop body never reached its manual cleanup.
        let team2 = Arc::clone(&team);
        let handle = tokio::spawn(async move {
            let _guard = TeammateShutdownGuard {
                team: team2,
                name: "alice".to_string(),
            };
            panic!("boom");
        });
        let _ = handle.await; // panicked task → JoinError, ignored

        assert!(
            team.assignments.get("alice").is_none(),
            "assignment not released on panic"
        );
        let reloaded = store.load(&task.id).unwrap();
        assert_eq!(reloaded.status, TaskStatus::Pending, "task not returned to Pending");
        assert!(reloaded.owner.is_none(), "owner not cleared");
        assert!(
            !team.active.lock().unwrap().contains_key("alice"),
            "teammate still active after panic"
        );
    }
}
