//! tools — tool system (s02).
//!
//! Module hub: submodule declarations, re-exports, shared path utilities (`workdir`,
//! `safe_path_in`, `ctx_cwd`, `escapes_workspace_lexical`, `normalize`), and `build_registry()`.
//! `safe_path_in`: lexical `path-clean` + `dunce::canonicalize` comparison; valid for not-yet-existing paths. Details: `docs/modules/tools.md`.

// Core modules
pub mod trait_def;
pub mod registry;

// Tool module implementations
pub mod command;
pub mod read_file;
pub mod write_file;
pub mod edit_file;
pub mod glob_tool;
pub mod load_skill;
pub mod todo_write;
pub mod task;
pub mod workflow;

// Re-exports for convenient access
pub use self::registry::ToolRegistry;
pub use self::trait_def::{PermissionCheck, Tool, ToolContext, ToolResult};

use std::env;
use std::path::{Path, PathBuf};
use path_clean::PathClean;

/// Working directory.
pub fn workdir() -> PathBuf {
    env::current_dir().unwrap_or_else(|_| ".".into())
}

/// Resolve the caller's working directory from a ToolContext (s13).
///
/// Lead/subagent -> repo workdir; teammate with an active assignment -> the
/// task's cwd; teammate with no assignment -> Err("Claim a Task...").
pub fn ctx_cwd(ctx: &ToolContext<'_>) -> Result<PathBuf, String> {
    ctx.cwd()
}

/// Path-safety check: ensure the path stays within the workdir.
///
/// `path-clean` lexical normalization (resolves `..`/`.`, no filesystem) + `dunce::canonicalize`
/// for escape comparison (strips the Windows `\\?\` prefix, expands 8.3 short names so forms are
/// `starts_with`-comparable). Works for not-yet-existing paths (`write_file` new files/dirs aren't rejected).
pub fn safe_path_in(workdir: &Path, path_str: &str) -> Result<PathBuf, String> {
    // Canonicalize the workdir: strip `\\?\` prefix, expand short names, resolve symlinks.
    // The workdir always exists, so this won't fail.
    let workdir_canonical = dunce::canonicalize(workdir)
        .map_err(|e| format!("Error: {}", e))?;

    // Lexical normalization: base.join(path) then path-clean resolves `..`/`.`, no filesystem.
    // Note: an absolute path_str makes join replace base — expected (absolute paths aren't relative to base).
    let norm = workdir_canonical.join(path_str).clean();

    // Escape check: compare dunce canonical forms.
    // If no ancestor can be canonicalized, fail closed (safe side).
    let within = match canonical_form_of(&norm) {
        Some(c) => c.starts_with(&workdir_canonical),
        None => false,
    };
    if !within {
        return Err(format!(
            "Error: path escapes workspace {:?}, {:?}",
            workdir_canonical, norm
        ));
    }

    // Return canonical form for existing paths (resolves symlinks/junctions, strips `\\?\`);
    // pass through the lexical result for not-yet-existing paths.
    if norm.exists() {
        dunce::canonicalize(&norm).map_err(|e| format!("Error: {}", e))
    } else {
        Ok(norm)
    }
}

/// Normalize a path to canonical form for escape comparison.
///
/// - Path exists: `dunce::canonicalize()` (strips `\\?\` prefix).
/// - Path doesn't exist: walk up to the first existing ancestor, canonicalize, re-append the tail.
/// - Root can't be canonicalized: return `None` (caller fails closed).
fn canonical_form_of(path: &Path) -> Option<PathBuf> {
    // Fast path: the path itself exists.
    if let Ok(c) = dunce::canonicalize(path) {
        return Some(c);
    }
    // Path doesn't exist: walk up to the first existing ancestor, canonicalize, re-append the tail.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        match dunce::canonicalize(&cur) {
            Ok(canon) => {
                let mut full = canon;
                for seg in tail.into_iter().rev() {
                    full.push(seg);
                }
                return Some(full);
            }
            Err(_) => {
                // cur doesn't exist: record this segment and keep walking up.
                let name = cur.file_name()?.to_owned();
                tail.push(name);
                cur = cur.parent()?.to_path_buf();
            }
        }
    }
}

/// Path-safety check: ensure the path is within the current workdir.
pub fn safe_path(path_str: &str) -> Result<PathBuf, String> {
    safe_path_in(&workdir(), path_str)
}

/// Lexically check whether a path may escape the workspace (no filesystem access).
///
/// Lightweight variant of `safe_path_in` for fast checks on not-yet-existing paths:
/// lexical `path-clean` normalization, then check it still starts with the workdir.
pub fn escapes_workspace_lexical(path_str: &str) -> bool {
    let workdir = workdir();
    let norm = workdir.join(path_str).clean();
    !norm.starts_with(&workdir)
}

/// Lexically normalize a path (resolves `..`/`.`, no filesystem access).
///
/// Returns a path without `.` or `..` via `path-clean`'s `PathClean::clean()`.
pub fn normalize(path_str: &str) -> PathBuf {
    Path::new(path_str).clean()
}

/// Build and return a tool registry with all tools registered.
///
/// Built-in tools register via `Box::new(...)` (`register` converts to `Arc`); MCP tools mount
/// at runtime via `register_dynamic(Arc::new(...))` (see `agent.rs::new`). `compact` is absent —
/// compaction is driven directly by `agent::run_loop` via `ContextCompactor`, not via tool dispatch.
pub fn build_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    // File operations
    registry.register(Box::new(command::CommandTool));
    registry.register(Box::new(read_file::ReadFileTool));
    registry.register(Box::new(write_file::WriteFileTool));
    registry.register(Box::new(edit_file::EditFileTool));

    // Search utilities
    registry.register(Box::new(glob_tool::GlobTool));

    // Skill management
    registry.register(Box::new(load_skill::LoadSkillTool));

    // Todo management
    registry.register(Box::new(todo_write::TodoWriteTool));

    // Task delegation (s06)
    registry.register(Box::new(task::TaskTool));

    // Workflow runtime (s16)
    registry.register(Box::new(workflow::WorkflowTool));

    // Task system tools (s06)
    registry.register(Box::new(crate::task_system::CreateTaskTool));
    registry.register(Box::new(crate::task_system::ListTasksTool));
    registry.register(Box::new(crate::task_system::GetTaskTool));
    registry.register(Box::new(crate::task_system::ClaimTaskTool));
    registry.register(Box::new(crate::task_system::CompleteTaskTool));

    // Background task tools (s07)
    registry.register(Box::new(crate::background_tasks::TaskOutputTool));
    registry.register(Box::new(crate::background_tasks::TaskStopTool));

    // Cron scheduler tools (s09)
    registry.register(Box::new(crate::cron_scheduler::ScheduleCronTool));
    registry.register(Box::new(crate::cron_scheduler::ListCronsTool));
    registry.register(Box::new(crate::cron_scheduler::CancelCronTool));

    // Team coordination tools (s13)
    registry.register(Box::new(crate::team::tools::SpawnTeammateTool));
    registry.register(Box::new(crate::team::tools::ListTeammatesTool));
    registry.register(Box::new(crate::team::tools::SendMessageTool));
    registry.register(Box::new(crate::team::tools::RequestShutdownTool));
    registry.register(Box::new(crate::team::tools::RequestPlanTool));
    registry.register(Box::new(crate::team::tools::ReviewPlanTool));
    registry.register(Box::new(crate::team::tools::SubmitPlanTool));
    registry.register(Box::new(crate::team::tools::CreateWorktreeTool));

    registry
}

#[cfg(test)]
mod safe_path_tests {
    use super::safe_path_in;
    use std::fs;

    #[test]
    fn allows_nonexistent_path_for_new_file() {
        // Regression: write_file new files/dirs target a not-yet-existing path;
        // safe_path must not reject it just because canonicalize fails.
        let dir = std::env::temp_dir().join("bytemaker-safe-path-nonexistent");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dir_canon = dunce::canonicalize(&dir).unwrap();

        let got = safe_path_in(&dir, "subdir/newfile.txt");
        assert!(got.is_ok(), "non-existent path should be allowed: {:?}", got);
        let abs = got.unwrap();
        assert!(abs.starts_with(&dir_canon), "{:?} should be under base", abs);
        assert_eq!(abs.file_name(), Some(std::ffi::OsStr::new("newfile.txt")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_escape_via_dotdot() {
        let dir = std::env::temp_dir().join("bytemaker-safe-path-escape");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let got = safe_path_in(&dir, "../secret.txt");
        assert!(got.is_err(), "path escaping workspace must be rejected");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_path_canonicalized_and_allowed() {
        let dir = std::env::temp_dir().join("bytemaker-safe-path-existing");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("real.txt");
        fs::write(&file, b"hi").unwrap();

        let got = safe_path_in(&dir, "real.txt");
        assert!(got.is_ok(), "existing in-workspace path should be allowed: {:?}", got);
        assert_eq!(
            dunce::canonicalize(got.unwrap()).unwrap(),
            dunce::canonicalize(&file).unwrap()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn absolute_path_in_workspace_is_allowed() {
        // Regression: callers often pass absolute paths. join replaces base and drops the `\\?\`
        // verbatim prefix; temp_dir may use 8.3 short names. Both prevent direct starts_with
        // comparison between lexical norm and canonical base — compare canonical-to-canonical and allow.
        let dir = std::env::temp_dir().join("bytemaker-safe-path-abs");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("inner.txt");
        fs::write(&file, b"x").unwrap();

        let abs = file.to_string_lossy().to_string();
        let got = safe_path_in(&dir, &abs);
        assert!(got.is_ok(), "absolute in-workspace path should be allowed: {:?}", got);
        // Existing paths should return the canonical form (dunce strips `\\?\`), matching dunce::canonicalize.
        assert_eq!(got.unwrap(), dunce::canonicalize(&file).unwrap());

        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod team_visibility_tests {
    use super::build_registry;
    use crate::tools::trait_def::AgentKind;

    #[test]
    fn teammate_tool_set_excludes_lead_tools() {
        let reg = build_registry();
        let defs = reg.definitions_for(AgentKind::Teammate);
        let teammate_names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        // Lead-only coordination tools + cron/bg tools must be withheld from teammates.
        for lead_only in [
            "spawn_teammate",
            "request_shutdown",
            "request_plan",
            "review_plan",
            "create_worktree",
            "schedule_cron",
            "task_output",
            "task_stop",
        ] {
            assert!(
                !teammate_names.contains(&lead_only),
                "teammate must not see {}, got {:?}",
                lead_only,
                teammate_names
            );
        }
        // Teammate-visible tools.
        assert!(teammate_names.contains(&"submit_plan"));
        assert!(teammate_names.contains(&"claim_task"));
        assert!(teammate_names.contains(&"complete_task"));
        assert!(teammate_names.contains(&"command"));
    }

    #[test]
    fn lead_tool_set_excludes_submit_plan() {
        let reg = build_registry();
        let defs = reg.definitions_for(AgentKind::Lead);
        let lead_names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(lead_names.contains(&"spawn_teammate"));
        assert!(
            !lead_names.contains(&"submit_plan"),
            "Lead must not see submit_plan, got {:?}",
            lead_names
        );
    }
}
