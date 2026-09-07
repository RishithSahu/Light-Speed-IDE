//! Where a workspace's settled dependency graph is kept between sessions.
//!
//! Scanning a workspace and settling the force simulation costs a few
//! hundred milliseconds, and the answer only changes when the code's imports
//! do. Keeping it means opening the view is instant on every visit after the
//! first.
//!
//! This module is only the filing: it stores and returns text, and knows
//! nothing about what is in it. The encoding lives with the code that builds
//! the graph (`app::depgraph`), which is also what decides when a cache is
//! too stale to use.
//!
//! Losing or corrupting one of these files costs a rescan, never a document,
//! so a plain overwrite is proportionate -- the same reasoning as
//! [`crate::recents`], and the same reason this does not go through
//! [`crate::fsops`].

use std::path::{Path, PathBuf};

/// The directory the caches live in, or `None` if the platform gives us
/// nowhere standard to put them.
pub fn directory() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
    }?;
    Some(base.join("LightSpeed").join("graphs"))
}

/// The cache file for one workspace root.
///
/// Named for the folder so the directory can be read by a human, and
/// suffixed with a hash of the full path so two folders that share a name --
/// `~/work/app` and `~/play/app` -- never collide.
pub fn path_for(root: &Path) -> Option<PathBuf> {
    let directory = directory()?;
    let name: String = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string())
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
        .take(40)
        .collect();
    Some(directory.join(format!("{name}-{:016x}.graph", fingerprint(root))))
}

/// FNV-1a over the path's bytes. Hand-rolled because it is nine lines and
/// the alternative is a dependency; this is a filename, not a security
/// boundary, so all that is asked of it is that different paths rarely
/// collide.
fn fingerprint(root: &Path) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in root.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Reads the cache for `root`, or `None` if there is not one to read.
///
/// A missing or unreadable cache is an ordinary outcome -- the first visit to
/// a workspace, or a cache directory the user cleared -- so it is a `None`,
/// never an error the caller has to handle.
pub fn load(root: &Path) -> Option<String> {
    std::fs::read_to_string(path_for(root)?).ok()
}

/// Writes the cache for `root`, reporting whether it landed.
///
/// Best-effort on purpose: a read-only or missing cache directory should cost
/// a rescan next time, not interrupt what the user was doing.
pub fn save(root: &Path, contents: &str) -> bool {
    let Some(path) = path_for(root) else { return false };
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    std::fs::write(path, contents).is_ok()
}

/// Deletes the cache for `root`, so the next visit rescans.
pub fn forget(root: &Path) -> bool {
    path_for(root).is_some_and(|path| std::fs::remove_file(path).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_folders_of_the_same_name_get_different_files() {
        // The reason the filename carries a hash at all: `~/work/app` and
        // `~/play/app` are different workspaces with the same folder name,
        // and one must not read back the other's graph.
        let one = path_for(Path::new("/work/app")).expect("a cache directory");
        let other = path_for(Path::new("/play/app")).expect("a cache directory");
        assert_ne!(one, other);
        assert!(one.to_string_lossy().contains("app"), "named for the folder: {one:?}");
    }

    #[test]
    fn the_same_folder_always_gets_the_same_file() {
        assert_eq!(path_for(Path::new("/work/app")), path_for(Path::new("/work/app")));
    }

    #[test]
    fn a_folder_name_that_is_not_a_safe_filename_is_made_into_one() {
        let path = path_for(Path::new("/tmp/my project (v2)")).expect("a cache directory");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.contains(' '), "{name}");
        assert!(!name.contains('('), "{name}");
    }

    #[test]
    fn what_was_saved_is_what_comes_back() {
        let root = std::env::temp_dir().join("lightspeed-depgraph-cache-roundtrip");
        assert!(save(&root, "graph 1\nsome contents\n"), "the cache directory is writable");
        assert_eq!(load(&root).as_deref(), Some("graph 1\nsome contents\n"));
        assert!(forget(&root));
        assert_eq!(load(&root), None, "a forgotten cache is gone");
    }

    #[test]
    fn a_workspace_never_visited_reads_back_nothing_rather_than_failing() {
        let root = std::env::temp_dir().join("lightspeed-depgraph-cache-never-written");
        let _ = forget(&root);
        assert_eq!(load(&root), None);
    }
}
