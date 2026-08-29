use std::path::PathBuf;
use std::sync::Mutex;
use std::collections::HashMap;

use crate::task_system::task::TaskStatus;
use crate::task_system::store::TaskStore;
use super::worktree::task_worktree_cwd;

/// A teammate's current assignment: the task it is working and the cwd it
/// should operate in (repo dir in Phase 1, task worktree in Phase 2).
#[derive(Clone, Debug)]
pub struct Assignment {
    pub task_id: String,
    pub cwd: PathBuf,
}

/// In-memory registry of active assignments + a per-owner version counter.
/// The version lets plan approvals be invalidated when the owner re-claims a
/// new task (advance_version) after submitting a plan.
pub struct AssignmentRegistry {
    pub assignments: Mutex<HashMap<String, Assignment>>,
    pub versions: Mutex<HashMap<String, u32>>,
}

impl AssignmentRegistry {
    pub fn new() -> Self {
        Self {
            assignments: Mutex::new(HashMap::new()),
            versions: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, owner: &str) -> Option<Assignment> {
        self.assignments.lock().unwrap().get(owner).cloned()
    }

    pub fn set(&self, owner: &str, a: Assignment) {
        self.assignments.lock().unwrap().insert(owner.into(), a);
    }

    pub fn remove(&self, owner: &str) -> Option<Assignment> {
        self.assignments.lock().unwrap().remove(owner)
    }

    /// Snapshot all (owner, assignment) pairs (s13 worktree removal: detect leases).
    pub fn snap(&self) -> Vec<(String, Assignment)> {
        self.assignments
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn version(&self, owner: &str) -> u32 {
        *self.versions.lock().unwrap().get(owner).unwrap_or(&0)
    }

    /// Invalidate stale plan approvals without clearing an explicit requirement.
    pub fn advance_version(&self, owner: &str) -> u32 {
        let mut v = self.versions.lock().unwrap();
        let n = v.get(owner).copied().unwrap_or(0) + 1;
        v.insert(owner.into(), n);
        n
    }
}

/// Resolve a teammate's current cwd. No assignment → repo workdir.
/// Assignment whose task is no longer active (pending/completed-but-unowned)
/// → Err (fail-closed, never silently fall back). Broken worktree binding → Err.
pub fn assignment_cwd(
    workdir: &std::path::Path,
    store: &TaskStore,
    registry: &AssignmentRegistry,
    owner: &str,
) -> Result<PathBuf, String> {
    if let Some(a) = registry.get(owner) {
        let task = store.load(&a.task_id).map_err(|e| e.to_string())?;
        if task.status != TaskStatus::InProgress && task.status != TaskStatus::Completed {
            return Err(format!("Assignment for {} is no longer active", owner));
        }
        if task.owner.as_deref() != Some(owner) {
            return Err(format!("Assignment for {} is no longer active", owner));
        }
        let (cwd, err) = task_worktree_cwd(workdir, &task);
        if let Some(e) = err {
            return Err(e);
        }
        return Ok(cwd);
    }
    Ok(workdir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_system::task::Task;
    use crate::task_system::store::create_test_store;
    use tempfile::TempDir;

    fn store_with_task(owner: Option<&str>, status: TaskStatus) -> (TempDir, TaskStore, Task) {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());
        let mut t = store.create("T".into(), "".into(), vec![]).unwrap();
        t.status = status;
        t.owner = owner.map(String::from);
        store.save(&t).unwrap();
        (tmp, store, t)
    }

    #[test]
    fn no_assignment_returns_workdir() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());
        let reg = AssignmentRegistry::new();
        let cwd = assignment_cwd(tmp.path(), &store, &reg, "alice").unwrap();
        assert_eq!(cwd, tmp.path());
    }

    #[test]
    fn version_advances() {
        let reg = AssignmentRegistry::new();
        assert_eq!(reg.version("alice"), 0);
        assert_eq!(reg.advance_version("alice"), 1);
        assert_eq!(reg.advance_version("alice"), 2);
        assert_eq!(reg.version("alice"), 2);
    }

    #[test]
    fn assignment_for_inactive_task_errors() {
        let (tmp, store, t) = store_with_task(Some("alice"), TaskStatus::Pending);
        let reg = AssignmentRegistry::new();
        reg.set(
            "alice",
            Assignment {
                task_id: t.id,
                cwd: tmp.path().to_path_buf(),
            },
        );
        let r = assignment_cwd(tmp.path(), &store, &reg, "alice");
        assert!(r.is_err(), "pending task assignment must error, got {:?}", r);
    }
}
