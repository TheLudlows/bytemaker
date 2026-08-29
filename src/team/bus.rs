use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::time::Duration;
use serde_json::Value;
use tokio::sync::Notify;

use crate::tools::safe_path_in;

pub const MAILBOX_DIR_NAME: &str = ".mailboxes";

/// One delivered message, serialized one-per-line to `<workdir>/.mailboxes/<name>.jsonl`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MessageRecord {
    pub from: String,
    pub to: String,
    pub content: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub ts: f64,
    pub metadata: Value,
}

/// File-based mailboxes (`.mailboxes/<name>.jsonl`) plus per-agent `Notify`
/// wakeups so a teammate can block on `wait_for_messages` without polling.
pub struct MessageBus {
    workdir: PathBuf,
    inner: Mutex<BusInner>,
}

struct BusInner {
    notifies: HashMap<String, Arc<Notify>>,
}

impl MessageBus {
    pub fn new(workdir: PathBuf) -> Self {
        Self { workdir, inner: Mutex::new(BusInner { notifies: HashMap::new() }) }
    }

    pub fn maildir(&self) -> PathBuf {
        self.workdir.join(MAILBOX_DIR_NAME)
    }

    fn notify_for(&self, agent: &str) -> Arc<Notify> {
        let mut inner = self.inner.lock().unwrap();
        inner.notifies.entry(agent.to_string()).or_default().clone()
    }

    fn mailbox_path(&self, agent: &str) -> Result<PathBuf, String> {
        if !is_valid_agent_name(agent) {
            return Err(format!("Invalid mailbox recipient: {:?}", agent));
        }
        let dir = self.maildir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        safe_path_in(&dir, &format!("{}.jsonl", agent))
    }

    pub fn send(&self, from: &str, to: &str, content: &str, msg_type: &str, metadata: Option<Value>) {
        let record = MessageRecord {
            from: from.into(),
            to: to.into(),
            content: content.into(),
            msg_type: msg_type.into(),
            ts: 0.0,
            metadata: metadata.unwrap_or(Value::Null),
        };
        if let Ok(path) = self.mailbox_path(to) {
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                use std::io::Write;
                let _ = writeln!(file, "{}", serde_json::to_string(&record).unwrap_or_default());
            }
        }
        self.notify_for(to).notify_one();
    }

    fn read_file(&self, agent: &str) -> Vec<MessageRecord> {
        let path = match self.mailbox_path(agent) {
            Ok(p) => p,
            Err(_) => return vec![],
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                // Destructive read: drain the mailbox file once consumed.
                let _ = std::fs::remove_file(&path);
                s.lines()
                    .filter_map(|l| {
                        if l.trim().is_empty() {
                            None
                        } else {
                            serde_json::from_str(l).ok()
                        }
                    })
                    .collect()
            }
            Err(_) => vec![],
        }
    }

    pub fn read_inbox(&self, agent: &str) -> Vec<MessageRecord> {
        self.read_file(agent)
    }

    pub fn peek(&self, agent: &str) -> bool {
        match self.mailbox_path(agent) {
            Ok(p) => p.exists() && std::fs::metadata(&p).map(|m| m.len() > 0).unwrap_or(false),
            Err(_) => false,
        }
    }

    pub async fn wait_for_messages(&self, agent: &str, timeout: Duration) -> Vec<MessageRecord> {
        loop {
            let existing = self.read_file(agent);
            if !existing.is_empty() {
                return existing;
            }
            let notify = self.notify_for(agent);
            let notified = notify.notified();
            tokio::pin!(notified);
            match tokio::time::timeout(timeout, notified).await {
                Ok(_) => continue, // re-check after wake
                Err(_) => return self.read_file(agent),
            }
        }
    }
}

pub fn is_valid_agent_name(name: &str) -> bool {
    let re = regex::Regex::new(r"^[A-Za-z0-9_-]{1,64}$").unwrap();
    re.is_match(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn send_then_read_is_destructive() {
        let tmp = TempDir::new().unwrap();
        let bus = MessageBus::new(tmp.path().to_path_buf());
        bus.send("lead", "alice", "hi", "message", None);
        let msgs = bus.read_inbox("alice");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "hi");
        assert_eq!(msgs[0].msg_type, "message");
        // second read is empty (destructive)
        assert!(bus.read_inbox("alice").is_empty());
    }

    #[test]
    fn peek_detects_messages() {
        let tmp = TempDir::new().unwrap();
        let bus = MessageBus::new(tmp.path().to_path_buf());
        assert!(!bus.peek("bob"));
        bus.send("lead", "bob", "x", "message", None);
        assert!(bus.peek("bob"));
    }

    #[tokio::test]
    async fn wait_for_messages_times_out_empty() {
        let tmp = TempDir::new().unwrap();
        let bus = MessageBus::new(tmp.path().to_path_buf());
        let msgs = bus.wait_for_messages("carol", Duration::from_millis(50)).await;
        assert!(msgs.is_empty());
    }

    #[test]
    fn invalid_agent_name_rejected() {
        let tmp = TempDir::new().unwrap();
        let bus = MessageBus::new(tmp.path().to_path_buf());
        bus.send("lead", "../escape", "x", "message", None);
        // no file written outside maildir; peek false
        assert!(!bus.peek("../escape"));
    }
}
