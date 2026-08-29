//! Append-only journal; resume replays cached agent() calls by stable key.
//!
//! On resume, an `agent()` whose semantic key is already in the journal is
//! served from cache instead of re-run. The key is a content hash that does
//! NOT depend on concurrency order (mirrors s16 WorkflowJournal).

use crate::workflow::ids;
use crate::workflow::store::Store;
use crate::workflow::WorkflowError;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

pub struct WorkflowJournal {
    file: std::fs::File,
    inner: Mutex<JournalInner>,
}

struct JournalInner {
    cache: HashMap<String, Value>,
}

impl WorkflowJournal {
    pub fn new(store: &Store, run_id: &str, resume: bool) -> Result<Self, WorkflowError> {
        let path = store.journal_path(run_id);
        let mut cache = HashMap::new();
        let file = if resume {
            if !path.exists() {
                return Err(WorkflowError::JournalCorrupt(format!(
                    "journal not found for {run_id}"
                )));
            }
            for (lineno, line) in std::fs::read_to_string(&path)?.lines().enumerate() {
                let rec: Value = serde_json::from_str(line).map_err(|e| {
                    WorkflowError::JournalCorrupt(format!("line {}: {e}", lineno + 1))
                })?;
                let key = rec
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        WorkflowError::JournalCorrupt(format!(
                            "line {}: missing key",
                            lineno + 1
                        ))
                    })?;
                let value = rec.get("value").cloned().ok_or_else(|| {
                    WorkflowError::JournalCorrupt(format!(
                        "line {}: missing value",
                        lineno + 1
                    ))
                })?;
                cache.insert(key.to_string(), value);
            }
            std::fs::OpenOptions::new().append(true).open(&path)?
        } else {
            std::fs::File::create(&path)? // fresh run truncates
        };
        Ok(Self {
            file,
            inner: Mutex::new(JournalInner { cache }),
        })
    }

    pub fn key(label: &str, prompt: &str, schema: Option<&Value>) -> String {
        ids::agent_key(label, prompt, schema)
    }

    pub fn cached(&self, key: &str) -> Option<Value> {
        self.inner.lock().unwrap().cache.get(key).cloned()
    }

    pub fn record(&self, key: &str, value: &Value) -> Result<(), WorkflowError> {
        use std::io::Write;
        let line = serde_json::to_string(&serde_json::json!({"key": key, "value": value}))?;
        {
            let mut f = self.file.try_clone()?;
            writeln!(f, "{line}")?;
            f.flush()?;
        }
        self.inner
            .lock()
            .unwrap()
            .cache
            .insert(key.to_string(), value.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn journal(resume: bool) -> (TempDir, Store, String, WorkflowJournal) {
        let td = TempDir::new().unwrap();
        let store = Store::new(td.path()).unwrap();
        let run_id = store.reserve_run_id("review-changes").unwrap();
        let j = WorkflowJournal::new(&store, &run_id, resume).unwrap();
        (td, store, run_id, j)
    }

    #[test]
    fn record_then_cached_hits() {
        let (_td, _s, _id, j) = journal(false);
        let k = "agent-123";
        j.record(k, &json!({"isReal":true})).unwrap();
        assert_eq!(j.cached(k), Some(json!({"isReal":true})));
        assert_eq!(j.cached("agent-999"), None);
    }

    #[test]
    fn resume_replays_cache() {
        let (_td, store, run_id, j) = journal(false);
        j.record("agent-1", &json!({"v":1})).unwrap();
        j.record("agent-2", &json!({"v":2})).unwrap();
        drop(j);
        let j2 = WorkflowJournal::new(&store, &run_id, true).unwrap();
        assert_eq!(j2.cached("agent-1"), Some(json!({"v":1})));
        assert_eq!(j2.cached("agent-2"), Some(json!({"v":2})));
    }

    #[test]
    fn resume_corrupt_line_errors() {
        let (_td, store, run_id, _j) = journal(false);
        std::fs::write(store.journal_path(&run_id), "not json\n").unwrap();
        let err = WorkflowJournal::new(&store, &run_id, true);
        assert!(matches!(err, Err(WorkflowError::JournalCorrupt(_))));
    }
}
