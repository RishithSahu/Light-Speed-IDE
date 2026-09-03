//! Recently opened files.
//!
//! A small, best-effort preference list. It is not document content and does
//! not go through the workspace's atomic-write path (`fsops`) -- that path
//! exists so a crash mid-write never corrupts a file the user is editing, and
//! this file is neither: losing or corrupting it costs an empty "Open Recent"
//! list on next launch, not a manuscript. A plain overwrite is proportionate.
//!
//! Shell code is free to call this directly, the same way it already calls
//! [`crate::dialog`] directly: both are platform conveniences, not document
//! persistence.

use std::io;
use std::path::{Path, PathBuf};

/// Most recent files kept, oldest dropped once the list is full.
pub const MAX_RECENT: usize = 8;

/// Where the list lives, or `None` if the platform gives us nowhere standard.
pub fn default_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }?;
    Some(base.join("LightSpeed").join("recent_files.txt"))
}

/// Loads the list, most-recent first. A missing or unreadable file is an empty
/// list, not an error: losing recent-file history must never block startup.
pub fn load(path: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(path)
        .map(|content| {
            content
                .lines()
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .take(MAX_RECENT)
                .collect()
        })
        .unwrap_or_default()
}

/// Writes the list, most-recent first. Best-effort: the caller decides whether
/// a failure is worth surfacing, but it must never block on it.
pub fn save(path: &Path, files: &[PathBuf]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content: String =
        files.iter().take(MAX_RECENT).map(|p| p.to_string_lossy().into_owned() + "\n").collect();
    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch_path(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("lightspeed-recents-test-{name}-{unique}.txt"))
    }

    #[test]
    fn a_missing_file_loads_as_empty() {
        let path = scratch_path("missing");
        assert!(load(&path).is_empty());
    }

    #[test]
    fn saving_then_loading_round_trips_the_list() {
        let path = scratch_path("roundtrip");
        let files = vec![PathBuf::from("a.txt"), PathBuf::from("b.rs")];
        save(&path, &files).expect("write succeeds");
        assert_eq!(load(&path), files);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn loading_never_returns_more_than_the_cap() {
        let path = scratch_path("cap");
        let files: Vec<PathBuf> = (0..20).map(|n| PathBuf::from(format!("file{n}.txt"))).collect();
        save(&path, &files).expect("write succeeds");
        assert_eq!(load(&path).len(), MAX_RECENT);
        let _ = std::fs::remove_file(&path);
    }
}
