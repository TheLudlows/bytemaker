use std::sync::Mutex;
use std::collections::HashMap;

/// The plan gate for a teammate: whether mutating tools (bash/write_file/
/// edit_file) are blocked pending Lead approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateStatus {
    NotRequired,
    Required,
    Pending,
    Approved,
    Rejected,
}

impl GateStatus {
    pub fn blocks_mutating_tools(&self) -> bool {
        matches!(self, Self::Required | Self::Pending | Self::Rejected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolType {
    Shutdown,
    PlanApproval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolStatus {
    Pending,
    Approved,
    Rejected,
}

/// One typed protocol request (shutdown or plan-approval) in flight.
#[derive(Debug, Clone)]
pub struct ProtocolState {
    pub request_id: String,
    pub ptype: ProtocolType,
    pub sender: String,
    pub target: String,
    pub status: ProtocolStatus,
    pub payload: String,
    pub work_version: Option<u32>,
    pub task_id: Option<String>,
}

/// Pending protocol requests + per-owner plan gate + current plan request id.
pub struct ProtocolRegistry {
    pub pending: Mutex<HashMap<String, ProtocolState>>,
    pub gates: Mutex<HashMap<String, GateStatus>>,
    pub plan_request_ids: Mutex<HashMap<String, String>>,
}

impl ProtocolRegistry {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
            plan_request_ids: Mutex::new(HashMap::new()),
        }
    }

    pub fn gate(&self, owner: &str) -> GateStatus {
        self.gates
            .lock()
            .unwrap()
            .get(owner)
            .copied()
            .unwrap_or(GateStatus::NotRequired)
    }

    pub fn set_gate(&self, owner: &str, g: GateStatus) {
        self.gates.lock().unwrap().insert(owner.into(), g);
    }

    /// Match a typed response to a pending request. Returns false on any mismatch
    /// (wrong type, wrong role pair, already-resolved). On a match, resolves the
    /// request status to Approved/Rejected.
    pub fn match_response(
        &self,
        response_type: &str,
        request_id: &str,
        approve: bool,
        from_agent: &str,
        to_agent: &str,
    ) -> bool {
        let mut pending = self.pending.lock().unwrap();
        let Some(state) = pending.get(request_id) else {
            return false;
        };
        let expected = match state.ptype {
            ProtocolType::Shutdown => "shutdown_response",
            ProtocolType::PlanApproval => "plan_approval_response",
        };
        if response_type != expected {
            return false;
        }
        if from_agent != state.target || to_agent != state.sender {
            return false;
        }
        if state.status != ProtocolStatus::Pending {
            return false;
        }
        let new_status = if approve {
            ProtocolStatus::Approved
        } else {
            ProtocolStatus::Rejected
        };
        let state = pending.get_mut(request_id).unwrap();
        state.status = new_status;
        true
    }

    pub fn new_request_id(&self) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        loop {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst) % 1_000_000;
            let id = format!("req_{:06}", n);
            if !self.pending.lock().unwrap().contains_key(&id) {
                return id;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shutdown_req() -> ProtocolState {
        ProtocolState {
            request_id: "req_000001".into(),
            ptype: ProtocolType::Shutdown,
            sender: "lead".into(),
            target: "alice".into(),
            status: ProtocolStatus::Pending,
            payload: String::new(),
            work_version: None,
            task_id: None,
        }
    }

    #[test]
    fn match_response_approves_on_valid_pair() {
        let reg = ProtocolRegistry::new();
        reg.pending.lock().unwrap().insert("req_000001".into(), shutdown_req());
        assert!(reg.match_response("shutdown_response", "req_000001", true, "alice", "lead"));
        assert_eq!(
            reg.pending.lock().unwrap().get("req_000001").unwrap().status,
            ProtocolStatus::Approved
        );
    }

    #[test]
    fn match_response_rejects_wrong_type() {
        let reg = ProtocolRegistry::new();
        reg.pending.lock().unwrap().insert("req_000001".into(), shutdown_req());
        assert!(!reg.match_response("plan_approval_response", "req_000001", true, "alice", "lead"));
    }

    #[test]
    fn match_response_rejects_wrong_role() {
        let reg = ProtocolRegistry::new();
        reg.pending.lock().unwrap().insert("req_000001".into(), shutdown_req());
        assert!(!reg.match_response("shutdown_response", "req_000001", true, "bob", "lead"));
    }

    #[test]
    fn match_response_rejects_double_resolution() {
        let reg = ProtocolRegistry::new();
        reg.pending.lock().unwrap().insert("req_000001".into(), shutdown_req());
        assert!(reg.match_response("shutdown_response", "req_000001", true, "alice", "lead"));
        assert!(
            !reg.match_response("shutdown_response", "req_000001", true, "alice", "lead"),
            "already-approved request must not resolve twice"
        );
    }

    #[test]
    fn gate_blocks_mutating_tools_states() {
        assert!(GateStatus::Required.blocks_mutating_tools());
        assert!(GateStatus::Pending.blocks_mutating_tools());
        assert!(GateStatus::Rejected.blocks_mutating_tools());
        assert!(!GateStatus::NotRequired.blocks_mutating_tools());
        assert!(!GateStatus::Approved.blocks_mutating_tools());
    }
}
