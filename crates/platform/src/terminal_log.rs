//! The built-in terminal's permanent transcript.
//!
//! Every command run in the panel, and the output it produced, is appended
//! here -- across every session, forever -- so something run last month is
//! still there to read (or `grep`) today, even though the panel's own
//! on-screen scrollback is capped and trims its oldest bytes as new output
//! arrives (`MAX_SCROLLBACK_BYTES` in `app/src/terminal.rs`). That trimming
//! is about what one panel can reasonably render; it says nothing about what
//! is worth keeping, which is everything.
//!
//! Same base directory as [`crate::recents`], and the opening lives here for
//! the same reason `dialog`, `recents` and `process` do: "the shell never
//! writes files itself" is an enforced architecture rule (see
//! `tests/tests/architecture.rs`'s `the_shell_never_writes_files_itself`),
//! so a raw `OpenOptions` has exactly one legal home, and this is it.
//! `app/src/terminal.rs` keeps the open handle for the life of a session
//! (opening and closing the file on every line, the way a rarely-written
//! preference list can get away with, is not what a streaming transcript
//! should do) but never constructs it directly.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where the transcript lives, or `None` if the platform gives us nowhere
/// standard.
pub fn default_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }?;
    Some(base.join("LightSpeed").join("terminal_history.log"))
}

/// Opens the transcript at the platform's standard location for a new
/// session. `None` if there is nowhere standard to put it, or opening it
/// failed.
pub fn open_session() -> Option<File> {
    open_session_at(&default_path()?)
}

/// [`open_session`], at an explicit path -- split out so a test (or a future
/// caller with its own reason to pick the location) is not forced through
/// `default_path`, which always names the one real, permanent transcript
/// every developer and CI machine has.
///
/// Appends (never truncates) a dated divider so the file itself shows where
/// one session ended and the next began -- the same reason a real terminal's
/// own scrollback growing forever is useful and a single unbroken wall of
/// text would not be.
///
/// `None` on any failure (the directory could not be created, the file could
/// not be opened): a terminal that refused to start because its *history*
/// file was unwritable would be a strange kind of broken, so this is silent
/// and best-effort, matching every other piece of convenience persistence in
/// this crate (`recents`).
pub fn open_session_at(path: &Path) -> Option<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
    let _ = writeln!(file, "\n=== session started {} ===", ls_log::timestamp_now());
    Some(file)
}

/// Every command line ever recorded to the permanent transcript, oldest
/// first, capped at `max` entries -- this is what lets Up-arrow recall reach
/// back across every past session of this app's terminal, not only the
/// current process's lifetime. `Terminal::send_line` writes each one as
/// `"> {line}"`; this is the inverse read.
pub fn read_history(max: usize) -> Vec<String> {
    let Some(path) = default_path() else { return Vec::new() };
    read_history_at(&path, max)
}

fn read_history_at(path: &Path, max: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let lines = text.lines().filter_map(|line| line.strip_prefix("> ").map(str::to_string));
    capped(lines, max)
}

/// The last `max` of an iterator of lines, oldest first -- shared by
/// [`read_history`] (which first strips the `"> "` marker) and
/// [`read_external_shell_history`] (whose lines need no stripping).
fn capped(lines: impl Iterator<Item = String>, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = lines.collect();
    if lines.len() > max {
        let cut = lines.len() - max;
        lines.drain(..cut);
    }
    lines
}

/// PowerShell's own command history (written by the PSReadLine module every
/// real PowerShell window already uses), so "commands run before" is not
/// scoped to this app -- a command typed in an ordinary PowerShell window
/// shows up here too. Windows-only: PSReadLine, and this history file, are a
/// Windows PowerShell/`pwsh` concept.
#[cfg(windows)]
pub fn read_external_shell_history(max: usize) -> Vec<String> {
    let Some(path) = external_shell_history_path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    capped(text.lines().map(str::to_string), max)
}

#[cfg(not(windows))]
pub fn read_external_shell_history(_max: usize) -> Vec<String> {
    Vec::new()
}

#[cfg(windows)]
fn external_shell_history_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("PowerShell")
            .join("PSReadLine")
            .join("ConsoleHost_history.txt"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch_path(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("lightspeed-termlog-test-{name}-{unique}.log"))
    }

    #[test]
    fn the_path_sits_beside_recents_own_file_not_inside_a_workspace() {
        let Some(path) = default_path() else { return };
        assert_eq!(path.file_name().unwrap(), "terminal_history.log");
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "LightSpeed");
    }

    #[test]
    fn opening_a_session_creates_the_file_and_its_parent_directory() {
        let path = scratch_path("create").join("nested").join("transcript.log");
        let file = open_session_at(&path);
        assert!(file.is_some(), "opening a writable scratch path must succeed");
        drop(file);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("=== session started "), "got: {content:?}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_second_session_appends_rather_than_replacing_the_first() {
        // A permanent record that reset itself on every relaunch would not
        // be permanent. Opening it twice, as two sessions would, must grow
        // the file, never replace it.
        let path = scratch_path("reopen");
        drop(open_session_at(&path));
        let first_size = std::fs::metadata(&path).unwrap().len();
        drop(open_session_at(&path));
        let second_size = std::fs::metadata(&path).unwrap().len();
        assert!(second_size > first_size, "a second session must add to the file, not replace it");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content.matches("=== session started ").count(),
            2,
            "both sessions' dividers must survive: {content:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn writes_made_through_the_returned_handle_land_in_the_file() {
        let path = scratch_path("writes");
        let mut file = open_session_at(&path).expect("scratch path is writable");
        writeln!(file, "> echo probe").unwrap();
        writeln!(file, "probe").unwrap();
        drop(file);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("> echo probe"), "the command line is missing: {content:?}");
        assert!(content.contains("probe"), "the output is missing: {content:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reading_history_back_extracts_only_the_command_lines() {
        let path = scratch_path("history");
        std::fs::write(&path, "> git status\nOn branch main\n> cargo build\nCompiling...\n").unwrap();
        assert_eq!(read_history_at(&path, 10), vec!["git status".to_string(), "cargo build".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reading_history_caps_to_the_most_recent_entries() {
        let path = scratch_path("history-cap");
        let content = (0..5).map(|n| format!("> cmd{n}\n")).collect::<String>();
        std::fs::write(&path, content).unwrap();
        assert_eq!(read_history_at(&path, 2), vec!["cmd3".to_string(), "cmd4".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_history_file_reads_as_empty_not_an_error() {
        let path = scratch_path("missing");
        assert_eq!(read_history_at(&path, 10), Vec::<String>::new());
    }
}
