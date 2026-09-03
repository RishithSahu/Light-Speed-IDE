//! Path semantics (specification section 7).
//!
//! Canonical identity and display path are different things. A [`CanonicalPath`]
//! is the identity used to decide whether two references mean the same file;
//! the display string is what a human reads. On Windows that means stripping
//! the `\\?\` verbatim prefix for display and comparing case-insensitively for
//! identity, while never hardcoding separators into higher-level logic.

use std::path::{Component, Path, PathBuf, Prefix};

/// A filesystem path in its identity form.
///
/// Two `CanonicalPath` values compare equal exactly when they denote the same
/// file for this platform's filesystem semantics, which is the property
/// `open_document()` relies on to return one document per file
/// (specification section 24).
#[derive(Clone, Debug)]
pub struct CanonicalPath {
    path: PathBuf,
    key: String,
}

impl CanonicalPath {
    /// Canonicalizes an existing path, resolving links, relative components and
    /// short (8.3) names.
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let canonical = std::fs::canonicalize(path.as_ref())?;
        Ok(Self::from_absolute(canonical))
    }

    /// Normalizes a path that does not exist yet (a Save As target) without
    /// touching the filesystem.
    pub fn unverified(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let absolute = std::path::absolute(path.as_ref())?;
        Ok(Self::from_absolute(absolute))
    }

    fn from_absolute(path: PathBuf) -> Self {
        let path = strip_verbatim(&path);
        let key = identity_key(&path);
        CanonicalPath { path, key }
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.path
    }

    /// Stable identity string; equal iff the paths denote the same file.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Human-readable form (no `\\?\` prefix).
    pub fn display_string(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    /// Final component, e.g. `main.rs`.
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.display_string())
    }

    pub fn extension(&self) -> Option<String> {
        self.path.extension().map(|e| e.to_string_lossy().into_owned())
    }

    pub fn parent(&self) -> Option<CanonicalPath> {
        self.path.parent().map(|p| CanonicalPath::from_absolute(p.to_path_buf()))
    }

    /// Path relative to a workspace root, for display. Returns `None` when the
    /// file lives outside the root.
    pub fn relative_to(&self, root: &CanonicalPath) -> Option<String> {
        let root_key = root.key();
        let self_key = self.key();
        if !self_key.starts_with(root_key) {
            return None;
        }
        let remainder = &self_key[root_key.len()..];
        let boundary_ok = remainder.is_empty()
            || remainder.starts_with(std::path::MAIN_SEPARATOR)
            || root_key.ends_with(std::path::MAIN_SEPARATOR);
        if !boundary_ok {
            return None; // `/foo/barn` must not look like it is inside `/foo/bar`
        }
        let components: Vec<_> = self.path.components().collect();
        let root_components = root.path.components().count();
        let relative: PathBuf = components.into_iter().skip(root_components).collect();
        Some(relative.to_string_lossy().into_owned())
    }
}

impl PartialEq for CanonicalPath {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for CanonicalPath {}

impl std::hash::Hash for CanonicalPath {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl std::fmt::Display for CanonicalPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display_string())
    }
}

/// Removes the Windows verbatim prefix that `canonicalize` adds, keeping UNC
/// paths in their familiar `\\server\share` form. A no-op elsewhere.
pub fn strip_verbatim(path: &Path) -> PathBuf {
    if !cfg!(windows) {
        return path.to_path_buf();
    }
    let text = path.as_os_str().to_string_lossy();
    let stripped = match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(_) => text.strip_prefix(r"\\?\").map(|s| s.to_string()),
            Prefix::VerbatimUNC(_, _) => text.strip_prefix(r"\\?\UNC\").map(|s| format!(r"\\{s}")),
            _ => None,
        },
        _ => None,
    };
    match stripped {
        Some(s) => PathBuf::from(s),
        None => path.to_path_buf(),
    }
}

/// Builds the identity key for a normalized absolute path.
///
/// Windows filesystems are case-insensitive in practice, so identity folds
/// case. (NTFS supports per-directory case sensitivity; treating those as
/// equal is a conservative choice that can merge two documents, never split
/// one, and is revisited if it ever matters.)
fn identity_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_prefix_is_stripped_for_display() {
        if cfg!(windows) {
            let path = Path::new(r"\\?\C:\src\main.rs");
            assert_eq!(strip_verbatim(path), PathBuf::from(r"C:\src\main.rs"));
            let unc = Path::new(r"\\?\UNC\server\share\file.txt");
            assert_eq!(strip_verbatim(unc), PathBuf::from(r"\\server\share\file.txt"));
        }
    }

    #[test]
    fn plain_paths_are_untouched() {
        let path = Path::new(if cfg!(windows) { r"C:\src\main.rs" } else { "/src/main.rs" });
        assert_eq!(strip_verbatim(path), path.to_path_buf());
    }

    #[test]
    fn identity_ignores_case_on_windows() {
        let a = CanonicalPath::unverified(if cfg!(windows) {
            r"C:\Src\Main.rs"
        } else {
            "/src/main.rs"
        })
        .unwrap();
        let b = CanonicalPath::unverified(if cfg!(windows) {
            r"C:\src\main.rs"
        } else {
            "/src/main.rs"
        })
        .unwrap();
        assert_eq!(a, b);
        assert_eq!(a.key(), b.key());
    }

    #[test]
    fn relative_to_root_is_display_only() {
        let root =
            CanonicalPath::unverified(if cfg!(windows) { r"C:\proj" } else { "/proj" }).unwrap();
        let file = CanonicalPath::unverified(if cfg!(windows) {
            r"C:\proj\src\a.rs"
        } else {
            "/proj/src/a.rs"
        })
        .unwrap();
        let relative = file.relative_to(&root).unwrap();
        assert!(relative.ends_with("a.rs"), "{relative}");
        assert!(relative.contains("src"), "{relative}");
    }

    #[test]
    fn sibling_prefix_is_not_inside_root() {
        let root =
            CanonicalPath::unverified(if cfg!(windows) { r"C:\proj" } else { "/proj" }).unwrap();
        let outside = CanonicalPath::unverified(if cfg!(windows) {
            r"C:\project\a.rs"
        } else {
            "/project/a.rs"
        })
        .unwrap();
        assert_eq!(outside.relative_to(&root), None);
    }

    #[test]
    fn file_name_and_extension_are_available() {
        let file = CanonicalPath::unverified(if cfg!(windows) {
            r"C:\proj\src\main.rs"
        } else {
            "/proj/src/main.rs"
        })
        .unwrap();
        assert_eq!(file.file_name(), "main.rs");
        assert_eq!(file.extension().as_deref(), Some("rs"));
    }

    #[test]
    fn two_references_to_one_file_share_identity() {
        let dir = std::env::temp_dir().join("lightspeed-path-identity-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("doc.txt");
        std::fs::write(&file, b"x").unwrap();

        let direct = CanonicalPath::new(&file).unwrap();
        let indirect = CanonicalPath::new(dir.join(".").join("doc.txt")).unwrap();
        assert_eq!(direct, indirect);

        std::fs::remove_file(&file).ok();
    }
}
