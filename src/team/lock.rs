use std::path::{Path, PathBuf};
use std::sync::Mutex;
use fs4::fs_std::FileExt;

/// Serializes task mutations across threads (in-process `Mutex`) and host
/// processes (fs4 exclusive lock on `.tasks/.lock`).
pub struct TaskStoreLock {
    inner: Mutex<()>,
    lock_path: PathBuf,
}

impl TaskStoreLock {
    pub fn new(tasks_dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(tasks_dir)?;
        Ok(Self {
            inner: Mutex::new(()),
            lock_path: tasks_dir.join(".lock"),
        })
    }

    /// Acquire both locks for the duration of the returned guard's lifetime.
    pub fn lock(&self) -> std::io::Result<TaskStoreGuard<'_>> {
        let guard = self.inner.lock().map_err(|e| std::io::Error::other(e.to_string()))?;
        let file = std::fs::OpenOptions::new()
            .read(true).write(true).create(true)
            .open(&self.lock_path)?;
        file.lock_exclusive()?;
        Ok(TaskStoreGuard { guard, file })
    }
}

pub struct TaskStoreGuard<'a> {
    #[allow(dead_code)]
    guard: std::sync::MutexGuard<'a, ()>,
    file: std::fs::File,
}

impl Drop for TaskStoreGuard<'_> {
    fn drop(&mut self) {
        let _ = fs4::fs_std::FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use tempfile::TempDir;

    #[test]
    fn lock_is_mutually_exclusive_across_threads() {
        let tmp = TempDir::new().unwrap();
        let lk = TaskStoreLock::new(tmp.path()).unwrap();
        let g1 = lk.lock().unwrap();
        let lk2 = TaskStoreLock::new(tmp.path()).unwrap();
        let child = thread::spawn(move || {
            let _g2 = lk2.lock().unwrap();
            true
        });
        // give the child a moment to block on the lock
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!child.is_finished(), "second lock must block while first is held");
        drop(g1);
        assert!(child.join().unwrap(), "second lock acquires after first drops");
    }

    #[test]
    fn worktree_field_defaults_none_for_old_json() {
        let json = r#"{"id":"task_12345678","subject":"s","description":"","status":"pending","owner":null,"blocked_by":[]}"#;
        let t: crate::task_system::task::Task = serde_json::from_str(json).unwrap();
        assert!(t.worktree.is_none());
    }
}
