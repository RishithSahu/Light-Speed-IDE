//! Shared helpers for the cross-crate test suites.
//!
//! The suites themselves live in `tests/tests/`:
//!
//! ```text
//! integration.rs   end-to-end editor behaviour through the public core API
//! architecture.rs  invariants the specification requires CI to enforce
//! regression.rs    behaviour that has to keep working, with its reason
//! ```

use ls_core::{EditorCore, EffectiveConfig};
use ls_platform::MemoryClipboard;
use std::path::{Path, PathBuf};

/// A temporary directory that deletes itself.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates an empty directory unique to this process and `name`.
    pub fn new(name: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("lightspeed-tests-{name}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("could not create the temporary directory");
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes a file inside the directory and returns its path.
    pub fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("could not create a parent directory");
        }
        std::fs::write(&path, contents).expect("could not write the test file");
        path
    }

    pub fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.path.join(name)).expect("could not read the test file")
    }

    pub fn read_string(&self, name: &str) -> String {
        String::from_utf8(self.read(name)).expect("test file is not UTF-8")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// An editor core with an in-process clipboard, so tests never touch the real
/// one and can run concurrently.
pub fn headless_editor() -> EditorCore {
    EditorCore::with_clipboard(EffectiveConfig::default(), Box::new(MemoryClipboard::new()))
}

/// Root of the repository, found from this crate's manifest directory.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the tests crate lives inside the workspace")
        .to_path_buf()
}

/// Every `.rs` file that ships as part of the product, excluding test modules'
/// enclosing crates only where noted by the caller.
pub fn source_files(relative_roots: &[&str]) -> Vec<PathBuf> {
    let root = workspace_root();
    let mut files = Vec::new();
    for relative in relative_roots {
        collect_rust_files(&root.join(relative), &mut files);
    }
    files.sort();
    files
}

fn collect_rust_files(directory: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
}

/// Reads a source file as the scanning rules should see it: without its
/// `#[cfg(test)]` module, and without comment lines.
///
/// Both exclusions matter. A rule about what the shipped code may contain
/// should not fire on test scaffolding, and it should not fire on a doc comment
/// that merely *names* the thing being banned - this file's own documentation
/// would otherwise be a violation of half the rules it describes.
pub fn source_without_tests(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let code = match text.find("#[cfg(test)]") {
        Some(index) => &text[..index],
        None => &text[..],
    };
    code.lines().filter(|line| !line.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n")
}
