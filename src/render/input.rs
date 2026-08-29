//! Console input thread: owns stdin + reedline, serially handles `InputCmd`.
//!
//! `spawn()` starts a named OS thread (`bytemaker-input`) that processes
//! `ReadLine` / `ReadPermission` / `Shutdown` over an `mpsc` channel. It blocks on
//! `blocking_recv` between commands, so permission prompts never block reading the next line.
//! `io::ConsoleInput` holds the sender + `JoinHandle`; `Shutdown` then joins to restore
//! terminal cooked mode. See `docs/modules/render.md`.

use std::borrow::Cow;

use tokio::sync::{mpsc, oneshot};

/// Input command for the command thread. `main` and `IO::ask_permission` send over the same channel.
pub enum InputCmd {
    /// Read one line of user input. `Some(line)` = submitted line; `None` = EOF / Ctrl+C cancel.
    ReadLine(oneshot::Sender<Option<String>>),
    /// Read a y/N permission confirmation. The prompt is already rendered by `IO::ask_permission`
    /// via `Output::permission`; the thread only reads a line and replies y/N via `reply`.
    ReadPermission(oneshot::Sender<bool>),
    /// Shut down the thread.
    Shutdown,
}

/// REPL prompt: left segment ` >> `, others empty (indicator row left to reedline default blank).
struct ReplPrompt;

impl reedline::Prompt for ReplPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(" >> ")
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _mode: reedline::PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_history_search_indicator(
        &self,
        _search: reedline::PromptHistorySearch,
    ) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
}

/// Start the InputTask: returns command sender + thread handle. The thread owns stdin + reedline
/// and serially handles commands, idling on `blocking_recv` between them so permission prompts
/// never block the next read.
///
/// `main` sends `ReadLine`; `IO::ask_permission` sends `ReadPermission` over the same sender.
/// `ConsoleInput` holds the `JoinHandle` for teardown: `shutdown` sends `Shutdown` then joins to
/// restore terminal state. main and the thread sync via oneshot, so on shutdown the thread is idle
/// or gone and join returns immediately.
pub fn spawn() -> (mpsc::Sender<InputCmd>, std::thread::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<InputCmd>(64);
    let handle = std::thread::Builder::new()
        .name("bytemaker-input".into())
        .spawn(move || {
            let mut ed = reedline::Reedline::create();
            let prompt = ReplPrompt;
            while let Some(cmd) = rx.blocking_recv() {
                match cmd {
                    InputCmd::ReadLine(reply) => {
                        match ed.read_line(&prompt) {
                            Ok(reedline::Signal::Success(line)) => {let _ = reply.send(Some(line));},
                            _ => break,
                        }
                    }
                    InputCmd::ReadPermission(reply) => {
                        let answer = match ed.read_line(&prompt) {
                            Ok(reedline::Signal::Success(l)) => l.trim().eq_ignore_ascii_case("y"),
                            _ => false,
                        };
                        let _ = reply.send(answer);
                    }
                    InputCmd::Shutdown => break,
                }
            }
        })
        .expect("spawn bytemaker-input thread");
    (tx, handle)
}
