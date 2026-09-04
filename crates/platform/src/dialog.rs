//! Native file dialogs.
//!
//! Stage 1 has no file tree (that belongs to the Foundation Stage), so opening
//! and saving files goes through the platform's own dialogs. They are modal and
//! run on the interactive thread: the user is waiting on them by definition, and
//! no editor work is pending while one is open.

use crate::PlatformError;
use std::path::{Path, PathBuf};

/// A named filter, e.g. `("Source files", "*.rs;*.py;*.c")`.
pub type Filter<'a> = (&'a str, &'a str);

/// Filters offered by the LightSpeed open/save dialogs.
pub const DEFAULT_FILTERS: &[Filter<'static>] = &[
    ("All files", "*.*"),
    (
        "Source files",
        "*.rs;*.py;*.c;*.h;*.cpp;*.hpp;*.cs;*.js;*.ts;*.tsx;*.json;*.toml;*.yaml;*.yml;*.md;*.sh;*.txt",
    ),
];

/// Shows a native "open file" dialog. `Ok(None)` means the user cancelled.
pub fn open_file(
    owner: Option<isize>,
    title: &str,
    initial_dir: Option<&Path>,
) -> Result<Option<PathBuf>, PlatformError> {
    platform::show(platform::Mode::Open, owner, title, initial_dir, None)
}

/// Shows a native "save file as" dialog. `Ok(None)` means the user cancelled.
pub fn save_file(
    owner: Option<isize>,
    title: &str,
    initial_dir: Option<&Path>,
    suggested_name: Option<&str>,
) -> Result<Option<PathBuf>, PlatformError> {
    platform::show(platform::Mode::Save, owner, title, initial_dir, suggested_name)
}

/// Shows a native "select folder" dialog. `Ok(None)` means the user
/// cancelled. Unlike [`open_file`], this never needs an already-open document
/// to know where to start or what to open -- the picker is the source of the
/// folder, not the active tab.
pub fn open_folder(
    owner: Option<isize>,
    title: &str,
    initial_dir: Option<&Path>,
) -> Result<Option<PathBuf>, PlatformError> {
    platform::show_folder(owner, title, initial_dir)
}

#[cfg(windows)]
mod platform {
    use super::{PlatformError, DEFAULT_FILTERS};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::UI::Controls::Dialogs::{
        CommDlgExtendedError, GetOpenFileNameW, GetSaveFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST,
        OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };

    pub enum Mode {
        Open,
        Save,
    }

    /// Win32 caps the classic dialog's path buffer; 32 KiB of UTF-16 is the
    /// documented maximum for a single selection.
    const PATH_BUFFER: usize = 32 * 1024;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Win32 filters are `label\0pattern\0...\0\0`.
    fn filter_string() -> Vec<u16> {
        let mut buffer = Vec::new();
        for (label, pattern) in DEFAULT_FILTERS {
            buffer.extend(label.encode_utf16());
            buffer.push(0);
            buffer.extend(pattern.encode_utf16());
            buffer.push(0);
        }
        buffer.push(0);
        buffer
    }

    pub fn show(
        mode: Mode,
        owner: Option<isize>,
        title: &str,
        initial_dir: Option<&Path>,
        suggested_name: Option<&str>,
    ) -> Result<Option<PathBuf>, PlatformError> {
        let mut file_buffer: Vec<u16> = vec![0; PATH_BUFFER];
        if let Some(name) = suggested_name {
            for (slot, unit) in file_buffer.iter_mut().zip(name.encode_utf16()) {
                *slot = unit;
            }
        }
        let title_w = wide(title);
        let filter_w = filter_string();
        let initial_dir_w: Option<Vec<u16>> = initial_dir
            .map(|dir| dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect());

        let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
        ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = owner.unwrap_or(0) as *mut std::ffi::c_void;
        ofn.lpstrFilter = filter_w.as_ptr();
        ofn.nFilterIndex = 1;
        ofn.lpstrFile = file_buffer.as_mut_ptr();
        ofn.nMaxFile = PATH_BUFFER as u32;
        ofn.lpstrTitle = title_w.as_ptr();
        ofn.lpstrInitialDir =
            initial_dir_w.as_ref().map(|d| d.as_ptr()).unwrap_or(std::ptr::null());
        ofn.Flags = OFN_EXPLORER
            | OFN_NOCHANGEDIR
            | match mode {
                Mode::Open => OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST,
                Mode::Save => OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST,
            };

        // SAFETY: every pointer in `ofn` refers to a buffer that outlives the
        // call, and `lpstrFile` is writable with `nMaxFile` units of space.
        let chosen = unsafe {
            match mode {
                Mode::Open => GetOpenFileNameW(&mut ofn),
                Mode::Save => GetSaveFileNameW(&mut ofn),
            }
        };

        if chosen == 0 {
            // Zero means cancelled *or* failed; the extended error separates them.
            // SAFETY: no preconditions.
            let error = unsafe { CommDlgExtendedError() };
            return if error == 0 {
                Ok(None)
            } else {
                Err(PlatformError::new(
                    "dialog.failed",
                    format!("the file dialog failed (CommDlgExtendedError {error:#x})"),
                    ls_log::diag::Recoverability::Recoverable,
                ))
            };
        }

        let length = file_buffer.iter().position(|&c| c == 0).unwrap_or(0);
        if length == 0 {
            return Ok(None);
        }
        Ok(Some(PathBuf::from(String::from_utf16_lossy(&file_buffer[..length]))))
    }

    /// A `CoInitializeEx` call, undone on drop for every success code
    /// (`S_OK` *and* `S_FALSE` -- the documented contract is one
    /// `CoUninitialize` per successful `CoInitializeEx`, including a
    /// redundant call on an already-initialized thread) and left alone on
    /// failure, so this never tears down whatever initialized COM first --
    /// winit's own OLE setup for drag-and-drop, on this thread.
    struct ComGuard(bool);

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: only called when this guard's own CoInitializeEx
                // succeeded, which is the documented precondition.
                unsafe { windows::Win32::System::Com::CoUninitialize() };
            }
        }
    }

    fn init_com() -> ComGuard {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        // SAFETY: no preconditions.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        ComGuard(hr.is_ok())
    }

    /// The modern, Explorer-style folder picker. `SHBrowseForFolderW` (the
    /// classic tree-view dialog) is not used here: `windows-sys` does not
    /// expose `IFileOpenDialog` at all (it only generates the flat C ABI,
    /// and this interface's vtable is absent from its Shell bindings), which
    /// is what `IFileOpenDialog` + `FOS_PICKFOLDERS` needs -- the same
    /// dialog `File > Open...` already uses, just configured to pick a
    /// folder instead of a file, matching Explorer everywhere else in the
    /// shell.
    pub fn show_folder(
        owner: Option<isize>,
        title: &str,
        initial_dir: Option<&Path>,
    ) -> Result<Option<PathBuf>, PlatformError> {
        use windows::core::HSTRING;
        use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
        use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
        use windows::Win32::UI::Shell::{
            FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName,
            FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
        };

        let _com = init_com();

        // SAFETY: `FileOpenDialog`'s CLSID and `IFileOpenDialog`'s IID are
        // the standard shell ones; `CoCreateInstance` reports failure
        // through its `Result` rather than an invalid object.
        let dialog: IFileOpenDialog =
            unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| dialog_error("could not create the folder picker", &error))?;

        // SAFETY: every `IFileOpenDialog` call below follows the documented
        // COM calling convention (a live `&self` on the interface just
        // created), and every string handed in (`HSTRING`) owns its buffer
        // for the call's duration.
        unsafe {
            let options = dialog.GetOptions().map_err(|error| {
                dialog_error("could not read the folder picker's options", &error)
            })?;
            dialog
                .SetOptions(options | FOS_PICKFOLDERS)
                .map_err(|error| dialog_error("could not configure the folder picker", &error))?;
            let _ = dialog.SetTitle(&HSTRING::from(title));

            if let Some(dir) = initial_dir {
                // A folder that no longer exists (renamed, deleted, on an
                // unplugged drive) just means the dialog opens at its own
                // default location instead -- not a reason to fail the
                // whole picker.
                if let Ok(item) =
                    SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(dir), None)
                {
                    let _ = dialog.SetFolder(&item);
                }
            }

            let hwnd = owner
                .filter(|&handle| handle != 0)
                .map(|handle| HWND(handle as *mut std::ffi::c_void));
            match dialog.Show(hwnd) {
                Ok(()) => {}
                Err(error)
                    if error.code() == windows::core::HRESULT::from_win32(ERROR_CANCELLED.0) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(dialog_error("the folder picker failed", &error)),
            }

            let result: IShellItem = dialog.GetResult().map_err(|error| {
                dialog_error("the folder picker closed without a result", &error)
            })?;
            let display_name = result
                .GetDisplayName(SIGDN_FILESYSPATH)
                .map_err(|error| dialog_error("could not read the chosen folder's path", &error))?;
            let path = display_name.to_string();
            windows::Win32::System::Com::CoTaskMemFree(Some(display_name.as_ptr() as *const _));

            let path = path.map_err(|_| {
                PlatformError::new(
                    "dialog.failed",
                    "the chosen folder's path was not valid UTF-16",
                    ls_log::diag::Recoverability::Recoverable,
                )
            })?;
            Ok(Some(PathBuf::from(path)))
        }
    }

    fn dialog_error(context: &str, error: &windows::core::Error) -> PlatformError {
        PlatformError::new(
            "dialog.failed",
            format!("{context}: {error}"),
            ls_log::diag::Recoverability::Recoverable,
        )
    }
}

#[cfg(not(windows))]
mod platform {
    use super::PlatformError;
    use std::path::{Path, PathBuf};

    pub enum Mode {
        Open,
        Save,
    }

    pub fn show(
        _mode: Mode,
        _owner: Option<isize>,
        _title: &str,
        _initial_dir: Option<&Path>,
        _suggested_name: Option<&str>,
    ) -> Result<Option<PathBuf>, PlatformError> {
        Err(PlatformError::unsupported(
            "dialog.unsupported",
            "native file dialogs are implemented for Windows only in Stage 1",
        ))
    }

    pub fn show_folder(
        _owner: Option<isize>,
        _title: &str,
        _initial_dir: Option<&Path>,
    ) -> Result<Option<PathBuf>, PlatformError> {
        Err(PlatformError::unsupported(
            "dialog.unsupported",
            "native folder dialogs are implemented for Windows only in Stage 1",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_filters_are_well_formed() {
        assert!(!DEFAULT_FILTERS.is_empty());
        for (label, pattern) in DEFAULT_FILTERS {
            assert!(!label.is_empty());
            assert!(pattern.starts_with('*'));
            assert!(!label.contains('\0') && !pattern.contains('\0'));
        }
    }
}
