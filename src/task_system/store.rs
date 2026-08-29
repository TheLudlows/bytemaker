/*
store.rs - TaskStore for file persistence

Manages .tasks/ directory, file I/O, and task persistence.
*/

use thiserror::Error;
use regex::Regex;
use std::path::PathBuf;
use std::env;
use std::fs::create_dir_all;
use fastrand;
use path_clean::PathClean;
use dunce;

use crate::task_system::task::{Task, TaskStatus};

/// TaskStore error types
#[derive(Error, Debug)]
pub enum TaskStoreError {
    #[error("Invalid task ID: {0}")]
    InvalidId(String),

    #[error("Task not found: {0}")]
    NotFound(String),

    #[error("Task store escapes workspace")]
    EscapesWorkspace,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Invalid task status: {0}")]
    InvalidStatus(String),
}

/// TaskStore manages task persistence in the file system
#[derive(Debug)]
pub struct TaskStore {
    directory: PathBuf,
    id_pattern: Regex,
}

impl TaskStore {
    const MAX_ID_RETRIES: usize = 100;

    /// Creates a new TaskStore instance
    ///
    /// Security measures:
    /// - Uses dunce to normalize paths (removes Windows \\?\ prefix)
    /// - Uses path-clean for lexical normalization (no filesystem access)
    /// - Checks for directory traversal before creating directories
    /// - Verifies .tasks directory is not a symlink (Unix only)
    pub fn new(directory: PathBuf) -> Result<Self, TaskStoreError> {
        tracing::info!("Creating new task store path: {}", directory.display());

        let workdir = dunce::canonicalize(env::current_dir()?)
            .map_err(|_| TaskStoreError::EscapesWorkspace)?;

        // Normalize input path with dunce (removes \\?\ prefix on Windows)
        // If path doesn't exist, use lexical normalization
        let directory = if directory.exists() {
            dunce::canonicalize(&directory)
                .map_err(|_| TaskStoreError::EscapesWorkspace)?
        } else {
            directory.clean()
        };

        // For relative paths, join with workdir; for absolute paths, use directly
        let normalized = if directory.is_absolute() {
            directory
        } else {
            workdir.join(&directory).clean()
        };

        // Check for path traversal before creating anything
        if !normalized.starts_with(&workdir) {
            return Err(TaskStoreError::EscapesWorkspace);
        }

        // Create .tasks directory
        let tasks_dir = normalized.join(".tasks");
        create_dir_all(&tasks_dir)?;

        // Ensure .tasks directory is not a symlink (Unix only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            if let Ok(meta) = std::fs::symlink_metadata(&tasks_dir) {
                if meta.file_type().is_symlink() {
                    return Err(TaskStoreError::EscapesWorkspace);
                }
            }
        }

        Ok(Self {
            directory: tasks_dir,
            id_pattern: Regex::new(r"^task_[0-9a-f]{8}$")
                .map_err(|_| TaskStoreError::InvalidId("regex".into()))?,
        })
    }

    /// Gets the file path for a task
    fn task_path(&self, task_id: &str) -> Result<PathBuf, TaskStoreError> {
        if !self.id_pattern.is_match(task_id) {
            return Err(TaskStoreError::InvalidId(task_id.to_string()));
        }

        Ok(self.directory.join(format!("{}.json", task_id)))
    }

    /// The on-disk `.tasks` directory this store manages (s13: for TaskStoreLock).
    pub fn directory(&self) -> &std::path::Path {
        &self.directory
    }

    /// Checks if a task exists
    pub fn exists(&self, task_id: &str) -> bool {
        self.task_path(task_id)
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    /// Creates a new task
    pub fn create(
        &self,
        subject: String,
        description: String,
        blocked_by: Vec<String>,
    ) -> Result<Task, TaskStoreError> {
        let subject = subject.trim().to_string();
        if subject.is_empty() {
            return Err(TaskStoreError::InvalidId("empty subject".into()));
        }

        // Deduplicate dependency list
        let mut unique_deps = Vec::new();
        for dep in &blocked_by {
            if !unique_deps.contains(dep) {
                unique_deps.push(dep.clone());
            }
        }

        // Validate dependencies exist
        for dep in &unique_deps {
            if !self.exists(dep) {
                return Err(TaskStoreError::NotFound(dep.clone()));
            }
        }

        // Create directory
        std::fs::create_dir_all(&self.directory)?;

        // Generate unique ID (max 100 retries)
        for _ in 0..Self::MAX_ID_RETRIES {
            let id = format!("task_{:08x}", fastrand::u32(..));
            let path = self.task_path(&id)?;

            // Atomic write: use create_new to avoid overwriting
            match std::fs::File::options()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_file) => {
                    let task = Task {
                        id: id.clone(),
                        subject: subject.clone(),
                        description: description.clone(),
                        status: TaskStatus::Pending,
                        owner: None,
                        blocked_by: unique_deps.clone(),
                        worktree: None,
                    };

                    let content = serde_json::to_string_pretty(&task)?;
                    std::fs::write(&path, content)?;
                    return Ok(task);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    continue;
                }
                Err(e) => return Err(TaskStoreError::Io(e)),
            }
        }

        Err(TaskStoreError::InvalidId("failed to allocate unique ID".into()))
    }

    pub fn load(&self, task_id: &str) -> Result<Task, TaskStoreError> {
        let path = self.task_path(task_id)?;
        let content = std::fs::read_to_string(&path)?;
        let task: Task = serde_json::from_str(&content)?;

        // Validate ID match
        if task.id != task_id {
            return Err(TaskStoreError::InvalidId(format!(
                "ID mismatch: file={}, loaded={}", task_id, task.id
            )));
        }

        Ok(task)
    }

    pub fn save(&self, task: &Task) -> Result<(), TaskStoreError> {
        let path = self.task_path(&task.id)?;
        let content = serde_json::to_string_pretty(task)?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<Task>, TaskStoreError> {
        // Ensure directory still exists and is not a symlink (Unix only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            if let Ok(meta) = std::fs::symlink_metadata(&self.directory) {
                if meta.file_type().is_symlink() {
                    return Err(TaskStoreError::EscapesWorkspace);
                }
            }
        }

        if !self.directory.exists() {
            return Ok(Vec::new());
        }

        let mut tasks = Vec::new();
        for entry in std::fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if name_str.ends_with(".json") && self.id_pattern.is_match(name_str.strip_suffix(".json").unwrap()) {
                let task_id = name_str.trim_end_matches(".json");
                match self.load(task_id) {
                    Ok(task) => tasks.push(task),
                    Err(_) => continue, // skip corrupted tasks
                }
            }
        }

        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(tasks)
    }
}

/// Test-only: build a TaskStore in the given dir, bypassing workspace validation
/// (temp dirs fall outside the workspace). `tools` tests reuse this helper.
#[cfg(test)]
pub(crate) fn create_test_store(dir: &std::path::Path) -> TaskStore {
    let store_dir = dir.join("tasks");
    std::fs::create_dir_all(&store_dir).unwrap();
    TaskStore {
        directory: store_dir,
        id_pattern: regex::Regex::new(r"^task_[0-9a-f]{8}$").unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_new_validates_workspace() {
        let workdir = env::current_dir().unwrap().canonicalize().unwrap();
        let store = TaskStore::new(workdir.clone());
        assert!(store.is_ok());

        // Try to use outside workspace
        let outside = PathBuf::from("/etc");
        let store = TaskStore::new(outside);
        assert!(matches!(store, Err(TaskStoreError::EscapesWorkspace)));
    }

    #[test]
    fn test_task_path_validates_id_format() {
        let workdir = env::current_dir().unwrap();
        let store = create_test_store(&workdir);

        // Valid ID
        let path = store.task_path("task_12345678");
        assert!(path.is_ok());

        // Invalid IDs
        assert!(matches!(
            store.task_path("invalid"),
            Err(TaskStoreError::InvalidId(_))
        ));
        assert!(matches!(
            store.task_path("task_123"),
            Err(TaskStoreError::InvalidId(_))
        ));
        assert!(matches!(
            store.task_path("task_123456789"),
            Err(TaskStoreError::InvalidId(_))
        ));
    }

    #[test]
    fn test_exists_returns_false_for_nonexistent() {
        let workdir = env::current_dir().unwrap();
        let store = create_test_store(&workdir);
        assert!(!store.exists("task_12345678"));
    }

    #[test]
    fn test_create_creates_task_file() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());

        let task = store.create(
            "Test task".to_string(),
            "Test description".to_string(),
            vec![],
        ).unwrap();

        assert!(task.id.starts_with("task_"));
        assert_eq!(task.subject, "Test task");
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.owner.is_none());
        assert!(task.blocked_by.is_empty());

        // Verify file exists
        assert!(store.exists(&task.id));
    }

    #[test]
    fn test_create_rejects_empty_subject() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());

        let result = store.create(
            "".to_string(),
            "".to_string(),
            vec![],
        );
        assert!(matches!(result, Err(TaskStoreError::InvalidId(_))));
    }

    #[test]
    fn test_create_validates_dependencies_exist() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());

        let result = store.create(
            "Dependent task".to_string(),
            "".to_string(),
            vec!["task_nonexistent".to_string()],
        );
        assert!(matches!(result, Err(TaskStoreError::NotFound(_))));
    }

    #[test]
    fn test_create_deduplicates_dependencies() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());

        let dep = store.create("Dependency".to_string(), "".to_string(), vec![]).unwrap();

        let task = store.create(
            "Task".to_string(),
            "".to_string(),
            vec![dep.id.clone(), dep.id.clone()],
        ).unwrap();

        assert_eq!(task.blocked_by.len(), 1);
        assert_eq!(task.blocked_by[0], dep.id);
    }

    #[test]
    fn test_load_retrieves_task() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());

        let created = store.create(
            "Original".to_string(),
            "Description".to_string(),
            vec![],
        ).unwrap();

        let loaded = store.load(&created.id).unwrap();
        assert_eq!(loaded.id, created.id);
        assert_eq!(loaded.subject, "Original");
    }

    #[test]
    fn test_load_validates_id_match() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());

        let task = store.create("Test".to_string(), "".to_string(), vec![]).unwrap();

        // Corrupt the file ID
        let path = store.task_path(&task.id).unwrap();
        let mut data = std::fs::read_to_string(&path).unwrap();
        data = data.replace(&task.id, "task_wrongid");
        std::fs::write(&path, data).unwrap();

        let result = store.load(&task.id);
        assert!(matches!(result, Err(TaskStoreError::InvalidId(_))));
    }

    #[test]
    fn test_save_persists_changes() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());

        let mut task = store.create("Test".to_string(), "".to_string(), vec![]).unwrap();
        task.status = TaskStatus::Completed;
        task.owner = Some("agent".to_string());

        store.save(&task).unwrap();

        let loaded = store.load(&task.id).unwrap();
        assert_eq!(loaded.status, TaskStatus::Completed);
        assert_eq!(loaded.owner, Some("agent".to_string()));
    }

    #[test]
    fn test_list_returns_empty_for_no_tasks() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());

        let tasks = store.list().unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn test_list_returns_all_tasks_sorted() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());

        let t1 = store.create("First".to_string(), "".to_string(), vec![]).unwrap();
        let t2 = store.create("Second".to_string(), "".to_string(), vec![]).unwrap();
        let t3 = store.create("Third".to_string(), "".to_string(), vec![]).unwrap();

        let tasks = store.list().unwrap();
        assert_eq!(tasks.len(), 3);

        // Collect all IDs
        let ids = vec![t1.id.clone(), t2.id.clone(), t3.id.clone()];
        let mut sorted_ids = ids.clone();
        sorted_ids.sort();

        // Tasks should be sorted by ID
        for (i, task) in tasks.iter().enumerate() {
            assert_eq!(task.id, sorted_ids[i]);
        }
    }

    #[test]
    fn test_list_skips_corrupted_files() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());

        let valid = store.create("Valid".to_string(), "".to_string(), vec![]).unwrap();

        // Create corrupted file
        let corrupted_path = store.directory.join("task_deadbeef.json");
        std::fs::write(&corrupted_path, "invalid json").unwrap();

        let tasks = store.list().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, valid.id);
    }
}