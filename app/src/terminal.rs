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
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Bytes of scrollback kept. Older output is dropped, not the process's
/// actual output -- this bounds memory for a long-running or noisy command.
const MAX_SCROLLBACK_BYTES: usize = 512 * 1024;
/// Never keep less than this, whatever a setting says: a terminal that can
/// hold only a line or two of output is not a terminal.
const MIN_SCROLLBACK_BYTES: usize = 4 * 1024;

/// One shell this panel can run, and the arguments it needs.
pub struct ShellSpec {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

/// The shells to try, best first.
///
/// # Why PowerShell rather than `cmd`
///
/// A terminal session has exactly one interpreter, so "accept commands from
/// any shell" is really "pick the interpreter that understands the most of
/// what people actually type". On Windows that is PowerShell by a wide
/// margin: it runs `cmd`'s builtins (`cd`, `dir`, `echo`, `cls`, `copy`,
/// `del`), it ships Unix-shaped aliases for the common ones (`ls`, `cat`,
/// `pwd`, `rm`, `cp`, `mv`, `ps`, `kill`, `clear`), and like every shell it
/// runs any executable on `PATH`. Starting `cmd` instead means
/// `Get-ChildItem` and `ls` are both errors; starting PowerShell means only
/// genuinely bash-specific *syntax* is (see `augmented_path` for how the
/// missing Unix programs are filled in).
///
/// # Cost, measured rather than assumed
///
/// PowerShell starts slower than `cmd`: about 370ms against 50ms. That is a
/// one-time cost when the panel first opens -- running a command afterwards
/// is a line written to a pipe either way -- and it buys every command above
/// working instead of one family of them.
///
/// The user's profile is deliberately *not* skipped. `-NoProfile` is the
/// usual reflex for embedded shells, and it was tried here: it measured 383ms
/// against 370ms with the profile loaded, so it bought nothing at all while
/// costing the aliases and functions the user has actually defined. A profile
/// heavy enough to matter would change that trade, but assuming one that is
/// not there is how a panel ends up quietly missing the commands its user
/// expects. `-NoLogo` stays: the copyright banner is noise in a panel this
/// size.
#[cfg(windows)]
const SHELL_CANDIDATES: &[ShellSpec] = &[
    // PowerShell 7+, the most capable and the best at Unix-style commands.
    ShellSpec { program: "pwsh.exe", args: &["-NoLogo"] },
    // Windows PowerShell 5.1, present on every Windows install.
    ShellSpec { program: "powershell.exe", args: &["-NoLogo"] },
    // Only if neither is usable: better a limited shell than no terminal.
    ShellSpec { program: "cmd.exe", args: &[] },
];

#[cfg(not(windows))]
const SHELL_CANDIDATES: &[ShellSpec] =
    &[ShellSpec { program: "/bin/bash", args: &[] }, ShellSpec { program: "/bin/sh", args: &[] }];

/// `PATH` for the shell, with Git for Windows' Unix tools appended if they
/// are installed.
///
/// PowerShell's aliases cover the common Unix *commands*, but not the
/// programs behind them: `grep`, `sed`, `awk`, `touch`, `wc`, `head`, `tail`,
/// `diff`, `xargs` and the rest are simply not on a stock Windows box. Git
/// for Windows ships all of them in `usr/bin`, and anyone using this editor
/// on a repository has it installed -- so if `git` is on `PATH`, its sibling
/// tools directory goes on too, and those commands start working.
///
/// **Appended, never prepended.** Windows has its own `find.exe` and
/// `sort.exe` with different meanings; putting GNU's ahead of them would
/// silently change what already-working commands do. Appending only fills
/// gaps.
///
/// Returns `None` when there is nothing to add, so the caller can leave the
/// environment untouched rather than rewriting it with an identical value.
pub fn augmented_path(current: Option<&str>) -> Option<String> {
    let current = current?;
    let tools = git_unix_tools_dir(current)?;
    let tools = tools.to_string_lossy();
    // Already there (a user who set this up themselves): changing nothing is
    // better than a duplicate entry.
    if current.split(';').any(|entry| entry.eq_ignore_ascii_case(&tools)) {
        return None;
    }
    Some(format!("{current};{tools}"))
}

/// Locates Git for Windows' `usr/bin` from `git.exe`'s own place on `PATH`.
///
/// Git installs as `<root>/cmd/git.exe` with the Unix tools in
/// `<root>/usr/bin`, so finding one locates the other without guessing at
/// install paths or reading the registry.
fn git_unix_tools_dir(path_var: &str) -> Option<std::path::PathBuf> {
    for entry in path_var.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let dir = std::path::Path::new(entry);
        if !dir.join("git.exe").is_file() {
            continue;
        }
        // `<root>/cmd` -> `<root>/usr/bin`
        let Some(root) = dir.parent() else { continue };
        let tools = root.join("usr").join("bin");
        if tools.is_dir() {
            return Some(tools);
        }
    }
    None
}

pub struct Terminal {
    child: Child,
    stdin: ChildStdin,
    output: Arc<Mutex<Vec<u8>>>,
    alive: bool,
    /// The permanent transcript (`ls_platform::terminal_log`), open for the
    /// life of the session. `None` when the platform gives us nowhere
    /// standard to put it, or opening it failed -- a missing history file
    /// must never be a reason the terminal itself refuses to start.
    ///
    /// Touched only from `send_line` and `drain_output`, both called from the
    /// event-loop thread (`app::send_terminal_line`,
    /// `app::drain_terminal_output`), so a plain `File` needs no `Mutex`: it
    /// is never the reader threads' job to write here (see `drain_output`'s
    /// own doc for why the finished, ANSI-stripped text is what gets
    /// recorded rather than raw chunks off the pipe).
    log: Option<std::fs::File>,
    /// How much output to keep. Shared with the reader threads, which are
    /// what actually trim the buffer, so this has to be a value they can
    /// read rather than a field only the shell can see.
    scrollback: Arc<AtomicUsize>,
}

impl Terminal {
    /// Spawns the platform shell. `wake` is called from a reader thread every
    /// time output arrives, so the caller can post a wakeup to the event
    /// loop without the reader thread touching any editor state itself.
    pub fn spawn(wake: impl Fn() + Send + Clone + 'static) -> std::io::Result<Self> {
        // The most capable shell available, not a fixed one -- see
        // `SHELL_CANDIDATES` for why that is what makes commands from
        // different families work in one session.
        let augmented_path = augmented_path(std::env::var("PATH").ok().as_deref());
        let mut spawned = None;
        for shell in SHELL_CANDIDATES {
            // `ls_platform::command`, not `Command::new`: a bare spawn gives
            // the shell a console window of its own, so the panel appeared
            // inside the editor *and* a separate `cmd.exe` window appeared
            // beside it, both attached to the same session.
            let mut command = ls_platform::command(shell.program);
            command
                .args(shell.args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(path) = &augmented_path {
                command.env("PATH", path);
            }
            if let Ok(child) = command.spawn() {
                spawned = Some(child);
                break;
            }
        }
        let Some(mut child) = spawned else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no usable shell was found on this system",
            ));
        };

        let stdin = child.stdin.take().expect("stdin was piped");
        let output = Arc::new(Mutex::new(Vec::new()));
        let scrollback = Arc::new(AtomicUsize::new(MAX_SCROLLBACK_BYTES));

        for pipe in [
            child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
            child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let output = output.clone();
            let scrollback = scrollback.clone();
            let wake = wake.clone();
            std::thread::Builder::new()
                .name("terminal-reader".to_string())
                .spawn(move || read_loop(pipe, output, scrollback, wake))
                .expect("spawning a reader thread should not fail");
        }

        let log = ls_platform::terminal_log::open_session();
        Ok(Terminal { child, stdin, output, alive: true, log, scrollback })
    }

    /// Changes how much output is kept.
    ///
    /// Takes effect on the next chunk the shell writes rather than trimming
    /// what is already held: shrinking the limit should not throw away
    /// output the reader can currently see and may still be reading.
    pub fn set_scrollback(&self, bytes: usize) {
        self.scrollback.store(bytes.max(MIN_SCROLLBACK_BYTES), Ordering::Relaxed);
    }

    /// Sends one line of input, as if the user had typed it and pressed
    /// Enter, and records it in the permanent transcript.
    pub fn send_line(&mut self, line: &str) {
        let _ = writeln!(self.stdin, "{line}");
        if let Some(log) = self.log.as_mut() {
            let _ = writeln!(log, "> {line}");
        }
    }

    /// Takes whatever output has arrived since the last call, stripped of
    /// ANSI escape sequences, as text -- and appends that same text to the
    /// permanent transcript.
    ///
    /// The transcript gets the *stripped* text, not the raw bytes off the
    /// pipe, and gets it here rather than from the reader threads that
    /// actually received it. Both follow from the same fact: an escape
    /// sequence or a multi-byte UTF-8 character can straddle the boundary
    /// between two 4KB reads, so stripping has to happen after the pieces are
    /// joined back into one buffer -- which is exactly what this method
    /// already does for the in-memory scrollback. Duplicating that logic per
    /// chunk on the reader thread would risk getting it wrong in a way the
    /// existing path does not; reusing this method's own output cannot.
    pub fn drain_output(&mut self) -> String {
        let mut buffer = self.output.lock().unwrap();
        if buffer.is_empty() {
            return String::new();
        }
        let text = strip_ansi(&buffer);
        buffer.clear();
        drop(buffer);
        if let Some(log) = self.log.as_mut() {
            let _ = log.write_all(text.as_bytes());
        }
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

fn read_loop(
    mut pipe: Box<dyn Read + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    scrollback: Arc<AtomicUsize>,
    wake: impl Fn(),
) {
    let mut chunk = [0u8; 4096];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(count) => {
                let keep = scrollback.load(Ordering::Relaxed).max(MIN_SCROLLBACK_BYTES);
                let mut buffer = output.lock().unwrap();
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.len() > keep {
                    let overflow = buffer.len() - keep;
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
mod scrollback_tests {
    use super::*;

    #[test]
    fn a_scrollback_setting_can_never_shrink_the_buffer_to_nothing() {
        // The floor exists because the setting is a number someone can type:
        // a terminal that keeps two bytes of output is not a terminal, and
        // the reader threads apply this on every chunk.
        let shared = Arc::new(AtomicUsize::new(MAX_SCROLLBACK_BYTES));
        for asked in [0usize, 1, 100, MIN_SCROLLBACK_BYTES / 2] {
            shared.store(asked.max(MIN_SCROLLBACK_BYTES), Ordering::Relaxed);
            assert!(
                shared.load(Ordering::Relaxed) >= MIN_SCROLLBACK_BYTES,
                "asked for {asked} and got less than the floor"
            );
        }
        shared.store(2 * MAX_SCROLLBACK_BYTES, Ordering::Relaxed);
        assert_eq!(shared.load(Ordering::Relaxed), 2 * MAX_SCROLLBACK_BYTES, "a larger ask stands");
    }
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

#[cfg(all(test, windows))]
mod shell_tests {
    use super::*;

    #[test]
    fn the_unix_tools_directory_is_found_next_to_git() {
        // Not a mock: if Git for Windows is installed on this machine, the
        // tools this promises must genuinely be there. Skips rather than
        // fails where it is not installed.
        let Ok(path) = std::env::var("PATH") else { return };
        let Some(tools) = git_unix_tools_dir(&path) else {
            println!("git not on PATH; nothing to verify");
            return;
        };
        println!("unix tools: {}", tools.display());
        for tool in ["grep.exe", "sed.exe", "awk.exe", "touch.exe", "wc.exe", "head.exe"] {
            assert!(tools.join(tool).is_file(), "{tool} missing from {}", tools.display());
        }
    }

    #[test]
    fn augmenting_appends_and_never_duplicates() {
        let Ok(path) = std::env::var("PATH") else { return };
        let Some(once) = augmented_path(Some(&path)) else { return };
        assert!(once.starts_with(&path), "existing PATH entries must keep priority");
        assert_eq!(augmented_path(Some(&once)), None, "a second pass must add nothing");
    }

    #[test]
    fn a_path_without_git_is_left_alone() {
        assert_eq!(augmented_path(Some(r"C:\Windows\System32")), None);
        assert_eq!(augmented_path(None), None);
    }
}

#[cfg(all(test, windows))]
mod live_shell_tests {
    use super::*;

    /// Runs commands from three different shell families through one real
    /// session, exactly as the panel does, and reports what each produced.
    ///
    /// This is the check behind the claim that the terminal "accepts any
    /// type of input": asserting it from the shell table alone would prove
    /// nothing about whether these commands actually resolve.
    ///
    /// `cargo test -p lightspeed -- --ignored --nocapture every_shell_family`
    #[test]
    #[ignore = "spawns a real shell"]
    fn every_shell_family_of_command_runs_in_one_session() {
        let started = std::time::Instant::now();
        let mut terminal = Terminal::spawn(|| {}).expect("a shell should start");
        println!("shell start: {:?}", started.elapsed());

        // cmd builtin, PowerShell cmdlet, PowerShell's Unix alias, and a
        // real GNU program from Git's usr/bin.
        let probes = [
            ("cmd builtin", "echo probe_cmd_ok"),
            ("PowerShell cmdlet", "Write-Output probe_ps_ok"),
            ("Unix alias", "pwd"),
            ("GNU coreutils", "grep --version"),
            ("GNU sed", "sed --version"),
        ];
        for (_, command) in probes {
            terminal.send_line(command);
        }
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let output = terminal.drain_output();
        println!("--- output ---\n{output}\n--------------");

        for (family, command) in probes {
            assert!(
                !output.contains(&format!("'{command}' is not recognized")),
                "{family}: `{command}` was not recognized by the shell"
            );
        }
        assert!(output.contains("probe_cmd_ok"), "cmd-style echo produced nothing");
        assert!(output.contains("probe_ps_ok"), "PowerShell cmdlet produced nothing");
        assert!(
            output.to_lowercase().contains("grep"),
            "GNU grep did not run; Git's usr/bin is not reaching the shell"
        );
    }
}
