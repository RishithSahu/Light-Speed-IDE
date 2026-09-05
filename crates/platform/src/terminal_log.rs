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
}
