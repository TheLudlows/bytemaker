//! I/O abstraction: decouples the Agent from concrete implementations.
//!
//! Output/Input traits let the Agent depend on abstractions, easing testing
//! and swapping I/O backends (e.g. Web UI, file logging).

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

// =====================================================================
// Trait definitions
// =====================================================================

/// Output abstraction: used by the Agent to render all user-visible content.
pub trait Output: Send + Sync {
    /// Emit a full output line (auto-newline).
    fn emit(&self, line: &str);

    /// Emit a banner.
    fn banner(&self, msg: &str);

    /// Emit status info (usually colored).
    fn status(&self, msg: &str);

    /// Emit error info (usually colored).
    fn error(&self, msg: &str);

    /// Emit a blank line.
    fn blank(&self);

    /// Render a tool output.
    fn render_tool_output(&self, name: &str, result: &str, color: bool);

    /// Render the prompt (for non-interactive mode).
    fn prompt(&self);

    /// Render a blocked-command notice.
    fn blocked(&self, pattern: &str);

    /// Render a permission request.
    fn permission(&self, reason: &str, name: &str, input: &serde_json::Value);

    /// Render the startup logo.
    fn logo(&self);

    /// Render a heading + body.
    fn heading(&self, title: &str, body: &str);
}

/// Input abstraction: used by the Agent/hook system to read user input.
///
/// `Input` only "reads". Cross-I/O coordination (e.g. permission prompts render first, then read y/N)
/// is done by the `IO` composite layer — hence `ask_permission` isn't here.
#[async_trait]
pub trait Input: Send + Sync {
    /// Read one line of user input.
    /// Returns `Some(line)` on normal input, `None` on EOF/cancel.
    async fn read_line(&self) -> Option<String>;

    /// Read a y/N permission confirmation. The prompt is rendered by the caller (`IO::ask_permission`)
    /// via `Output::permission`; this method only reads the reply. `y` (case-insensitive) → true, else false.
    async fn read_permission(&self) -> bool;

    /// Shut down the input backend (interactive mode stops InputTask; others no-op).
    async fn shutdown(&self);
}

// =====================================================================
// Console implementation: built on the existing render module
// =====================================================================

use crate::render::{Coordinator, CrosstermBackend};

/// Console output implementation backed by Coordinator.
pub struct ConsoleOutput {
    coordinator: Arc<std::sync::Mutex<Coordinator<CrosstermBackend>>>,
}

impl ConsoleOutput {
    pub fn new(coordinator: Arc<std::sync::Mutex<Coordinator<CrosstermBackend>>>) -> Self {
        Self { coordinator }
    }
}

impl Output for ConsoleOutput {
    fn emit(&self, line: &str) {
        let _ = self.coordinator.lock().unwrap().emit(line);
    }

    fn banner(&self, msg: &str) {
        self.coordinator.lock().unwrap().banner(msg);
    }

    fn status(&self, msg: &str) {
        self.coordinator.lock().unwrap().status(msg);
    }

    fn error(&self, msg: &str) {
        self.coordinator.lock().unwrap().error(msg);
    }

    fn blank(&self) {
        self.coordinator.lock().unwrap().blank();
    }

    fn render_tool_output(&self, name: &str, result: &str, color: bool) {
        self.coordinator
            .lock()
            .unwrap()
            .render_tool_output(name, result, color);
    }

    fn prompt(&self) {
        self.coordinator.lock().unwrap().prompt();
    }

    fn blocked(&self, pattern: &str) {
        self.coordinator.lock().unwrap().blocked(pattern);
    }

    fn permission(&self, reason: &str, name: &str, input: &serde_json::Value) {
        self.coordinator
            .lock()
            .unwrap()
            .permission(reason, name, input);
    }

    fn logo(&self) {
        self.coordinator.lock().unwrap().logo();
    }

    fn heading(&self, title: &str, body: &str) {
        self.coordinator.lock().unwrap().heading(title, body);
    }
}

/// Console input implementation backed by InputTask.
///
/// Holds the command sender (always present — backed by the input thread from `render::input::spawn`)
/// and the input thread's `JoinHandle` (wrapped in `Mutex` so `shutdown` can `take` it to join —
/// `JoinHandle::join` takes ownership). Allows deterministic thread join on teardown (see `render::input::spawn`).
pub struct ConsoleInput {
    cmd_tx: mpsc::Sender<crate::render::input::InputCmd>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl ConsoleInput {
    pub fn new(
        cmd_tx: mpsc::Sender<crate::render::input::InputCmd>,
        handle: Option<std::thread::JoinHandle<()>>,
    ) -> Self {
        Self {
            cmd_tx,
            handle: Mutex::new(handle),
        }
    }
}

#[async_trait]
impl Input for ConsoleInput {
    async fn read_line(&self) -> Option<String> {
        // Ask reedline to read a line via the InputTask command channel (don't short-circuit stdin at the IO layer).
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(crate::render::input::InputCmd::ReadLine(tx))
            .await
            .is_err()
        {
            return None;
        }
        rx.await.ok().flatten()
    }

    async fn read_permission(&self) -> bool {
        // Prompt already rendered by IO::ask_permission via Output::permission; here we only read y/N.
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(crate::render::input::InputCmd::ReadPermission(tx))
            .await
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    async fn shutdown(&self) {
        let _ = self
            .cmd_tx
            .send(crate::render::input::InputCmd::Shutdown)
            .await;
        // Take it out first so the MutexGuard drops before the await (guard is not Send, can't cross await).
        let to_join = self.handle.lock().unwrap().take();
        if let Some(handle) = to_join {
            // The thread is now idle or exited: main syncs with it via oneshot, so shutdown can't race an
            // in-flight read (EOF path already broke out; otherwise it's blocked on blocking_recv
            // awaiting Shutdown). Thus join returns immediately. join takes ownership; run via
            // spawn_blocking to avoid blocking the tokio executor.
            let _ = tokio::task::spawn_blocking(move || handle.join()).await;
        }
    }
}

// =====================================================================
// Mock implementations for testing
// =====================================================================

use std::sync::Mutex;

/// In-memory output: accumulates all output for assertions.
#[derive(Default)]
pub struct MemoryOutput {
    lines: Arc<Mutex<Vec<String>>>,
}

impl MemoryOutput {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get all accumulated lines.
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    /// Clear accumulated lines.
    pub fn clear(&self) {
        self.lines.lock().unwrap().clear();
    }
}

impl Output for MemoryOutput {
    fn emit(&self, line: &str) {
        self.lines.lock().unwrap().push(line.to_string());
    }

    fn banner(&self, msg: &str) {
        self.lines.lock().unwrap().push(format!("[BANNER] {}", msg));
    }

    fn status(&self, msg: &str) {
        self.lines.lock().unwrap().push(format!("[STATUS] {}", msg));
    }

    fn error(&self, msg: &str) {
        self.lines.lock().unwrap().push(format!("[ERROR] {}", msg));
    }

    fn blank(&self) {
        self.lines.lock().unwrap().push("".to_string());
    }

    fn render_tool_output(&self, name: &str, result: &str, _color: bool) {
        self.lines
            .lock()
            .unwrap()
            .push(format!("[TOOL] {} -> {}", name, result));
    }

    fn prompt(&self) {
        self.lines.lock().unwrap().push(">> ".to_string());
    }

    fn blocked(&self, pattern: &str) {
        self.lines
            .lock()
            .unwrap()
            .push(format!("[BLOCKED] {}", pattern));
    }

    fn permission(&self, reason: &str, name: &str, input: &serde_json::Value) {
        self.lines
            .lock()
            .unwrap()
            .push(format!("[PERMISSION] {} {}({})", reason, name, input));
    }

    fn logo(&self) {
        self.lines.lock().unwrap().push("[LOGO] ByteMaker".to_string());
    }

    fn heading(&self, title: &str, body: &str) {
        self.lines
            .lock()
            .unwrap()
            .push(format!("[HEADING] ## {title}\n{body}"));
    }
}

/// Mock input: a sequence of predefined responses.
#[derive(Default)]
pub struct MockInput {
    /// Predefined `read_line` responses (LIFO via `Vec::pop`).
    read_responses: Arc<Mutex<Vec<Option<String>>>>,
    /// Predefined `read_permission` responses (LIFO via `Vec::pop`).
    permission_responses: Arc<Mutex<Vec<bool>>>,
}

impl MockInput {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a `read_line` response.
    pub fn push_read(&self, response: Option<String>) {
        self.read_responses
            .lock()
            .unwrap()
            .push(response);
    }

    /// Add a `read_permission` response.
    pub fn push_permission(&self, granted: bool) {
        self.permission_responses
            .lock()
            .unwrap()
            .push(granted);
    }
}

#[async_trait]
impl Input for MockInput {
    async fn read_line(&self) -> Option<String> {
        self.read_responses.lock().unwrap().pop().flatten()
    }

    async fn read_permission(&self) -> bool {
        self.permission_responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(false)
    }

    async fn shutdown(&self) {}
}

// =====================================================================
// Composite: IO pair
// =====================================================================

/// I/O composite: holds both input and output.
///
/// Cross-I/O coordination happens here — e.g. `ask_permission` renders the prompt via
/// `Output::permission` then reads y/N via `Input::read_permission`. The `Input` trait only
/// "reads" and no longer holds an `Output`.
#[derive(Clone)]
pub struct IO {
    pub output: Arc<dyn Output>,
    pub input: Arc<dyn Input>,
}

impl IO {
    pub fn new(output: Arc<dyn Output>, input: Arc<dyn Input>) -> Self {
        Self { output, input }
    }

    /// Create console I/O (Coordinator is an internal detail).
    pub fn console() -> Self {
        let coordinator: Arc<Mutex<Coordinator<CrosstermBackend>>> =
            Arc::new(Mutex::new(Coordinator::new(CrosstermBackend::new())));

        let output: Arc<dyn Output> = Arc::new(ConsoleOutput::new(coordinator.clone()));
        let (cmd_tx, handle) = crate::render::input::spawn();
        let input: Arc<dyn Input> = Arc::new(ConsoleInput::new(cmd_tx, Some(handle)));

        Self { output, input }
    }

    /// Read one line of user input (via the Input trait; interactive mode routes through
    /// ConsoleInput's InputTask, others fall back on their own).
    pub async fn read_line(&self) -> Option<String> {
        self.input.read_line().await
    }

    /// Request user permission: render the prompt via `Output::permission`, then read y/N
    /// via `Input::read_permission`. With no interactive channel, returns false after rendering.
    pub async fn ask_permission(
        &self,
        reason: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> bool {
        self.output.permission(reason, name, input);
        self.input.read_permission().await
    }

    /// Shut down the input backend (via the Input trait; interactive mode stops InputTask).
    pub async fn shutdown(&self) {
        self.input.shutdown().await;
    }

    /// Create in-memory I/O for testing.
    pub fn memory() -> Self {
        let output = Arc::new(MemoryOutput::new());
        let input = Arc::new(MockInput::new());
        Self { output, input }
    }
}

// =====================================================================
// Type-conversion helpers (for tests)
// =====================================================================

/// Add downcast support for test types.
impl MemoryOutput {
    /// Get a reference to internal lines (for test assertions).
    pub fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl MockInput {
    /// Get a reference to internal state (for test assertions).
    pub fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// =====================================================================
// Unit tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_output_collects_lines() {
        let output = MemoryOutput::new();
        output.banner("hello");
        output.emit("world");
        output.error("error");

        let lines = output.lines();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("hello"));
        assert_eq!(lines[1], "world");
        assert!(lines[2].contains("error"));
    }

    #[test]
    fn memory_output_clear() {
        let output = MemoryOutput::new();
        output.emit("a");
        output.emit("b");
        assert_eq!(output.lines().len(), 2);

        output.clear();
        assert!(output.lines().is_empty());
    }

    #[tokio::test]
    async fn mock_input_fifo() {
        let input = MockInput::new();
        // LIFO order (Vec::pop)
        input.push_read(None); // EOF
        input.push_read(Some("second".into()));
        input.push_read(Some("first".into()));

        assert_eq!(input.read_line().await, Some("first".into()));
        assert_eq!(input.read_line().await, Some("second".into()));
        assert_eq!(input.read_line().await, None);
    }

    #[tokio::test]
    async fn mock_input_read_permission() {
        let input = MockInput::new();
        // LIFO (Vec::pop)
        input.push_permission(false);
        input.push_permission(true);

        assert!(input.read_permission().await);
        assert!(!input.read_permission().await);
        // defaults to false
        assert!(!input.read_permission().await);
    }

    #[tokio::test]
    async fn io_ask_permission_renders_then_reads() {
        // ask_permission should render the prompt first (into MemoryOutput) then read the reply (MockInput).
        let output = Arc::new(MemoryOutput::new());
        let input = Arc::new(MockInput::new());
        let io = IO::new(output.clone(), input.clone());

        // LIFO: first ask → true, second → false, third defaults to false
        input.push_permission(false);
        input.push_permission(true);

        assert!(io.ask_permission("r1", "n1", &serde_json::json!({"k": "v"})).await);
        assert!(!io.ask_permission("r2", "n2", &serde_json::json!({})).await);
        assert!(!io.ask_permission("r3", "n3", &serde_json::json!({})).await);

        // each ask writes a [PERMISSION] prompt first
        let lines = output.lines();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("[PERMISSION]") && lines[0].contains("r1") && lines[0].contains("n1"));
        assert!(lines[1].contains("r2") && lines[1].contains("n2"));
        assert!(lines[2].contains("r3") && lines[2].contains("n3"));
    }

    #[test]
    fn io_memory_creates_pair() {
        // directly create concrete I/O implementations for testing
        let mem_output = MemoryOutput::new();
        let _mock_input = MockInput::new();

        mem_output.emit("test");
        assert_eq!(mem_output.lines()[0], "test");
    }
}
