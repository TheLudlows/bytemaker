//! Console output render backend.
//!
//! `Coordinator<B: Backend>` writes output line-by-line via `emit` (each line + `\n`).
//! Real terminals use `CrosstermBackend` (wrapped in `Mutex` at the `io` layer for `Arc` sharing).
//! Coloring is double-gated: `colors_enabled()` (by `NO_COLOR`) and `should_colorize()`
//! (by TTY + `NO_COLOR`) must both be true; `render_tool_output` never touches global
//! `set_override` to avoid polluting later output. See `docs/modules/render.md`.

use std::io::{self, Write};
use colored::{Colorize, control as colored_control};

pub mod input;

/// Disable colors when `NO_COLOR` is set (<https://no-color.org>; zero-dependency, CI-friendly).
pub fn colors_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

/// Backend abstraction; Coordinator is generic over this.
pub trait Backend {
    fn write_str(&mut self, s: &str) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

/// Coordinator: holds a backend, writes output line-by-line.
pub struct Coordinator<B: Backend> {
    backend: B,
}

impl<B: Backend> Coordinator<B> {
    pub fn new(backend: B) -> Self { Self { backend } }

    /// Emit one full output line.
    pub fn emit(&mut self, line: &str) -> io::Result<()> {
        self.backend.write_str(line)?;
        self.backend.write_str("\n")?;
        self.backend.flush()?;
        Ok(())
    }

    // ---- UX output methods ----

    /// Plain banner line (uncolored).
    pub fn banner(&mut self, msg: &str) { let _ = self.emit(msg); }

    /// Blank line.
    pub fn blank(&mut self) { let _ = self.emit(""); }

    /// Status line.
    pub fn status(&mut self, msg: &str) {
        if colored_control::SHOULD_COLORIZE.should_colorize() {
            let _ = self.emit(&format!("{}", msg.yellow()));
        } else {
            let _ = self.emit(msg);
        }
    }

    /// Error line.
    pub fn error(&mut self, msg: &str) {
        if colored_control::SHOULD_COLORIZE.should_colorize() {
            let _ = self.emit(&format!("{}", msg.red()));
        } else {
            let _ = self.emit(msg);
        }
    }

    /// Blocked notice.
    pub fn blocked(&mut self, pattern: &str) {
        let _ = self.emit(""); // leading blank line
        let msg = format!("[blocked] '{}' is on the deny list", pattern);
        if colored_control::SHOULD_COLORIZE.should_colorize() {
            let _ = self.emit(&format!("{}", msg.red()));
        } else {
            let _ = self.emit(&msg);
        }
    }

    /// Prompt ` >> ` (cyan, no newline).
    pub fn prompt(&mut self) {
        let s = if colored_control::SHOULD_COLORIZE.should_colorize() {
            format!("{}", " >> ".cyan())
        } else {
            " >> ".to_string()
        };
        let _ = self.backend.write_str(&s);
        let _ = self.backend.flush();
    }

    /// Permission confirmation.
    pub fn permission(&mut self, reason: &str, name: &str, input: &serde_json::Value) {
        let _ = self.emit(""); // leading blank line
        if colored_control::SHOULD_COLORIZE.should_colorize() {
            let _ = self.emit(&format!("{}", format!("[permission] {reason}").yellow()));
        } else {
            let _ = self.emit(&format!("[permission] {reason}"));
        }
        let _ = self.emit(&format!("   Tool: {}({})", name, input));
        let _ = self.backend.write_str("   Allow? [y/N] ");
        let _ = self.backend.flush();
    }

    /// ByteMaker startup logo: 5-row pixel wordmark, cyan; degrades to plain when `NO_COLOR` is set.
    /// Each glyph is a fixed 5 columns; rows join 9 letters (ByteMaker) with 2 spaces for alignment.
    /// `trim_end` strips only trailing placeholder spaces after the full row is assembled.
    pub fn logo(&mut self) {
        let glyphs: [[&str; 5]; 9] = [
            // B
            ["#### ", "#   #", "#### ", "#   #", "#### "],
            // y
            ["#   #", " # # ", "  #  ", "  #  ", "   # "],
            // t
            ["  #  ", "#####", "  #  ", "  #  ", "  ## "],
            // e
            [" ####", "#   #", "#####", "#    ", " ### "],
            // M
            ["#   #", "## ##", "# # #", "#   #", "#   #"],
            // a
            [" ### ", "#   #", "#####", "#   #", "#   #"],
            // k
            ["#   #", "#  # ", "###  ", "#  # ", "#   #"],
            // e
            [" ####", "#   #", "#####", "#    ", " ### "],
            // r
            ["#### ", "#  # ", "#    ", "#    ", "#    "],
        ];
        let mut rows: [String; 5] = Default::default();
        for glyph in &glyphs {
            for (i, row) in glyph.iter().enumerate() {
                if !rows[i].is_empty() {
                    rows[i].push_str("  ");
                }
                rows[i].push_str(row);
            }
        }
        // Keep full 5-column glyph width for alignment; strip trailing spaces only after row is assembled.
        for row in &mut rows {
            let len = row.trim_end().len();
            row.truncate(len);
        }
        let art = rows.join("\n");
        let _ = self.emit(&art);
    }

    /// Yellow heading + body: `\n## {title}\n{body}`.
    pub fn heading(&mut self, title: &str, body: &str) {
        let _ = self.emit(""); // leading blank line
        let title_line = format!("## {title}");
        if colored_control::SHOULD_COLORIZE.should_colorize() {
            let _ = self.emit(&format!("{}", title_line.yellow()));
        } else {
            let _ = self.emit(&title_line);
        }
        let _ = self.emit(body);
    }

    /// Render tool output (collapse + truncate).
    pub fn render_tool_output(&mut self, name: &str, result: &str, color: bool) {
        // Double color gate: color only when caller's `color` (by NO_COLOR) and `should_colorize`
        // (by TTY/NO_COLOR) are both true; never touch global `set_override` to avoid disabling
        // all later colored output.
        const TRUNCATE_AT: usize = 200;
        let size = if result.len() < 1024 {
            format!("{} B", result.len())
        } else if result.len() < 1024 * 1024 {
            format!("{:.1} KB", result.len() as f64 / 1024.0)
        } else {
            format!("{:.1} MB", result.len() as f64 / (1024.0 * 1024.0))
        };

        // Collapse newlines to spaces, trim whitespace
        let collapsed: String = result
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
            .collect::<String>()
            .trim()
            .to_string();
        let total = collapsed.chars().count();
        let (content, truncated) = if total > TRUNCATE_AT {
            let s: String = collapsed.chars().take(TRUNCATE_AT).collect();
            (format!("{s}…"), true)
        } else {
            (collapsed, false)
        };

        let prefix = format!("↳ {name} 结果 ({size}): ");
        let _ = self.emit(&format!(
            "{}{}",
            if color && colored_control::SHOULD_COLORIZE.should_colorize() {
                format!("{}", prefix.dimmed())
            } else {
                prefix
            },
            content
        ));

        if truncated {
            let trunc_msg = format!("  (已截断，共 {total} 字符)");
            let _ = self.emit(&format!(
                "{}",
                if color && colored_control::SHOULD_COLORIZE.should_colorize() {
                    format!("{}", trunc_msg.dimmed())
                } else {
                    trunc_msg
                }
            ));
        }
    }
}

/// Real terminal backend: synchronous byte-level writes via `io::stdout().lock()`.
pub struct CrosstermBackend;
impl CrosstermBackend {
    pub fn new() -> Self { Self }
}
impl Default for CrosstermBackend {
    fn default() -> Self { Self::new() }
}
impl Backend for CrosstermBackend {
    fn write_str(&mut self, s: &str) -> io::Result<()> {
        io::Write::write_all(&mut io::stdout().lock(), s.as_bytes())
    }
    fn flush(&mut self) -> io::Result<()> {
        io::stdout().lock().flush()
    }
}
