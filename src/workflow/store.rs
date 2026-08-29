//! On-disk store for runs: snapshots, outputs, journals, locks.
//!
//! Atomic temp+rename persist, `create_new` to reserve run ids, fs4
//! cross-process locks (works on Windows too).

use crate::workflow::{ids, WorkflowError};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(workdir: &Path) -> Result<Self, WorkflowError> {
        let dir = workdir.join(".workflows");
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn snapshot_path(&self, run_id: &str) -> PathBuf {
        self.dir.join(format!("{run_id}.json"))
    }
    pub fn output_path(&self, run_id: &str) -> PathBuf {
        self.dir.join(format!("{run_id}.output.json"))
    }
    pub fn journal_path(&self, run_id: &str) -> PathBuf {
        self.dir.join(format!("{run_id}.journal.jsonl"))
    }
    pub fn lock_path(&self, run_id: &str) -> PathBuf {
        self.dir.join(format!("{run_id}.lock"))
    }

    /// Atomically reserve a fresh run identity before any journal is written.
    pub fn reserve_run_id(&self, name: &str) -> Result<String, WorkflowError> {
        for _ in 0..32 {
            let run_id = ids::create_run_id(name);
            let path = self.snapshot_path(&run_id);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(run_id),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(WorkflowError::Validation(
            "could not allocate a unique runId".into(),
        ))
    }

    /// Atomic JSON write: temp file suffixed with PID, then rename.
    pub fn write_json_atomic(&self, path: &Path, value: &Value) -> Result<(), WorkflowError> {
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        let json = serde_json::to_string_pretty(value)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn read_snapshot(&self, run_id: &str) -> Result<Value, WorkflowError> {
        let path = self.snapshot_path(run_id);
        if !path.exists() {
            return Err(WorkflowError::SnapshotNotFound(run_id.into()));
        }
        let text = std::fs::read_to_string(&path)?;
        let v: Value = serde_json::from_str(&text)?;
        Ok(v)
    }
}

/// Cross-process exclusive lock on a run (fs4). Held for the run lifecycle so
/// two host processes cannot resume the same run simultaneously.
pub struct RunLock {
    _file: std::fs::File,
}

impl RunLock {
    pub fn try_acquire(lock_path: &Path) -> Result<Self, WorkflowError> {
        use fs4::fs_std::FileExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(lock_path)?;
        file.try_lock_exclusive()
            .map_err(|_| WorkflowError::RunActive(lock_path.to_string_lossy().into()))?;
        Ok(Self { _file: file })
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn store() -> (TempDir, Store) {
        let td = TempDir::new().unwrap();
        let s = Store::new(td.path()).unwrap();
        (td, s)
    }

    #[test]
    fn reserve_is_exclusive() {
        let (_td, s) = store();
        let id = s.reserve_run_id("review-changes").unwrap();
        // `create_new` makes the exact id non-reservable again; a second call
        // yields a different id.
        let id2 = s.reserve_run_id("review-changes").unwrap();
        assert_ne!(id, id2);
        assert!(s.snapshot_path(&id).exists());
    }

    #[test]
    fn atomic_write_roundtrips() {
        let (_td, s) = store();
        let p = s.dir.join("snap.json");
        s.write_json_atomic(&p, &json!({"a":1})).unwrap();
        let read: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(read, json!({"a":1}));
    }

    #[test]
    fn read_snapshot_missing_errors() {
        let (_td, s) = store();
        assert!(matches!(
            s.read_snapshot("wf_x_0123456789abcdef"),
            Err(WorkflowError::SnapshotNotFound(_))
        ));
    }

    #[test]
    fn run_lock_excludes_second_acquirer() {
        let td = TempDir::new().unwrap();
        let lp = td.path().join("x.lock");
        let _g = RunLock::try_acquire(&lp).unwrap();
        let second = RunLock::try_acquire(&lp);
        assert!(matches!(second, Err(WorkflowError::RunActive(_))));
    }
}
