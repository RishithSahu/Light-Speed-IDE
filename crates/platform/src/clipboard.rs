//! Clipboard service (specification sections 9.4, 20, 62).
//!
//! The editor core talks to `read_text`/`write_text` and never to the OS. Text
//! is the only supported format in Stage 1; multi-format clipboard support is
//! explicitly out of scope.

use crate::PlatformError;
use ls_log::diag::Recoverability;
use std::sync::Mutex;

/// Text clipboard interface used by the editor core.
pub trait Clipboard: Send {
    fn read_text(&self) -> Result<String, PlatformError>;
    fn write_text(&self, text: &str) -> Result<(), PlatformError>;
}

/// In-process clipboard: the test double, and the fallback on platforms whose
/// native clipboard is not implemented yet.
#[derive(Default)]
pub struct MemoryClipboard {
    text: Mutex<String>,
}

impl MemoryClipboard {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Clipboard for MemoryClipboard {
    fn read_text(&self) -> Result<String, PlatformError> {
        Ok(self.text.lock().map(|t| t.clone()).unwrap_or_default())
    }

    fn write_text(&self, text: &str) -> Result<(), PlatformError> {
        match self.text.lock() {
            Ok(mut slot) => {
                slot.clear();
                slot.push_str(text);
                Ok(())
            }
            Err(_) => Err(PlatformError::new(
                "clipboard.poisoned",
                "in-process clipboard lock poisoned",
                Recoverability::FatalToSubsystem,
            )),
        }
    }
}

/// Returns the clipboard implementation for this platform.
pub fn system_clipboard() -> Box<dyn Clipboard> {
    #[cfg(windows)]
    {
        Box::new(windows_impl::SystemClipboard)
    }
    #[cfg(not(windows))]
    {
        Box::new(MemoryClipboard::new())
    }
}

#[cfg(windows)]
pub use windows_impl::SystemClipboard;

#[cfg(windows)]
mod windows_impl {
    use super::{Clipboard, PlatformError};
    use ls_log::diag::Recoverability;
    use std::ptr;
    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    /// The Win32 clipboard.
    pub struct SystemClipboard;

    /// The clipboard is a single global resource; another process can hold it
    /// briefly. Retry a few times before giving up, bounded so an interactive
    /// copy can never stall for long.
    const OPEN_ATTEMPTS: u32 = 6;
    const OPEN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(1);

    struct ClipboardSession;

    impl ClipboardSession {
        fn open() -> Result<Self, PlatformError> {
            for attempt in 0..OPEN_ATTEMPTS {
                // SAFETY: a null owner associates the clipboard with the current
                // task, which is what a single-window application wants.
                if unsafe { OpenClipboard(ptr::null_mut()) } != 0 {
                    return Ok(ClipboardSession);
                }
                if attempt + 1 < OPEN_ATTEMPTS {
                    std::thread::sleep(OPEN_RETRY_DELAY);
                }
            }
            Err(PlatformError::new(
                "clipboard.busy",
                "another application is holding the clipboard",
                Recoverability::Retryable,
            ))
        }
    }

    impl Drop for ClipboardSession {
        fn drop(&mut self) {
            // SAFETY: paired with the successful OpenClipboard above.
            unsafe { CloseClipboard() };
        }
    }

    impl Clipboard for SystemClipboard {
        fn read_text(&self) -> Result<String, PlatformError> {
            let _session = ClipboardSession::open()?;
            // SAFETY: the session guarantees the clipboard is open; the handle is
            // only dereferenced while locked, and unlocked on every path.
            unsafe {
                let handle: HANDLE = GetClipboardData(CF_UNICODETEXT as u32);
                if handle.is_null() {
                    // No text on the clipboard is not an error.
                    return Ok(String::new());
                }
                let global = handle as HGLOBAL;
                let data = GlobalLock(global) as *const u16;
                if data.is_null() {
                    return Err(PlatformError::new(
                        "clipboard.lock_failed",
                        "could not lock clipboard memory",
                        Recoverability::Retryable,
                    ));
                }
                let mut len = 0usize;
                while *data.add(len) != 0 {
                    len += 1;
                }
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(data, len));
                GlobalUnlock(global);
                Ok(text)
            }
        }

        fn write_text(&self, text: &str) -> Result<(), PlatformError> {
            let _session = ClipboardSession::open()?;
            let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = std::mem::size_of_val(utf16.as_slice());

            // SAFETY: the moveable block is filled while locked and either handed
            // to the clipboard (which takes ownership) or freed.
            unsafe {
                EmptyClipboard();
                let global = GlobalAlloc(GMEM_MOVEABLE, bytes);
                if global.is_null() {
                    return Err(PlatformError::new(
                        "clipboard.alloc_failed",
                        "could not allocate clipboard memory",
                        Recoverability::Recoverable,
                    ));
                }
                let destination = GlobalLock(global) as *mut u16;
                if destination.is_null() {
                    GlobalFree(global);
                    return Err(PlatformError::new(
                        "clipboard.lock_failed",
                        "could not lock clipboard memory",
                        Recoverability::Retryable,
                    ));
                }
                ptr::copy_nonoverlapping(utf16.as_ptr(), destination, utf16.len());
                GlobalUnlock(global);

                if SetClipboardData(CF_UNICODETEXT as u32, global as HANDLE).is_null() {
                    GlobalFree(global);
                    return Err(PlatformError::new(
                        "clipboard.set_failed",
                        "the system rejected the clipboard contents",
                        Recoverability::Recoverable,
                    ));
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_clipboard_round_trips() {
        let clipboard = MemoryClipboard::new();
        assert_eq!(clipboard.read_text().unwrap(), "");
        clipboard.write_text("hello \u{1F600} world").unwrap();
        assert_eq!(clipboard.read_text().unwrap(), "hello \u{1F600} world");
        clipboard.write_text("").unwrap();
        assert_eq!(clipboard.read_text().unwrap(), "");
    }

    #[test]
    fn system_clipboard_round_trips() {
        // The OS clipboard is shared machine state: tolerate a busy clipboard
        // rather than failing the suite for something outside our control.
        let clipboard = system_clipboard();
        let marker = format!("lightspeed-test-{}", std::process::id());
        match clipboard.write_text(&marker) {
            Ok(()) => assert_eq!(clipboard.read_text().unwrap(), marker),
            Err(err) => assert_eq!(err.code(), "clipboard.busy"),
        }
    }
}
