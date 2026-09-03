//! Typed errors (specification section 39).
//!
//! Every error names a stable code, the subsystem that produced it and what the
//! caller can do about it. Cancellation is not represented here: cancellation is
//! normal control flow, not a failure.

use crate::document::DocumentId;
use ls_log::diag::{LsError, Recoverability};
use ls_platform::PlatformError;
use std::fmt;
use std::path::PathBuf;

/// Failure of an editing operation.
#[derive(Debug)]
pub enum EditorError {
    /// The document id does not refer to an open document.
    UnknownDocument(DocumentId),
    /// A position or range fell outside the document.
    InvalidPosition { position: usize, length: usize },
    /// Inverted or otherwise impossible range.
    InvalidRange { start: usize, end: usize },
    /// The clipboard could not be read or written.
    Clipboard(PlatformError),
    /// The command id is not in the registry.
    UnknownCommand(String),
    /// The command exists but is not applicable right now.
    CommandNotEnabled(&'static str),
    /// Closing would discard unsaved edits; the caller must save or say so
    /// explicitly (see `EditorCore::close_document_discarding_changes`).
    UnsavedChanges(DocumentId),
}

impl fmt::Display for EditorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditorError::UnknownDocument(id) => write!(f, "no open document with id {id}"),
            EditorError::InvalidPosition { position, length } => {
                write!(f, "position {position} is outside a document of {length} characters")
            }
            EditorError::InvalidRange { start, end } => {
                write!(f, "invalid range {start}..{end}")
            }
            EditorError::Clipboard(err) => write!(f, "clipboard unavailable: {err}"),
            EditorError::UnknownCommand(id) => write!(f, "unknown command {id}"),
            EditorError::CommandNotEnabled(id) => write!(f, "command {id} is not available"),
            EditorError::UnsavedChanges(_) => f.write_str("the document has unsaved changes"),
        }
    }
}

impl std::error::Error for EditorError {}

impl LsError for EditorError {
    fn code(&self) -> &'static str {
        match self {
            EditorError::UnknownDocument(_) => "editor.unknown_document",
            EditorError::InvalidPosition { .. } => "editor.invalid_position",
            EditorError::InvalidRange { .. } => "editor.invalid_range",
            EditorError::Clipboard(_) => "editor.clipboard",
            EditorError::UnknownCommand(_) => "editor.unknown_command",
            EditorError::CommandNotEnabled(_) => "editor.command_not_enabled",
            EditorError::UnsavedChanges(_) => "editor.unsaved_changes",
        }
    }

    fn subsystem(&self) -> &'static str {
        "editor"
    }

    fn recoverability(&self) -> Recoverability {
        match self {
            EditorError::Clipboard(_) => Recoverability::Retryable,
            EditorError::UnsavedChanges(_) => Recoverability::UserActionRequired,
            _ => Recoverability::Recoverable,
        }
    }
}

/// Failure of `open_document()` (specification section 24).
#[derive(Debug)]
pub enum OpenDocumentError {
    NotFound(PathBuf),
    PermissionDenied(PathBuf),
    IsDirectory(PathBuf),
    /// The file is not editable text (specification section 19).
    Binary {
        path: PathBuf,
        reason: BinaryReason,
    },
    Encoding {
        path: PathBuf,
        source: EncodingError,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The scheduler refused the load: its queue is full (amendment section
    /// 3.5.1). The request never became a task, and nothing was dropped
    /// silently - the caller is being told to try again.
    Rejected {
        path: PathBuf,
        reason: String,
    },
}

impl From<WorkspaceError> for OpenDocumentError {
    fn from(error: WorkspaceError) -> Self {
        match error {
            WorkspaceError::NotFound(path) => OpenDocumentError::NotFound(path),
            WorkspaceError::NotADirectory(path) => OpenDocumentError::IsDirectory(path),
            WorkspaceError::Io { path, source } => match source.kind() {
                std::io::ErrorKind::PermissionDenied => OpenDocumentError::PermissionDenied(path),
                _ => OpenDocumentError::Io { path, source },
            },
        }
    }
}

/// Why a file was classified as binary.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BinaryReason {
    /// A NUL byte appeared inside the inspected prefix.
    NulByte { offset: usize },
    /// The bytes are not valid text in any supported encoding.
    UndecodableBytes,
}

impl fmt::Display for BinaryReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BinaryReason::NulByte { offset } => write!(f, "NUL byte at offset {offset}"),
            BinaryReason::UndecodableBytes => f.write_str("bytes are not decodable text"),
        }
    }
}

impl fmt::Display for OpenDocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenDocumentError::NotFound(path) => write!(f, "{} does not exist", path.display()),
            OpenDocumentError::PermissionDenied(path) => {
                write!(f, "no permission to read {}", path.display())
            }
            OpenDocumentError::IsDirectory(path) => {
                write!(f, "{} is a directory", path.display())
            }
            OpenDocumentError::Binary { path, reason } => {
                write!(f, "{} is a binary file ({reason})", path.display())
            }
            OpenDocumentError::Encoding { path, source } => {
                write!(f, "cannot decode {}: {source}", path.display())
            }
            OpenDocumentError::Io { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            OpenDocumentError::Rejected { path, reason } => {
                write!(f, "cannot open {} right now: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for OpenDocumentError {}

impl LsError for OpenDocumentError {
    fn code(&self) -> &'static str {
        match self {
            OpenDocumentError::NotFound(_) => "document.not_found",
            OpenDocumentError::PermissionDenied(_) => "document.permission_denied",
            OpenDocumentError::IsDirectory(_) => "document.is_directory",
            OpenDocumentError::Binary { .. } => "document.binary_rejected",
            OpenDocumentError::Encoding { .. } => "document.encoding",
            OpenDocumentError::Io { .. } => "document.io",
            OpenDocumentError::Rejected { .. } => "document.open_rejected",
        }
    }

    fn subsystem(&self) -> &'static str {
        "document"
    }

    fn recoverability(&self) -> Recoverability {
        match self {
            // Both of these get better on their own: the disk settles, or the
            // queue drains.
            OpenDocumentError::Io { .. } | OpenDocumentError::Rejected { .. } => {
                Recoverability::Retryable
            }
            _ => Recoverability::UserActionRequired,
        }
    }
}

/// Failure of a save (specification sections 25, 29).
#[derive(Debug)]
pub enum PersistenceError {
    /// The document has never been saved and no path was supplied.
    NoPath,
    /// The platform's durable-write path failed.
    Platform(PlatformError),
    /// The document could not be encoded in its declared encoding.
    Encoding(EncodingError),
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PersistenceError::NoPath => f.write_str("the document has no file to save to"),
            PersistenceError::Platform(err) => write!(f, "{err}"),
            PersistenceError::Encoding(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for PersistenceError {}

impl LsError for PersistenceError {
    fn code(&self) -> &'static str {
        match self {
            PersistenceError::NoPath => "persistence.no_path",
            PersistenceError::Platform(err) => err.code(),
            PersistenceError::Encoding(_) => "persistence.encoding",
        }
    }

    fn subsystem(&self) -> &'static str {
        "persistence"
    }

    fn recoverability(&self) -> Recoverability {
        match self {
            PersistenceError::NoPath => Recoverability::UserActionRequired,
            PersistenceError::Platform(err) => err.recoverability(),
            PersistenceError::Encoding(_) => Recoverability::UserActionRequired,
        }
    }
}

/// Failure of a workspace filesystem operation (specification section 31).
#[derive(Debug)]
pub enum WorkspaceError {
    NotFound(PathBuf),
    NotADirectory(PathBuf),
    Io { path: PathBuf, source: std::io::Error },
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspaceError::NotFound(path) => write!(f, "{} does not exist", path.display()),
            WorkspaceError::NotADirectory(path) => {
                write!(f, "{} is not a directory", path.display())
            }
            WorkspaceError::Io { path, source } => {
                write!(f, "filesystem error on {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl LsError for WorkspaceError {
    fn code(&self) -> &'static str {
        match self {
            WorkspaceError::NotFound(_) => "workspace.not_found",
            WorkspaceError::NotADirectory(_) => "workspace.not_a_directory",
            WorkspaceError::Io { .. } => "workspace.io",
        }
    }

    fn subsystem(&self) -> &'static str {
        "workspace"
    }

    fn recoverability(&self) -> Recoverability {
        match self {
            WorkspaceError::Io { .. } => Recoverability::Retryable,
            _ => Recoverability::UserActionRequired,
        }
    }
}

/// Failure to decode or encode text (specification section 19).
#[derive(Debug)]
pub enum EncodingError {
    /// The byte order mark names an encoding LightSpeed does not support.
    Unsupported(&'static str),
    /// The bytes are not valid in the detected encoding.
    Invalid { encoding: &'static str, offset: usize },
    /// UTF-16 input with an odd number of bytes.
    TruncatedUtf16,
}

impl fmt::Display for EncodingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodingError::Unsupported(name) => write!(f, "unsupported encoding {name}"),
            EncodingError::Invalid { encoding, offset } => {
                write!(f, "invalid {encoding} at byte {offset}")
            }
            EncodingError::TruncatedUtf16 => f.write_str("truncated UTF-16 input"),
        }
    }
}

impl std::error::Error for EncodingError {}

impl LsError for EncodingError {
    fn code(&self) -> &'static str {
        match self {
            EncodingError::Unsupported(_) => "encoding.unsupported",
            EncodingError::Invalid { .. } => "encoding.invalid",
            EncodingError::TruncatedUtf16 => "encoding.truncated_utf16",
        }
    }

    fn subsystem(&self) -> &'static str {
        "encoding"
    }

    fn recoverability(&self) -> Recoverability {
        Recoverability::UserActionRequired
    }
}

/// Failure to load configuration (specification section 10.2).
#[derive(Debug)]
pub enum ConfigError {
    Syntax { path: PathBuf, message: String },
    Invalid { path: PathBuf, field: &'static str, message: String },
    Io { path: PathBuf, source: std::io::Error },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Syntax { path, message } => {
                write!(f, "{} is not valid configuration: {message}", path.display())
            }
            ConfigError::Invalid { path, field, message } => {
                write!(f, "{} has an invalid value for {field}: {message}", path.display())
            }
            ConfigError::Io { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl LsError for ConfigError {
    fn code(&self) -> &'static str {
        match self {
            ConfigError::Syntax { .. } => "config.syntax",
            ConfigError::Invalid { .. } => "config.invalid_value",
            ConfigError::Io { .. } => "config.io",
        }
    }

    fn subsystem(&self) -> &'static str {
        "config"
    }

    fn recoverability(&self) -> Recoverability {
        Recoverability::UserActionRequired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_reports_a_code_and_recoverability() {
        let errors: Vec<Box<dyn LsError>> = vec![
            Box::new(EditorError::UnknownDocument(DocumentId::new(7))),
            Box::new(OpenDocumentError::NotFound(PathBuf::from("a.txt"))),
            Box::new(PersistenceError::NoPath),
            Box::new(WorkspaceError::NotADirectory(PathBuf::from("a.txt"))),
            Box::new(EncodingError::TruncatedUtf16),
            Box::new(ConfigError::Syntax {
                path: PathBuf::from("config.toml"),
                message: "bad".into(),
            }),
        ];
        for error in errors {
            assert!(error.code().contains('.'), "code {} should be dotted", error.code());
            assert!(!error.subsystem().is_empty());
            assert!(!error.to_string().is_empty());
            let _ = error.recoverability();
        }
    }

    #[test]
    fn binary_rejection_requires_user_action() {
        let error = OpenDocumentError::Binary {
            path: PathBuf::from("a.png"),
            reason: BinaryReason::NulByte { offset: 3 },
        };
        assert_eq!(error.code(), "document.binary_rejected");
        assert_eq!(error.recoverability(), Recoverability::UserActionRequired);
        assert!(error.to_string().contains("NUL byte at offset 3"));
    }
}
