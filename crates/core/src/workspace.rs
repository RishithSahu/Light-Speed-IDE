//! Workspace filesystem access (specification sections 9.2, 31).
//!
//! The workspace owns file bytes and file metadata. It does not own unsaved
//! editor content: documents hold that. Every filesystem call the editor makes
//! goes through here, which is what keeps path and durability semantics in one
//! place instead of scattered through the UI.
//!
//! `enumerate_children` is lazy by contract: it lists exactly one directory
//! level. Recursive traversal is a scheduler-managed background task and does
//! not exist in Stage 1.

use crate::document::DiskStamp;
use crate::error::{PersistenceError, WorkspaceError};
use ls_platform::{fsops, CanonicalPath};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}

/// One child of a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub kind: EntryKind,
    pub len_bytes: u64,
}

/// Identity of an open workspace.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkspaceId(u64);

impl WorkspaceId {
    pub const fn new(value: u64) -> Self {
        WorkspaceId(value)
    }
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Filesystem access rooted at an optional project directory.
#[derive(Clone, Debug)]
pub struct Workspace {
    id: WorkspaceId,
    root: Option<CanonicalPath>,
}

impl Default for Workspace {
    fn default() -> Self {
        Workspace::rootless()
    }
}

impl Workspace {
    /// A workspace with no project directory: single files can still be opened.
    pub fn rootless() -> Self {
        Workspace { id: WorkspaceId::new(0), root: None }
    }

    pub fn with_root(id: WorkspaceId, root: CanonicalPath) -> Self {
        Workspace { id, root: Some(root) }
    }

    pub fn id(&self) -> WorkspaceId {
        self.id
    }

    pub fn root(&self) -> Option<&CanonicalPath> {
        self.root.as_ref()
    }

    /// Path as it should be shown to a human: relative to the root when it is
    /// inside the project, absolute otherwise (specification section 7).
    pub fn display_path(&self, path: &CanonicalPath) -> String {
        self.root
            .as_ref()
            .and_then(|root| path.relative_to(root))
            .unwrap_or_else(|| path.display_string())
    }

    /// Reads a whole file.
    pub fn read_file(&self, path: &Path) -> Result<Vec<u8>, WorkspaceError> {
        std::fs::read(path).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => WorkspaceError::NotFound(path.to_path_buf()),
            _ => WorkspaceError::Io { path: path.to_path_buf(), source },
        })
    }

    /// Reads a file in chunks, stopping early when `cancelled` says so.
    ///
    /// `Ok(None)` means the read was cancelled. A whole-file `read` cannot be
    /// interrupted, so a 100 MB load would otherwise ignore cancellation until
    /// it finished; chunking bounds that to roughly one chunk
    /// (`crate::loading::READ_CHUNK`).
    pub fn read_file_cancellable(
        &self,
        path: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<Vec<u8>>, WorkspaceError> {
        use std::io::Read;

        let to_error = |source: std::io::Error| match source.kind() {
            std::io::ErrorKind::NotFound => WorkspaceError::NotFound(path.to_path_buf()),
            _ => WorkspaceError::Io { path: path.to_path_buf(), source },
        };

        let mut file = std::fs::File::open(path).map_err(to_error)?;
        let expected = file.metadata().map(|meta| meta.len() as usize).unwrap_or(0);
        let mut contents = Vec::with_capacity(expected);
        let mut chunk = vec![0u8; crate::loading::READ_CHUNK];

        loop {
            if cancelled() {
                return Ok(None);
            }
            let read = file.read(&mut chunk).map_err(to_error)?;
            if read == 0 {
                return Ok(Some(contents));
            }
            contents.extend_from_slice(&chunk[..read]);
        }
    }

    /// Durably replaces a file (specification section 29).
    pub fn write_file_atomic(&self, path: &Path, contents: &[u8]) -> Result<(), PersistenceError> {
        fsops::write_file_atomic(path, contents).map_err(PersistenceError::Platform)
    }

    /// Durably replaces a file, streaming the contents instead of buffering
    /// them, so saving a large document does not double its memory.
    pub fn write_file_atomic_with<F>(&self, path: &Path, write: F) -> Result<(), PersistenceError>
    where
        F: FnOnce(&mut BufWriter<File>) -> std::io::Result<()>,
    {
        fsops::write_file_atomic_with(path, write).map_err(PersistenceError::Platform)
    }

    /// Size and modification time of a file, for external-change detection.
    pub fn stamp(&self, path: &Path) -> Result<DiskStamp, WorkspaceError> {
        let metadata = std::fs::metadata(path).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => WorkspaceError::NotFound(path.to_path_buf()),
            _ => WorkspaceError::Io { path: path.to_path_buf(), source },
        })?;
        Ok(DiskStamp { modified: metadata.modified().ok(), len_bytes: metadata.len() })
    }

    /// Lists one directory level. Never recurses (specification section 31).
    pub fn enumerate_children(&self, directory: &Path) -> Result<Vec<FileEntry>, WorkspaceError> {
        let metadata = std::fs::metadata(directory).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => WorkspaceError::NotFound(directory.to_path_buf()),
            _ => WorkspaceError::Io { path: directory.to_path_buf(), source },
        })?;
        if !metadata.is_dir() {
            return Err(WorkspaceError::NotADirectory(directory.to_path_buf()));
        }

        let reader = std::fs::read_dir(directory)
            .map_err(|source| WorkspaceError::Io { path: directory.to_path_buf(), source })?;

        let mut entries = Vec::new();
        for entry in reader {
            let entry = match entry {
                Ok(entry) => entry,
                // A single unreadable entry must not fail the whole listing.
                Err(_) => continue,
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            let kind = if file_type.is_dir() {
                EntryKind::Directory
            } else if file_type.is_symlink() {
                EntryKind::Symlink
            } else {
                EntryKind::File
            };
            let len_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            entries.push(FileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path(),
                kind,
                len_bytes,
            });
        }

        // Directories first, then files, each alphabetically: a stable order the
        // UI can present without sorting again.
        entries.sort_by(|a, b| match (a.kind, b.kind) {
            (EntryKind::Directory, EntryKind::Directory) => a.name.cmp(&b.name),
            (EntryKind::Directory, _) => std::cmp::Ordering::Less,
            (_, EntryKind::Directory) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lightspeed-workspace-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_and_writes_files() {
        let dir = scratch("io");
        let file = dir.join("a.txt");
        let workspace = Workspace::rootless();

        workspace.write_file_atomic(&file, b"hello").unwrap();
        assert_eq!(workspace.read_file(&file).unwrap(), b"hello");

        let stamp = workspace.stamp(&file).unwrap();
        assert_eq!(stamp.len_bytes, 5);
    }

    #[test]
    fn reading_a_missing_file_is_typed() {
        let workspace = Workspace::rootless();
        let error = workspace.read_file(Path::new("definitely-not-here.txt")).unwrap_err();
        assert!(matches!(error, WorkspaceError::NotFound(_)));
    }

    #[test]
    fn enumerate_children_lists_one_level_only() {
        let dir = scratch("enumerate");
        std::fs::create_dir_all(dir.join("nested/deeper")).unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        std::fs::write(dir.join("a.txt"), b"aa").unwrap();
        std::fs::write(dir.join("nested/hidden.txt"), b"deep").unwrap();

        let workspace = Workspace::rootless();
        let entries = workspace.enumerate_children(&dir).unwrap();

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["nested", "a.txt", "b.txt"], "directories first, then files");
        assert!(
            !names.contains(&"hidden.txt"),
            "traversal must be lazy: nested files are not listed"
        );
        assert_eq!(entries[0].kind, EntryKind::Directory);
        assert_eq!(entries[1].len_bytes, 2);
    }

    #[test]
    fn enumerating_a_file_is_an_error() {
        let dir = scratch("not-a-dir");
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let workspace = Workspace::rootless();
        assert!(matches!(
            workspace.enumerate_children(&file),
            Err(WorkspaceError::NotADirectory(_))
        ));
    }

    #[test]
    fn display_path_is_relative_inside_the_root() {
        let dir = scratch("display");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let file = dir.join("src/main.rs");
        std::fs::write(&file, b"fn main() {}").unwrap();

        let root = CanonicalPath::new(&dir).unwrap();
        let workspace = Workspace::with_root(WorkspaceId::new(1), root);
        let canonical = CanonicalPath::new(&file).unwrap();

        let display = workspace.display_path(&canonical);
        assert!(display.ends_with("main.rs"));
        assert!(!display.contains("lightspeed-workspace"), "should be root-relative: {display}");
    }

    #[test]
    fn display_path_falls_back_to_absolute_outside_the_root() {
        let dir = scratch("outside");
        let root = CanonicalPath::new(&dir).unwrap();
        let workspace = Workspace::with_root(WorkspaceId::new(1), root);

        let elsewhere = CanonicalPath::unverified(if cfg!(windows) {
            r"C:\somewhere\else\file.txt"
        } else {
            "/somewhere/else/file.txt"
        })
        .unwrap();
        assert_eq!(workspace.display_path(&elsewhere), elsewhere.display_string());
    }
}
