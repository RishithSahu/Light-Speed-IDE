//! LightSpeed platform abstraction (specification section 6).
//!
//! Everything that differs between operating systems lives here: path
//! semantics, clipboard access, atomic file replacement, native file dialogs
//! and process resource sampling. The rest of LightSpeed is written against
//! these interfaces and contains no `if windows` branches.
//!
//! Stage 1 targets Windows 11 x64 (specification section 5.1). Non-Windows
//! builds compile against portable fallbacks so the editor core and its test
//! suite stay runnable everywhere; the fallbacks are honest about what they do
//! not implement rather than silently pretending to succeed.

pub mod clipboard;
pub mod depgraph_cache;
pub mod dialog;
pub mod fsops;
pub mod paths;
pub mod process;
pub mod recents;
pub mod settings_file;
pub mod terminal_log;

use ls_log::diag::{LsError, Recoverability};
use std::fmt;

pub use clipboard::{system_clipboard, Clipboard, MemoryClipboard};
pub use fsops::write_file_atomic;
pub use paths::CanonicalPath;
pub use process::{command, ProcessSampler, ProcessStats};

/// Failure of a platform service.
#[derive(Debug)]
pub struct PlatformError {
    code: &'static str,
    message: String,
    recoverability: Recoverability,
    source: Option<std::io::Error>,
}

impl PlatformError {
    pub fn new(
        code: &'static str,
        message: impl Into<String>,
        recoverability: Recoverability,
    ) -> Self {
        PlatformError { code, message: message.into(), recoverability, source: None }
    }

    pub fn io(code: &'static str, message: impl Into<String>, source: std::io::Error) -> Self {
        let recoverability = match source.kind() {
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound => {
                Recoverability::UserActionRequired
            }
            std::io::ErrorKind::Interrupted | std::io::ErrorKind::TimedOut => {
                Recoverability::Retryable
            }
            _ => Recoverability::Recoverable,
        };
        PlatformError { code, message: message.into(), recoverability, source: Some(source) }
    }

    /// The service is not implemented on this platform.
    pub fn unsupported(code: &'static str, message: impl Into<String>) -> Self {
        PlatformError {
            code,
            message: message.into(),
            recoverability: Recoverability::FatalToSubsystem,
            source: None,
        }
    }

    /// Stable machine-readable identifier.
    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn recoverability(&self) -> Recoverability {
        self.recoverability
    }

    pub fn io_kind(&self) -> Option<std::io::ErrorKind> {
        self.source.as_ref().map(|e| e.kind())
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Some(source) => write!(f, "{}: {source}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for PlatformError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|e| e as &(dyn std::error::Error + 'static))
    }
}

impl LsError for PlatformError {
    fn code(&self) -> &'static str {
        self.code
    }
    fn subsystem(&self) -> &'static str {
        "platform"
    }
    fn recoverability(&self) -> Recoverability {
        self.recoverability
    }
    fn cause(&self) -> Option<&dyn std::error::Error> {
        self.source.as_ref().map(|e| e as &dyn std::error::Error)
    }
}

/// Name of the platform this build targets, for logs and benchmark reports.
pub const fn platform_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}
