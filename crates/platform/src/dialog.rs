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
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
    };
    use windows_sys::Win32::UI::Controls::Dialogs::{
        CommDlgExtendedError, GetOpenFileNameW, GetSaveFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST,
        OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    use windows_sys::Win32::UI::Shell::{
        SHBrowseForFolderW, SHGetPathFromIDListW, BFFM_INITIALIZED, BFFM_SETSELECTIONW,
        BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;

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

    /// A `CoInitializeEx` call, undone on drop only if this call is the one
    /// that actually put the thread into an apartment. A second call on an
    /// already-initialized thread returns `S_FALSE`, and pairing that with
    /// `CoUninitialize` would tear down whatever initialized COM first --
    /// winit's own OLE setup for drag-and-drop, on this thread.
    struct ComGuard(bool);

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: only called when this guard's own CoInitializeEx
                // returned S_OK, so this thread's apartment is ours to leave.
                unsafe { CoUninitialize() };
            }
        }
    }

    fn init_com() -> ComGuard {
        const S_OK: i32 = 0;
        // SAFETY: no preconditions; a non-null-reserved argument is invalid,
        // and `null()` is what the API documents for it.
        let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        ComGuard(hr == S_OK)
    }

    pub fn show_folder(
        owner: Option<isize>,
        title: &str,
        initial_dir: Option<&Path>,
    ) -> Result<Option<PathBuf>, PlatformError> {
        let _com = init_com();

        let title_w = wide(title);
        let initial_dir_w: Option<Vec<u16>> = initial_dir
            .map(|dir| dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect());

        let mut display_name = [0u16; 260];
        let mut bi: BROWSEINFOW = unsafe { std::mem::zeroed() };
        bi.hwndOwner = owner.unwrap_or(0) as *mut std::ffi::c_void;
        bi.pszDisplayName = display_name.as_mut_ptr();
        bi.lpszTitle = title_w.as_ptr();
        bi.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
        if let Some(dir_w) = &initial_dir_w {
            bi.lpfn = Some(set_initial_selection);
            bi.lParam = dir_w.as_ptr() as isize;
        }

        // SAFETY: every pointer in `bi` refers to a buffer that outlives the
        // call (`title_w` and `initial_dir_w` live until the end of this
        // function, after `SHBrowseForFolderW` has returned).
        let pidl = unsafe { SHBrowseForFolderW(&bi) };
        if pidl.is_null() {
            return Ok(None); // cancelled
        }

        let mut path_buffer: Vec<u16> = vec![0; PATH_BUFFER];
        // SAFETY: `pidl` was just returned by `SHBrowseForFolderW` and
        // `path_buffer` has room for the longest path the legacy shell API
        // can produce into it.
        let ok = unsafe { SHGetPathFromIDListW(pidl, path_buffer.as_mut_ptr()) };
        // SAFETY: `pidl` was allocated by the shell with `CoTaskMemAlloc`,
        // per the documented contract of `SHBrowseForFolderW`.
        unsafe { CoTaskMemFree(pidl as *const std::ffi::c_void) };

        if ok == 0 {
            return Ok(None);
        }
        let length = path_buffer.iter().position(|&c| c == 0).unwrap_or(0);
        if length == 0 {
            return Ok(None);
        }
        Ok(Some(PathBuf::from(String::from_utf16_lossy(&path_buffer[..length]))))
    }

    /// Seeds the folder tree's initial selection once the dialog signals it
    /// is ready (`BFFM_INITIALIZED`), from the wide string stashed in
    /// `BROWSEINFOW::lParam` and handed back here as `lpdata`.
    unsafe extern "system" fn set_initial_selection(
        hwnd: HWND,
        msg: u32,
        _lparam: LPARAM,
        lpdata: LPARAM,
    ) -> i32 {
        if msg == BFFM_INITIALIZED {
            // SAFETY: `hwnd` is the live dialog handle the shell just passed
            // us; `lpdata` is the null-terminated wide string we stashed in
            // `BROWSEINFOW::lParam`, still alive for the dialog's lifetime.
            unsafe { SendMessageW(hwnd, BFFM_SETSELECTIONW, 1, lpdata) };
        }
        0
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
