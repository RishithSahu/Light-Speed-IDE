//! Durable file replacement (specification section 29).
//!
//! ```text
//! encode -> temporary file -> write -> flush -> fsync -> atomic replace
//! ```
//!
//! The original file stays intact until the replacement succeeds, so an
//! interrupted save can lose the new contents but never the old ones.

use crate::PlatformError;
use ls_log::diag::Recoverability;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `contents` to `path` durably, replacing any existing file.
pub fn write_file_atomic(path: &Path, contents: &[u8]) -> Result<(), PlatformError> {
    write_file_atomic_with(path, |writer| writer.write_all(contents))
}

/// Durable replacement for content that should not be materialized as one
/// buffer first - a large document is streamed chunk by chunk into the
/// temporary file.
pub fn write_file_atomic_with<F>(path: &Path, write: F) -> Result<(), PlatformError>
where
    F: FnOnce(&mut BufWriter<File>) -> std::io::Result<()>,
{
    let directory = path.parent().ok_or_else(|| {
        PlatformError::new(
            "persistence.no_parent_directory",
            format!("{} has no parent directory", path.display()),
            Recoverability::UserActionRequired,
        )
    })?;
    let temp_path = temp_path_for(path);

    let result = (|| -> std::io::Result<()> {
        let file = File::create(&temp_path)?;
        let mut writer = BufWriter::new(file);
        write(&mut writer)?;
        writer.flush()?;
        let file = writer.into_inner().map_err(|e| e.into_error())?;
        // Durability point: the bytes are on the device before the rename that
        // makes them visible under the real name.
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(PlatformError::io(
            "persistence.temp_write_failed",
            format!("could not write temporary file for {}", path.display()),
            err,
        ));
    }

    if let Err(err) = atomic_replace(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }

    // Best effort: make the directory entry itself durable. Windows has no
    // portable equivalent and MOVEFILE_WRITE_THROUGH already covers it.
    #[cfg(unix)]
    if let Ok(dir) = File::open(directory) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = directory;

    Ok(())
}

/// Replaces `destination` with `source` atomically.
pub fn atomic_replace(source: &Path, destination: &Path) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };

        let wide = |path: &Path| -> Vec<u16> {
            path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
        };
        let source_w = wide(source);
        let destination_w = wide(destination);
        // SAFETY: both buffers are null-terminated and outlive the call.
        let ok = unsafe {
            MoveFileExW(
                source_w.as_ptr(),
                destination_w.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            return Err(PlatformError::io(
                "persistence.replace_failed",
                format!("could not replace {}", destination.display()),
                std::io::Error::last_os_error(),
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(source, destination).map_err(|err| {
            PlatformError::io(
                "persistence.replace_failed",
                format!("could not replace {}", destination.display()),
                err,
            )
        })
    }
}

/// Temporary sibling of the target: same directory, so the replacement stays on
/// one volume and therefore stays atomic.
fn temp_path_for(path: &Path) -> PathBuf {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    directory.join(format!(".{stem}.lightspeed-{}-{unique}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lightspeed-fsops-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_new_file() {
        let dir = temp_dir("new");
        let path = dir.join("a.txt");
        std::fs::remove_file(&path).ok();
        write_file_atomic(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replaces_existing_file() {
        let dir = temp_dir("replace");
        let path = dir.join("b.txt");
        std::fs::write(&path, b"old contents").unwrap();
        write_file_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn leaves_no_temporary_files_behind() {
        let dir = temp_dir("clean");
        let path = dir.join("c.txt");
        write_file_atomic(&path, b"x").unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("lightspeed-"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn original_survives_a_failed_write() {
        let dir = temp_dir("failure");
        let path = dir.join("d.txt");
        std::fs::write(&path, b"original").unwrap();

        let error =
            write_file_atomic_with(&path, |_| Err(std::io::Error::other("encoder blew up")))
                .unwrap_err();
        assert_eq!(error.code, "persistence.temp_write_failed");
        // The old contents are still there, and no debris was left.
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("lightspeed-"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn streams_large_content_without_one_buffer() {
        let dir = temp_dir("stream");
        let path = dir.join("e.bin");
        let chunk = vec![b'z'; 64 * 1024];
        write_file_atomic_with(&path, |writer| {
            for _ in 0..16 {
                writer.write_all(&chunk)?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 16 * 64 * 1024);
        std::fs::remove_file(&path).ok();
    }
}
