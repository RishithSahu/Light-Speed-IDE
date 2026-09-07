//! Where the settings someone chose are kept.
//!
//! Two files, the same shape as the configuration layering already in
//! `ls_core::config`: one for the person, one for the project. The workspace
//! file sits inside the repository so a team can commit the tab width they
//! agreed on; the user file is theirs alone and follows them between
//! projects.
//!
//! This module is only the filing. What a settings file may contain, and what
//! happens to a value outside its range, belongs to `ls_core::settings`.
//!
//! Losing one of these costs a return to defaults, never a document, so a
//! plain overwrite is proportionate -- the same reasoning as
//! [`crate::recents`], and the same reason this does not go through
//! [`crate::fsops`].

use std::path::{Path, PathBuf};

/// The per-user settings file, or `None` if the platform gives us nowhere
/// standard to put one.
pub fn user_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }?;
    Some(base.join("LightSpeed").join("settings.conf"))
}

/// The settings file belonging to one workspace.
pub fn workspace_path(root: &Path) -> PathBuf {
    root.join(".lightspeed").join("settings.conf")
}

/// Reads a settings file, or `None` if there is not one to read.
///
/// A missing file is the ordinary case -- someone who has never changed a
/// setting has no file -- so it is a `None`, never an error to handle.
pub fn load(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Writes a settings file, reporting whether it landed.
pub fn save(path: &Path, contents: &str) -> bool {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    std::fs::write(path, contents).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_file_sits_beside_the_other_preferences() {
        let path = user_path().expect("a config directory");
        assert!(path.to_string_lossy().contains("LightSpeed"), "{path:?}");
        assert!(path.ends_with("settings.conf"), "{path:?}");
    }

    #[test]
    fn the_workspace_file_sits_inside_the_workspace() {
        // So it can be committed: a team's tab width belongs with the code,
        // not in one person's home directory.
        let path = workspace_path(Path::new("/work/app"));
        assert!(path.starts_with("/work/app"), "{path:?}");
        assert!(path.ends_with("settings.conf"), "{path:?}");
    }

    #[test]
    fn what_was_saved_is_what_comes_back() {
        let path = std::env::temp_dir().join("lightspeed-settings-roundtrip").join("settings.conf");
        assert!(save(&path, "editor.fontSize = 18\n"), "the directory is writable");
        assert_eq!(load(&path).as_deref(), Some("editor.fontSize = 18\n"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_that_was_never_written_reads_back_as_nothing() {
        let path = std::env::temp_dir().join("lightspeed-settings-never-written.conf");
        let _ = std::fs::remove_file(&path);
        assert_eq!(load(&path), None);
    }
}
