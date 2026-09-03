//! A basic command runner panel (item 10).
//!
//! **This transport is a deliberate, documented choice, not a placeholder for
//! "real terminal support later" that nobody wrote down.** See
//! `docs/adr/ADR-0016-terminal-transport.md` for the comparison against a PTY
//! (ConPTY on Windows) and why that is deferred rather than rushed.
//!
//! **Scoped deliberately, not a full terminal emulator.** A real terminal
//! needs a pseudo-console (ConPTY on Windows, a PTY elsewhere) so the child
//! process believes it has an interactive terminal, plus an ANSI/VT100
//! interpreter to turn cursor-positioning escape sequences into a 2D screen
//! buffer. Both are substantial subsystems on their own. What is built here
//! instead: a child process with piped stdio, a scrolling text log of its
//! output with escape sequences stripped (not interpreted) for readability,
//! and a line to type commands into. Line-oriented tools (most CLIs) work
//! fine; full-screen programs (an editor, `htop`) will print garbage, because
//! nothing here draws a 2D screen -- there is no PTY to make them think they
//! have one.
//!
//! The child's stdout/stderr are read on dedicated threads (this file is on
//! the worker allow-list for exactly that reason): reading a pipe blocks, and
//! the scheduler's task model is one-shot work, not a standing pump. The
//! threads only ever append bytes to a shared buffer and call a waker --
//! every byte is interpreted and every state change is applied on the
//! event-loop thread, the same rule background document loads follow.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

/// Bytes of scrollback kept. Older output is dropped, not the process's
/// actual output -- this bounds memory for a long-running or noisy command.
const MAX_SCROLLBACK_BYTES: usize = 512 * 1024;

pub struct Terminal {
    child: Child,
    stdin: ChildStdin,
    output: Arc<Mutex<Vec<u8>>>,
    alive: bool,
}

impl Terminal {
    /// Spawns the platform shell. `wake` is called from a reader thread every
    /// time output arrives, so the caller can post a wakeup to the event
    /// loop without the reader thread touching any editor state itself.
    pub fn spawn(wake: impl Fn() + Send + Clone + 'static) -> std::io::Result<Self> {
        let shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
        let mut child = Command::new(shell)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let output = Arc::new(Mutex::new(Vec::new()));

        for pipe in [
            child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
            child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let output = output.clone();
            let wake = wake.clone();
            std::thread::Builder::new()
                .name("terminal-reader".to_string())
                .spawn(move || read_loop(pipe, output, wake))
                .expect("spawning a reader thread should not fail");
        }

        Ok(Terminal { child, stdin, output, alive: true })
    }

    /// Sends one line of input, as if the user had typed it and pressed
    /// Enter.
    pub fn send_line(&mut self, line: &str) {
        let _ = writeln!(self.stdin, "{line}");
    }

    /// Takes whatever output has arrived since the last call, stripped of
    /// ANSI escape sequences, as text.
    pub fn drain_output(&self) -> String {
        let mut buffer = self.output.lock().unwrap();
        if buffer.is_empty() {
            return String::new();
        }
        let text = strip_ansi(&buffer);
        buffer.clear();
        text
    }

    /// Whether the shell process is still running. Checked lazily rather than
    /// tracked by the reader threads, so a check costs one `waitpid`/handle
    /// query and nothing when nobody asks.
    pub fn is_alive(&mut self) -> bool {
        if !self.alive {
            return false;
        }
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.alive = false;
                false
            }
            Ok(None) => true,
            Err(_) => {
                self.alive = false;
                false
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_loop(mut pipe: Box<dyn Read + Send>, output: Arc<Mutex<Vec<u8>>>, wake: impl Fn()) {
    let mut chunk = [0u8; 4096];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(count) => {
                let mut buffer = output.lock().unwrap();
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.len() > MAX_SCROLLBACK_BYTES {
                    let overflow = buffer.len() - MAX_SCROLLBACK_BYTES;
                    buffer.drain(..overflow);
                }
                drop(buffer);
                wake();
            }
        }
    }
}

/// Removes ANSI/VT100 escape sequences (colors, cursor movement) rather than
/// interpreting them, so raw bytes like `\x1b[32m` do not show up as visual
/// noise in a plain scrolling log.
fn strip_ansi(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI sequence: parameters and intermediates, then one
                    // final byte in 0x40..=0x7E.
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC sequence: runs until BEL or ESC \.
                    for next in chars.by_ref() {
                        if next == '\u{7}' {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
            continue;
        }
        if c == '\r' {
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through_unchanged() {
        assert_eq!(strip_ansi(b"hello world\n"), "hello world\n");
    }

    #[test]
    fn a_color_escape_sequence_is_removed() {
        assert_eq!(strip_ansi(b"\x1b[32mgreen\x1b[0m plain"), "green plain");
    }

    #[test]
    fn carriage_returns_are_dropped() {
        assert_eq!(strip_ansi(b"line one\r\nline two\r\n"), "line one\nline two\n");
    }

    #[test]
    fn an_osc_sequence_is_removed() {
        // A "set window title" sequence, terminated by BEL.
        assert_eq!(strip_ansi(b"\x1b]0;title\x07after"), "after");
    }

    #[test]
    fn spawning_and_running_one_command_produces_output() {
        let wake_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = wake_count.clone();
        let mut terminal = Terminal::spawn(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
        .expect("the platform shell should be available in CI and dev");

        terminal.send_line("echo hello-terminal");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut collected = String::new();
        while !collected.contains("hello-terminal") && std::time::Instant::now() < deadline {
            collected.push_str(&terminal.drain_output());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(collected.contains("hello-terminal"), "got: {collected:?}");
        assert!(wake_count.load(std::sync::atomic::Ordering::SeqCst) > 0);

        terminal.send_line("exit");
    }
}
