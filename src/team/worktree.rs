use std::path::{Path, PathBuf};
use std::process::Command;

use crate::task_system::task::{Task, TaskStatus};
use crate::team::TeamCtx;

/// Worktree name: 1-64 chars, starts [A-Za-z0-9], rest [A-Za-z0-9._-], no `..`.
/// Rust `regex` has no look-around, so the `..` rejection is a separate check.
const VALID_WORKTREE_NAME: &str = r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$";

pub fn validate_worktree_name(name: &str) -> Option<String> {
    let re = regex::Regex::new(VALID_WORKTREE_NAME).unwrap();
    if !re.is_match(name) || name.contains("..") {
        return Some(
            "worktree name must be 1-64 chars, start [A-Za-z0-9], rest [A-Za-z0-9._-], no '..'".into(),
        );
    }
    None
}

pub fn worktree_path(worktrees_dir: &Path, name: &str) -> Result<PathBuf, String> {
    if let Some(e) = validate_worktree_name(name) {
        return Err(e);
    }
    crate::tools::safe_path_in(worktrees_dir, name)
}

pub fn worktree_branch(name: &str) -> String {
    format!("wt/{}", name)
}

fn run_git(args: &[&str], cwd: &Path) -> (bool, String) {
    let out = Command::new("git").args(args).current_dir(cwd).output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let e = String::from_utf8_lossy(&o.stderr);
            (o.status.success(), format!("{}{}", s, e).trim().to_string())
        }
        Err(e) => (false, e.to_string()),
    }
}

/// Owned-String argv variant (for `worktree remove --force <path>` where the
/// path may need to live across the call).
fn run_git_owned(args: &[String], cwd: &Path) -> (bool, String) {
    let out = Command::new("git").args(args).current_dir(cwd).output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let e = String::from_utf8_lossy(&o.stderr);
            (o.status.success(), format!("{}{}", s, e).trim().to_string())
        }
        Err(e) => (false, e.to_string()),
    }
}

/// Is `<workdir>/.worktrees/<name>` a registered git worktree on branch `wt/<name>`?
fn registered_worktree(workdir: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = workdir.join(".worktrees");
    let path = worktree_path(&dir, name)?;
    let (ok, out) = run_git(&["worktree", "list", "--porcelain"], workdir);
    if !ok {
        return Err(format!("cannot read git worktree registry: {}", out));
    }
    let expected_branch = format!("refs/heads/{}", worktree_branch(name));
    let mut found = false;
    let mut current_path: Option<String> = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            current_path = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("branch ") {
            if rest == expected_branch
                && current_path.as_deref() == Some(&path.to_string_lossy())
            {
                found = true;
            }
        }
    }
    if !found {
        return Err(format!("worktree '{}' is not registered with git", name));
    }
    let p = PathBuf::from(current_path.unwrap());
    if !p.is_dir() {
        return Err(format!("worktree '{}' is missing at {}", name, p.display()));
    }
    Ok(p)
}

/// No worktree -> repo workdir. Broken binding -> (workdir, Some(err)) (caller fails closed).
pub fn task_worktree_cwd(workdir: &Path, task: &Task) -> (PathBuf, Option<String>) {
    let Some(name) = &task.worktree else {
        return (workdir.to_path_buf(), None);
    };
    match registered_worktree(workdir, name) {
        Ok(p) => (p, None),
        Err(e) => (workdir.to_path_buf(), Some(e)),
    }
}

/// Create + bind a task worktree (Lead only). Task must be pending and unowned.
/// Reports partial operations explicitly if git succeeds but binding fails.
pub fn create_worktree(team: &TeamCtx, name: &str, task_id: &str) -> String {
    if let Some(e) = validate_worktree_name(name) {
        return format!("Error: {}", e);
    }
    let worktrees_dir = team.workdir.join(".worktrees");
    // Create the worktrees dir first so safe_path_in (inside worktree_path) can
    // canonicalize the base; it doesn't exist yet on first create.
    let _ = std::fs::create_dir_all(&worktrees_dir);
    let path = match worktree_path(&worktrees_dir, name) {
        Ok(p) => p,
        Err(e) => return format!("Error: {}", e),
    };
    let path_str = path.to_string_lossy().into_owned();
    let branch = worktree_branch(name);
    let _g = match team.lock.lock() {
        Ok(g) => g,
        Err(e) => return format!("Error: {}", e),
    };
    let store = &team.task_store;
    let Ok(mut task) = store.load(task_id) else {
        return format!("Error: Task {} not found", task_id);
    };
    if task.status != TaskStatus::Pending || task.owner.is_some() {
        return format!("Error: Task {} must be pending and unowned", task_id);
    }
    if task.worktree.is_some() {
        return format!(
            "Error: Task {} already uses worktree '{}'",
            task_id,
            task.worktree.as_deref().unwrap_or("?")
        );
    }
    for t in store.list().unwrap_or_default() {
        if t.id != task_id && t.worktree.as_deref() == Some(name) {
            return format!("Error: Worktree '{}' already bound to another task", name);
        }
    }
    if path.exists() {
        return format!("Error: Worktree path already exists: {}", path.display());
    }

    let (ok, out) = run_git(&["rev-parse", "--show-toplevel"], &team.workdir);
    let toplevel_ok =
        ok && dunce::canonicalize(out.trim()).ok() == dunce::canonicalize(&team.workdir).ok();
    if !toplevel_ok {
        return "Error: Working directory must be the root of a Git repository".into();
    }
    let (ok, o) = run_git(&["check-ref-format", "--branch", &branch], &team.workdir);
    if !ok {
        return format!("Error: Invalid worktree branch '{}': {}", branch, o);
    }
    let ref_arg = format!("refs/heads/{}", branch);
    let (exists, _) = run_git(&["show-ref", "--verify", "--quiet", &ref_arg], &team.workdir);
    if exists {
        return format!("Error: Branch '{}' already exists", branch);
    }

    let (ok, result) = run_git(
        &["worktree", "add", "-b", &branch, &path_str, "HEAD"],
        &team.workdir,
    );
    if !ok {
        let mut artifacts = vec![];
        if path.exists() {
            artifacts.push(format!("checkout path '{}'", path.display()));
        }
        let (be, _) = run_git(&["show-ref", "--verify", "--quiet", &ref_arg], &team.workdir);
        if be {
            artifacts.push(format!("branch '{}'", branch));
        }
        if !artifacts.is_empty() {
            return format!(
                "Partial operation: git worktree add failed leaving {}. Task {} remains unbound; inspect '{}' and '{}'. Git error: {}",
                artifacts.join(", "),
                task_id,
                path.display(),
                branch,
                result
            );
        }
        return format!("Git error: {}", result);
    }
    task.worktree = Some(name.into());
    if let Err(e) = store.save(&task) {
        return format!(
            "Partial success: Worktree '{}' created at {} on '{}', but binding failed: {}. Git data retained for manual recovery.",
            name,
            path.display(),
            branch,
            e
        );
    }
    format!("Worktree '{}' created at {} for task {}", name, path.display(), task_id)
}

/// Host-side removal of a task worktree. Retains the branch for manual recovery.
pub fn remove_worktree(team: &TeamCtx, name: &str, discard_changes: bool) -> String {
    if let Some(e) = validate_worktree_name(name) {
        return format!("Error: {}", e);
    }
    let _g = match team.lock.lock() {
        Ok(g) => g,
        Err(e) => return format!("Error: {}", e),
    };
    let workdir = &team.workdir;
    let path = match registered_worktree(workdir, name) {
        Ok(p) => p,
        Err(e) => return format!("Error: {}", e),
    };
    let bound: Vec<Task> = team
        .task_store
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.worktree.as_deref() == Some(name))
        .collect();
    if bound.is_empty() {
        return format!("Error: Worktree '{}' is not bound to a task", name);
    }
    if let Some(t) = bound.iter().find(|t| t.status != TaskStatus::Completed) {
        return format!(
            "Error: Worktree '{}' bound to active task {}; complete it first",
            name, t.id
        );
    }
    let leased: Vec<String> = team
        .assignments
        .snap()
        .into_iter()
        .filter(|(_, a)| dunce::canonicalize(&a.cwd).ok() == dunce::canonicalize(&path).ok())
        .map(|(o, _)| o)
        .collect();
    if !leased.is_empty() {
        return format!(
            "Error: Worktree '{}' still in use by {}",
            name,
            leased.join(", ")
        );
    }
    let (ok, status) = run_git(&["status", "--porcelain", "--ignored"], &path);
    if !ok {
        return format!("Error: Cannot verify worktree '{}' status: {}", name, status);
    }
    if !status.trim().is_empty() && !discard_changes {
        let changed = status.lines().filter(|l| !l.trim().is_empty()).count();
        return format!(
            "Error: Worktree '{}' has {} uncommitted change(s); preserve or discard them manually",
            name, changed
        );
    }
    let mut argv = vec!["worktree".to_string(), "remove".to_string()];
    if discard_changes {
        argv.push("--force".to_string());
    }
    argv.push(path.to_string_lossy().to_string());
    let (ok, result) = run_git_owned(&argv, workdir);
    if !ok {
        return format!("Git error: {}", result);
    }
    for mut t in bound {
        t.worktree = None;
        let _ = team.task_store.save(&t);
    }
    format!(
        "Worktree '{}' removed; branch '{}' retained",
        name,
        worktree_branch(name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn validate_rejects_bad_names() {
        assert!(validate_worktree_name("../x").is_some());
        assert!(validate_worktree_name(".hidden").is_some()); // must start [A-Za-z0-9]
        assert!(validate_worktree_name(&"a".repeat(65)).is_some());
        assert!(validate_worktree_name("auth-refactor").is_none());
        assert!(validate_worktree_name("a.b-c_d").is_none());
    }

    #[test]
    fn task_worktree_cwd_no_binding_returns_workdir() {
        let tmp = TempDir::new().unwrap();
        let task = Task {
            id: "task_12345678".into(),
            subject: "s".into(),
            description: "".into(),
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: vec![],
            worktree: None,
        };
        let (cwd, err) = task_worktree_cwd(tmp.path(), &task);
        assert_eq!(cwd, tmp.path());
        assert!(err.is_none());
    }

    #[test]
    fn task_worktree_cwd_broken_binding_errors() {
        let tmp = TempDir::new().unwrap();
        let task = Task {
            id: "task_12345678".into(),
            subject: "s".into(),
            description: "".into(),
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: vec![],
            worktree: Some("missing".into()),
        };
        let (cwd, err) = task_worktree_cwd(tmp.path(), &task);
        assert!(err.is_some(), "broken worktree binding must produce an error");
        // not registered with git (tmp not even a repo) -> err
        let _ = cwd;
    }

    #[test]
    fn create_worktree_rejects_non_git_workdir() {
        let tmp = TempDir::new().unwrap();
        let store = std::sync::Arc::new(crate::task_system::store::create_test_store(tmp.path()));
        let team = crate::team::TeamCtx::new(tmp.path().to_path_buf(), store).unwrap();
        let t = team.task_store.create("T".into(), "".into(), vec![]).unwrap();
        let r = create_worktree(&team, "auth", &t.id);
        assert!(
            r.contains("must be the root of a Git repository"),
            "non-git workdir must be rejected, got {}",
            r
        );
    }

    #[test]
    fn create_worktree_rejects_non_pending_task() {
        // Build a tiny git repo in a tempdir.
        let tmp = TempDir::new().unwrap();
        let _ = std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "t@t.t"])
            .current_dir(tmp.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(tmp.path())
            .output();
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(tmp.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(tmp.path())
            .output();
        let store = std::sync::Arc::new(crate::task_system::store::create_test_store(tmp.path()));
        let team = crate::team::TeamCtx::new(tmp.path().to_path_buf(), store).unwrap();
        let mut t = team.task_store.create("T".into(), "".into(), vec![]).unwrap();
        t.status = TaskStatus::InProgress;
        t.owner = Some("alice".into());
        team.task_store.save(&t).unwrap();
        let r = create_worktree(&team, "auth", &t.id);
        assert!(
            r.contains("must be pending and unowned"),
            "non-pending must be rejected, got {}",
            r
        );
    }
}
