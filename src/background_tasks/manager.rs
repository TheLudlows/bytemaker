/*
manager.rs - BackgroundManager: background task registry + worker dispatch (s11)

In-process registry, in-memory (persistence is s10's job). state is Arc<Mutex<State>>:
the worker (tokio::spawn) clones the BackgroundManager to share the same state.
Locks are fine-grained and held briefly (only touches HashMap/queue, no IO).
*/

use crate::background_tasks::task::{BackgroundTask, TaskStatus};
use fastrand;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Max concurrent background tasks.
pub const MAX_CONCURRENT: usize = 8;
/// Background command timeout (seconds).
pub const BG_TIMEOUT_SECS: u64 = 120;
/// Truncation byte budget for output written to disk and read back.
pub const MAX_OUTPUT_BYTES: usize = 50_000;
/// Truncation char count for notification summaries.
pub const SUMMARY_CHARS: usize = 500;

struct State {
    /// bg_id -> task
    tasks: HashMap<String, BackgroundTask>,
    /// Finished bg_ids awaiting collection (FIFO).
    ready: VecDeque<String>,
    /// bg_id -> cancel signal.
    cancels: HashMap<String, Arc<Notify>>,
}

impl State {
    fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            ready: VecDeque::new(),
            cancels: HashMap::new(),
        }
    }

    /// Count of currently Running tasks (concurrency gate).
    fn running_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|t| t.status == TaskStatus::Running)
            .count()
    }
}

/// Background task manager. Clone is cheap (Arc-shared state); the worker takes a clone.
#[derive(Clone)]
pub struct BackgroundManager {
    output_dir: PathBuf,
    state: Arc<Mutex<State>>,
    timeout_secs: u64,
}

impl BackgroundManager {
    const MAX_ID_RETRIES: usize = 100;

    /// Create the manager. In-memory construction, infallible (unlike s10 TaskStore::new).
    pub fn new(output_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&output_dir);
        Self {
            output_dir,
            state: Arc::new(Mutex::new(State::new())),
            timeout_secs: BG_TIMEOUT_SECS,
        }
    }

    /// Start a background task: register + spawn worker + return bg_id immediately.
    /// Synchronous (the inner tokio::spawn is a sync call). The caller folds bg_id into a
    /// placeholder tool_output.
    pub fn start(&self, command: &str, call_id: &str) -> Result<String, String> {
        let command = command.trim();
        if command.is_empty() {
            return Err("Error: empty command".to_string());
        }
        let started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cancel = Arc::new(Notify::new());

        // Hold the lock across id generation + insert to close the TOCTOU window:
        // generate_id_locked mints a non-colliding id on an already-locked state and inserts
        // right after, so two concurrent start() calls can't grab the same uninserted id.
        let (id, output_file) = {
            let mut state = self.state.lock().expect("state mutex poisoned");
            if state.running_count() >= MAX_CONCURRENT {
                return Err(format!(
                    "Error: too many concurrent background tasks ({}). Wait for some to finish via TaskOutput.",
                    MAX_CONCURRENT
                ));
            }
            let id = generate_id_locked(&state);
            if id.is_empty() {
                return Err("Error: failed to allocate task id".to_string());
            }
            let output_file = self.output_dir.join(format!("{}.log", id));
            let task = BackgroundTask {
                id: id.clone(),
                command: command.to_string(),
                status: TaskStatus::Running,
                call_id: call_id.to_string(),
                started_at,
                output_file: output_file.clone(),
                exit_code: None,
            };
            state.tasks.insert(id.clone(), task);
            state.cancels.insert(id.clone(), cancel.clone());
            (id, output_file)
        };

        let mgr = self.clone();
        let cmd = command.to_string();
        tokio::spawn(run_worker(mgr, id.clone(), cmd, output_file, cancel));
        Ok(id)
    }

    /// Collect finished tasks, returning a list of <task_notification> XML strings.
    /// Removes them from tasks after collecting (notify once, then drop to avoid re-inject).
    pub fn collect(&self) -> Vec<String> {
        let drained: Vec<String> = {
            let mut state = self.state.lock().expect("state mutex poisoned");
            state.ready.drain(..).collect()
        };
        let mut out = Vec::new();
        for id in drained {
            let task_opt = {
                let mut state = self.state.lock().expect("state mutex poisoned");
                state.tasks.remove(&id)
            };
            if let Some(task) = task_opt {
                out.push(format_notification(&task));
            }
        }
        out
    }

    /// Passive fallback at the top of the loop: drain ready and append notifications as
    /// standalone user-message Text blocks.
    pub fn collect_and_inject(&self, messages: &mut Vec<crate::domain::message::Message>) -> Option<usize> {
        let notifications = self.collect();
        if notifications.is_empty() {
            return None;
        }
        let count = notifications.len();
        let blocks: Vec<crate::domain::message::ContentBlock> = notifications
            .into_iter()
            .map(|n| crate::domain::message::ContentBlock::Text { text: n })
            .collect();
        messages.push(crate::domain::message::Message::user_blocks(blocks));
        Some(count)
    }

    /// s17: true if any background task is still running (Goal `defer` gate).
    pub fn has_running(&self) -> bool {
        self.state
            .lock()
            .expect("state mutex poisoned")
            .running_count()
            > 0
    }

    /// Fetch a background task's output and status.
    ///
    /// - block=false: return immediately with status + current output_file contents
    ///   (truncated to MAX_OUTPUT_BYTES).
    /// - block=true: poll until the task leaves Running or timeout_ms elapses; timeout does
    ///   not cancel the task.
    pub async fn output(&self, task_id: &str, block: bool, timeout_ms: u64) -> String {
        if block {
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
            loop {
                let done = {
                    let state = self.state.lock().expect("state mutex poisoned");
                    state
                        .tasks
                        .get(task_id)
                        .map(|t| t.status != TaskStatus::Running)
                        .unwrap_or(true) // missing -> treat as done (returns not found below)
                };
                if done || std::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        let (status, output_file, exit_code, command) = {
            let state = self.state.lock().expect("state mutex poisoned");
            match state.tasks.get(task_id) {
                Some(t) => (
                    t.status,
                    t.output_file.clone(),
                    t.exit_code,
                    t.command.clone(),
                ),
                None => return format!("Error: task {} not found", task_id),
            }
        };
        let body = std::fs::read_to_string(&output_file)
            .unwrap_or_default()
            .trim()
            .to_string();
        let body = truncate_chars(&body, MAX_OUTPUT_BYTES);
        format!(
            "task_id: {}\nstatus: {}\ncommand: {}\nexit_code: {}\noutput:\n{}",
            task_id,
            status_word(status),
            command,
            exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "none".to_string()),
            body
        )
    }

    /// Cancel a background task: fire cancel Notify -> worker takes the cancel branch ->
    /// kill the process tree -> Cancelled -> enqueued in ready.
    /// Already-finished or unknown tasks return a message and are a no-op.
    pub fn stop(&self, task_id: &str) -> String {
        let cancel_opt = {
            let state = self.state.lock().expect("state mutex poisoned");
            match state.tasks.get(task_id).map(|t| t.status) {
                None => return format!("Error: task {} not found", task_id),
                Some(TaskStatus::Running) => state.cancels.get(task_id).cloned(),
                Some(other) => return format!("Task {} already {}", task_id, status_word(other)),
            }
        };
        match cancel_opt {
            Some(cancel) => {
                cancel.notify_one();
                format!("[Stopped {}]", task_id)
            }
            None => format!("Error: task {} not found", task_id),
        }
    }
}

/// Generate a non-colliding bg_id on an already-locked state (100 retries).
/// The caller must already hold the state lock so generate + insert happen in one critical
/// section, closing the TOCTOU window that a self-locking helper would open.
fn generate_id_locked(state: &State) -> String {
    for _ in 0..BackgroundManager::MAX_ID_RETRIES {
        let id = format!("bg_{:08x}", fastrand::u32(..));
        if !state.tasks.contains_key(&id) {
            return id;
        }
    }
    String::new() // vanishingly rare; caller treats as error
}

/// Worker: spawns the body as an inner task and awaits the JoinHandle to guard against panics.
///
/// Normal: the inner task runs select! + writes output + finalizes.
/// Panic: JoinHandle.await returns Err -> fallback finalize(Failed), so the task never
/// sticks in Running.
async fn run_worker(
    mgr: BackgroundManager,
    id: String,
    command: String,
    output_file: PathBuf,
    cancel: Arc<Notify>,
) {
    let output_file_for_body = output_file.clone();
    let mgr_panic = mgr.clone();
    let id_panic = id.clone();
    let handle = tokio::spawn(async move {
        let mut cmd = build_command(&command);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let text = format!("Error: spawn failed: {}", e);
                let _ = std::fs::write(&output_file_for_body, &text);
                finalize(&mgr, &id, TaskStatus::Failed, None);
                return;
            }
        };
        let timeout_secs = mgr.timeout_secs;
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let (status, exit_code, text) = tokio::select! {
            biased;
            _ = cancel.notified() => {
                kill_tree(&mut child).await;
                let _ = child.wait().await;
                (TaskStatus::Cancelled, None, "Cancelled by TaskStop".to_string())
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
                kill_tree(&mut child).await;
                let _ = child.wait().await;
                (TaskStatus::Failed, None, format!("Error: Timeout ({}s)", timeout_secs))
            }
            (exit_status, stdout, stderr) = async {
                tokio::join!(
                    child.wait(),
                    drain_pipe(stdout_pipe),
                    drain_pipe(stderr_pipe),
                )
            } => match exit_status {
                Ok(es) => {
                    let code = es.code();
                    let status = if code == Some(0) {
                        TaskStatus::Completed
                    } else {
                        TaskStatus::Failed
                    };
                    let body = format!("{}\n{}", stdout, stderr).trim().to_string();
                    let body = if body.is_empty() {
                        "(no output)".to_string()
                    } else {
                        truncate_chars(&body, MAX_OUTPUT_BYTES)
                    };
                    (status, code, body)
                }
                Err(e) => (
                    TaskStatus::Failed,
                    None,
                    format!("Error: wait failed: {}", e),
                ),
            }
        };
        let _ = std::fs::write(&output_file_for_body, &text);
        finalize(&mgr, &id, status, exit_code);
    });
    // Panic fallback: if the inner task crashed, still mark the task Failed + enqueue it so it
    // never sticks in Running.
    if let Err(_join_err) = handle.await {
        let _ = std::fs::write(&output_file, "Error: worker panicked");
        finalize(&mgr_panic, &id_panic, TaskStatus::Failed, None);
    }
}

/// Read and decode the full contents of a child pipe (stdout/stderr). Called after wait()
/// completes: the child has exited, the write end is closed, read_to_end hits EOF.
async fn drain_pipe<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> String {
    use tokio::io::AsyncReadExt;
    match pipe {
        Some(mut p) => {
            let mut buf = Vec::new();
            let _ = p.read_to_end(&mut buf).await;
            crate::tools::command::decode_console(&buf)
        }
        None => String::new(),
    }
}

/// Finalize: update task fields, enqueue into ready, and drop the cancel signal.
fn finalize(mgr: &BackgroundManager, id: &str, status: TaskStatus, exit_code: Option<i32>) {
    let mut state = mgr.state.lock().expect("state mutex poisoned");
    if let Some(task) = state.tasks.get_mut(id) {
        task.status = status;
        task.exit_code = exit_code;
        state.ready.push_back(id.to_string());
    }
    state.cancels.remove(id);
}

/// Build a cross-platform command. Sets a new process group / CREATE_NEW_PROCESS_GROUP so
/// kill_tree can kill the whole tree.
fn build_command(command: &str) -> tokio::process::Command {
    let mut c = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.args(["/C", command]);
        c
    } else {
        let mut c = tokio::process::Command::new("bash");
        c.arg("-c").arg(command);
        c
    };
    c.current_dir(crate::tools::workdir());

    // New process group: Unix process_group(0) sets PGID = child PID; Windows uses
    // CREATE_NEW_PROCESS_GROUP. kill_tree uses this to kill the whole group/tree.
    #[cfg(unix)]
    {
        c.process_group(0);
    }
    #[cfg(windows)]
    {
        // CREATE_NEW_PROCESS_GROUP = 0x00000200
        c.creation_flags(0x00000200);
    }
    c
}

/// Kill the whole process tree (zero deps, shell-out).
///
/// - Unix: `kill -KILL -{pgid}` (negative PID = process group; child is its own group via
///   process_group(0)).
/// - Windows: `taskkill /T /F /PID {pid}` (`/T` kills the whole subtree). Falls back to
///   child.kill() for the direct child.
async fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        let mut kill_cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("taskkill");
            c.args(["/T", "/F", "/PID", &pid.to_string()]);
            c
        } else {
            let mut c = tokio::process::Command::new("kill");
            c.args(["-KILL", &format!("-{}", pid)]);
            c
        };
        kill_cmd.stdout(std::process::Stdio::null());
        kill_cmd.stderr(std::process::Stdio::null());
        let _ = kill_cmd.output().await;
    }
    let _ = child.kill().await;
}

/// Truncate to a byte budget, landing on a UTF-8 char boundary (same logic as command.rs).
fn truncate_chars(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Format a finished task into a <task_notification> XML string.
fn format_notification(task: &BackgroundTask) -> String {
    let summary = match std::fs::read_to_string(&task.output_file) {
        Ok(s) => truncate_chars(s.trim(), SUMMARY_CHARS),
        Err(_) => "(output unavailable)".to_string(),
    };
    let exit = task
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "none".to_string());
    format!(
        "<task_notification>\n  <task_id>{}</task_id>\n  <status>{}</status>\n  <command>{}</command>\n  <exit_code>{}</exit_code>\n  <summary>{}</summary>\n</task_notification>",
        task.id,
        status_word(task.status),
        task.command,
        exit,
        summary
    )
}

/// Status -> bare snake_case word, stripping JSON quotes.
fn status_word(status: TaskStatus) -> String {
    serde_json::to_string(&status)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

#[cfg(test)]
pub(crate) fn create_test_manager(output_dir: &std::path::Path) -> BackgroundManager {
    BackgroundManager::new(output_dir.to_path_buf())
}

#[cfg(test)]
pub(crate) fn create_test_manager_with_timeout(
    output_dir: &std::path::Path,
    timeout_secs: u64,
) -> BackgroundManager {
    BackgroundManager {
        output_dir: output_dir.to_path_buf(),
        state: Arc::new(Mutex::new(State::new())),
        timeout_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn new_creates_output_dir() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("background");
        let _mgr = create_test_manager(&out);
        assert!(out.exists(), "output dir should be created");
    }

    #[test]
    fn generate_id_format_and_unique() {
        // Verify generate_id_locked directly on a bare State (not via the manager's internal
        // lock), matching how start() calls it (start holds the lock then calls generate_id_locked(&state)).
        let mut state = State::new();
        let mut ids = Vec::new();
        for _ in 0..50 {
            let id = generate_id_locked(&state);
            assert!(id.starts_with("bg_"));
            assert_eq!(id.len(), 11); // "bg_" + 8 hex
            assert!(id[3..].chars().all(|c| c.is_ascii_hexdigit()));
            assert!(!ids.contains(&id), "id collision: {}", id);
            ids.push(id.clone());
            // Occupy the id to verify no collision next time.
            state.tasks.insert(
                id,
                BackgroundTask {
                    id: String::new(),
                    command: "echo".to_string(),
                    status: TaskStatus::Running,
                    call_id: "t".to_string(),
                    started_at: 0,
                    output_file: PathBuf::from("/tmp/x"),
                    exit_code: None,
                },
            );
        }
    }

    #[tokio::test]
    async fn start_completes_and_collect_injects_notification() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager(dir.path());
        let id = mgr.start("echo hello_bg", "toolu_1").expect("start should succeed");
        assert!(id.starts_with("bg_"));

        // Wait for the worker to finish (fast command; poll until collect has content).
        let mut got = Vec::new();
        for _ in 0..200 {
            got = mgr.collect();
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(got.len(), 1, "expected one notification, got: {:?}", got);
        let n = &got[0];
        assert!(n.contains("<task_notification>"));
        assert!(n.contains(&id));
        assert!(n.contains("completed"));
        assert!(n.contains("hello_bg"));

        // After collect the task is gone from memory (prevents re-inject).
        let again = mgr.collect();
        assert!(again.is_empty(), "collect should drain, not repeat");
    }

    #[tokio::test]
    async fn failed_command_yields_failed_notification() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager(dir.path());
        let cmd = if cfg!(windows) { "cmd /C exit 7" } else { "bash -c 'exit 7'" };
        let _id = mgr.start(cmd, "toolu_2").unwrap();
        let mut got = Vec::new();
        for _ in 0..200 {
            got = mgr.collect();
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("failed"), "got: {}", got[0]);
        assert!(got[0].contains("7"), "exit code in notification: {}", got[0]);
    }

    #[tokio::test]
    async fn timeout_yields_failed_with_consistent_text() {
        // Regression for an s11 bug: on timeout exit_code=None, status must be Failed and text must say Timeout.
        let dir = tempdir().unwrap();
        let mgr = create_test_manager_with_timeout(dir.path(), 1);
        let cmd = if cfg!(windows) { "ping -n 120 127.0.0.1" } else { "sleep 120" };
        let id = mgr.start(cmd, "toolu_3").unwrap();
        let mut got = Vec::new();
        for _ in 0..400 {
            got = mgr.collect();
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(got.len(), 1, "should have completed via timeout");
        let n = &got[0];
        assert!(n.contains("failed"), "status must be failed: {}", n);
        assert!(n.contains("none"), "exit_code must be none: {}", n);
        let log = std::fs::read_to_string(dir.path().join(format!("{}.log", id)))
            .unwrap_or_default();
        assert!(log.contains("Timeout"), "log must say Timeout: {}", log);
    }

    #[tokio::test]
    async fn stop_cancels_running_task() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager_with_timeout(dir.path(), 60);
        let cmd = if cfg!(windows) { "ping -n 60 127.0.0.1" } else { "sleep 60" };
        let id = mgr.start(cmd, "toolu_4").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let msg = mgr.stop(&id);
        assert!(msg.contains(&format!("[Stopped {}]", id)), "stop msg: {}", msg);

        let mut got = Vec::new();
        for _ in 0..200 {
            got = mgr.collect();
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("cancelled"), "status must be cancelled: {}", got[0]);
    }

    #[tokio::test]
    async fn stop_on_finalized_task_returns_already() {
        // stop once -> Cancelled (finalize sets status to Cancelled, but the task stays in
        // tasks until collect). Immediately stop again to hit the "already {status}" branch
        // (non-Running).
        let dir = tempdir().unwrap();
        let mgr = create_test_manager_with_timeout(dir.path(), 60);
        let cmd = if cfg!(windows) { "ping -n 60 127.0.0.1" } else { "sleep 60" };
        let id = mgr.start(cmd, "toolu_5").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let first = mgr.stop(&id);
        assert!(first.contains("[Stopped"));

        // Wait for the worker to finish the cancel arm + finalize (status set to Cancelled,
        // still in tasks because not collected). Poll the second stop until it hits "already"
        // (finalize done, status non-Running). Polling instead of a fixed sleep avoids flakes
        // under CPU contention in parallel tests. Re-stopping a Running task just fires
        // notify_one again (worker already left the cancel arm) — no side effects.
        #[allow(unused_assignments)]
        let mut second = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            second = mgr.stop(&id);
            if second.contains("already") || std::time::Instant::now() >= deadline {
                break;
            }
        }
        assert!(
            second.contains("already"),
            "second stop on finalized task should say 'already', got: {}",
            second
        );
        // Cleanup: collect this cancelled task so it doesn't affect other tests.
        let _ = mgr.collect();
    }

    #[test]
    fn stop_unknown_task_is_noop_error() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager(dir.path());
        let msg = mgr.stop("bg_deadbeef");
        assert!(msg.contains("not found"), "unknown stop: {}", msg);
    }
}
