//! Native filesystem change notification (ADR-0017).
//!
//! The scheduler's task model is one-shot: a task body runs to completion on a
//! worker thread and nothing about it stands waiting forever (amendment
//! section 3.5). A filesystem watcher wants exactly the opposite shape --
//! "block until the kernel says something changed" -- so this module never
//! blocks indefinitely. [`wait_for_change`] issues one `ReadDirectoryChangesW`
//! and waits for it in bounded slices (`poll_interval`), checking the
//! cancellation callback between slices. That is what lets a caller run this
//! inside an ordinary scheduler task: cancelling the task is noticed within one
//! slice instead of never, and the task still completes and gets re-armed by
//! its caller rather than needing a dedicated thread (`crates/core`'s
//! `EditorCore` owns the re-arming loop; this module owns exactly one
//! directory handle and one pending read).
//!
//! Non-Windows builds return [`PlatformError::unsupported`] rather than
//! silently pretending to watch anything.

use crate::PlatformError;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What one bounded wait produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchOutcome {
    /// The directory changed. An empty list means the OS could not report
    /// which entries (its notification buffer overflowed): the caller should
    /// treat everything under the watched directory as possibly changed
    /// rather than trust an empty list to mean nothing happened.
    Changed(Vec<PathBuf>),
    /// `cancelled` returned `true` before anything changed.
    Cancelled,
}

/// Waits for the next change under `directory`.
///
/// Blocks the calling thread until a change is reported or `cancelled`
/// returns `true`, checking `cancelled` roughly every `poll_interval`. Returns
/// paths relative to `directory` (not full paths) on the Windows
/// implementation, since that is what `ReadDirectoryChangesW` reports.
pub fn wait_for_change(
    directory: &Path,
    watch_subtree: bool,
    poll_interval: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<WatchOutcome, PlatformError> {
    #[cfg(windows)]
    {
        windows_impl::wait_for_change(directory, watch_subtree, poll_interval, cancelled)
    }
    #[cfg(not(windows))]
    {
        let _ = (directory, watch_subtree, poll_interval, cancelled);
        Err(PlatformError::unsupported(
            "watch.unsupported",
            "filesystem change notification is not implemented on this platform",
        ))
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_IO_PENDING, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
        FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
        FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResultEx, OVERLAPPED};
    use windows_sys::Win32::System::Threading::CreateEventW;

    /// Comfortably holds a burst of edits (a formatter rewriting several
    /// files, a `git checkout`) without overflowing into the "unknown scope"
    /// case; large enough to be worth keeping off the stack.
    const BUFFER_LEN: usize = 16 * 1024;

    const NOTIFY_FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_CREATION;

    /// Closes a handle on drop so an early return (an error path, a
    /// cancellation) never leaks the OS object.
    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if self.0 != std::ptr::null_mut() {
                // SAFETY: `self.0` is a handle this module opened and does not
                // share with anyone else, so nothing else can be using it.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

    fn io_error(code: &'static str, message: String) -> PlatformError {
        PlatformError::io(code, message, std::io::Error::last_os_error())
    }

    pub fn wait_for_change(
        directory: &Path,
        watch_subtree: bool,
        poll_interval: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<WatchOutcome, PlatformError> {
        let path = wide(directory);
        // SAFETY: `path` is null-terminated and outlives the call.
        let raw = unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io_error(
                "watch.open_failed",
                format!("could not open {} for change notification", directory.display()),
            ));
        }
        let handle = OwnedHandle(raw);

        // SAFETY: no attributes, manual-reset, initially unsignaled, unnamed --
        // all valid null/zero arguments per `CreateEventW`'s contract.
        let raw_event =
            unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if raw_event.is_null() {
            return Err(io_error(
                "watch.event_failed",
                format!("could not create a wait event for {}", directory.display()),
            ));
        }
        let event = OwnedHandle(raw_event);

        let mut overlapped = OVERLAPPED::default();
        overlapped.hEvent = event.0;
        let mut buffer = vec![0u8; BUFFER_LEN];
        let mut queued_bytes: u32 = 0;

        // SAFETY: `buffer` outlives the call and is sized as declared;
        // `overlapped` outlives every use of it below, including the
        // cancellation path, which waits for the kernel to finish with it
        // before this function returns.
        let ok = unsafe {
            ReadDirectoryChangesW(
                handle.0,
                buffer.as_mut_ptr() as *mut core::ffi::c_void,
                buffer.len() as u32,
                if watch_subtree { 1 } else { 0 },
                NOTIFY_FILTER,
                &mut queued_bytes,
                &mut overlapped,
                None,
            )
        };
        if ok == 0 {
            let pending = std::io::Error::last_os_error().raw_os_error()
                == Some(ERROR_IO_PENDING as i32);
            if !pending {
                return Err(io_error(
                    "watch.read_failed",
                    format!("could not watch {}", directory.display()),
                ));
            }
        }

        loop {
            if cancelled() {
                // SAFETY: `overlapped` is still valid; cancelling and then
                // reaping the result (below) is what makes it safe to drop.
                unsafe {
                    CancelIoEx(handle.0, &overlapped);
                }
                let mut discarded: u32 = 0;
                // SAFETY: waits for the cancelled I/O to actually finish, so
                // the kernel is done writing into `buffer`/`overlapped`
                // before they are dropped.
                unsafe {
                    GetOverlappedResultEx(handle.0, &overlapped, &mut discarded, u32::MAX, 0);
                }
                return Ok(WatchOutcome::Cancelled);
            }

            let mut transferred: u32 = 0;
            let wait_ms = poll_interval.as_millis().min(u128::from(u32::MAX)) as u32;
            // SAFETY: `overlapped` and `buffer` are still alive and were the
            // exact ones passed to `ReadDirectoryChangesW` above.
            let ready = unsafe {
                GetOverlappedResultEx(handle.0, &overlapped, &mut transferred, wait_ms, 0)
            };
            if ready != 0 {
                // SAFETY: the kernel wrote exactly `transferred` bytes of
                // `FILE_NOTIFY_INFORMATION` records into `buffer`.
                let paths = unsafe { parse_notifications(&buffer[..transferred as usize]) };
                return Ok(WatchOutcome::Changed(paths));
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(WAIT_TIMEOUT as i32) {
                continue;
            }
            return Err(PlatformError::io(
                "watch.wait_failed",
                format!("waiting for changes under {} failed", directory.display()),
                error,
            ));
        }
    }

    /// Walks the `FILE_NOTIFY_INFORMATION` linked list the kernel wrote into
    /// `buffer`, returning each entry's file name.
    ///
    /// # Safety
    /// `buffer` must be exactly the bytes a completed `ReadDirectoryChangesW`
    /// wrote (or a prefix bounded by its returned byte count), so every
    /// `NextEntryOffset`/`FileNameLength` this function trusts for pointer
    /// arithmetic is one the kernel itself produced.
    unsafe fn parse_notifications(buffer: &[u8]) -> Vec<PathBuf> {
        const HEADER_LEN: usize = std::mem::size_of::<u32>() * 3;
        let mut paths = Vec::new();
        let mut offset = 0usize;
        loop {
            if offset + HEADER_LEN > buffer.len() {
                break;
            }
            let record = buffer.as_ptr().add(offset) as *const FILE_NOTIFY_INFORMATION;
            let next = (*record).NextEntryOffset as usize;
            let name_len = (*record).FileNameLength as usize;
            let name_offset = offset + HEADER_LEN;
            if name_offset + name_len > buffer.len() {
                break;
            }
            let name_ptr = buffer.as_ptr().add(name_offset) as *const u16;
            let name_units = name_len / 2;
            let name_slice = std::slice::from_raw_parts(name_ptr, name_units);
            paths.push(PathBuf::from(String::from_utf16_lossy(name_slice)));
            if next == 0 {
                break;
            }
            offset += next;
        }
        paths
    }
}

#[cfg(test)]
#[cfg(windows)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lightspeed-watch-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reports_a_new_file() {
        let dir = scratch("new-file");
        let never = || false;

        let handle = std::thread::spawn({
            let dir = dir.clone();
            move || wait_for_change(&dir, false, Duration::from_millis(200), &never)
        });
        // Give the watch time to be established before the write happens.
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();

        let outcome = handle.join().unwrap().unwrap();
        match outcome {
            WatchOutcome::Changed(paths) => {
                assert!(
                    paths.iter().any(|p| p == Path::new("a.txt")) || paths.is_empty(),
                    "expected a.txt among {paths:?}"
                );
            }
            WatchOutcome::Cancelled => panic!("expected a change, not a cancellation"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stops_promptly_once_cancelled() {
        let dir = scratch("cancel");
        let flag = Arc::new(AtomicBool::new(false));
        let checker = {
            let flag = flag.clone();
            move || flag.load(Ordering::Relaxed)
        };

        let handle = std::thread::spawn({
            let dir = dir.clone();
            move || wait_for_change(&dir, false, Duration::from_millis(50), &checker)
        });
        std::thread::sleep(Duration::from_millis(120));
        let started = std::time::Instant::now();
        flag.store(true, Ordering::Relaxed);

        let outcome = handle.join().unwrap().unwrap();
        assert_eq!(outcome, WatchOutcome::Cancelled);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancellation should be noticed within a couple of poll intervals"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
