//! LightSpeed editor core.
//!
//! Everything an editor needs that is not a window: documents, revisions,
//! cursors, undo, encoding, persistence, configuration, commands, events and
//! the immutable snapshots the renderer draws.
//!
//! ```text
//! EditorCore
//!  ├── Workspace        file bytes, durable replacement, lazy enumeration
//!  ├── Document         TextBuffer + selection + history + metadata
//!  ├── CommandRegistry  the single routing mechanism for actions
//!  ├── EventQueue       bounded, drained by the shell
//!  └── RenderSnapshot   immutable presentation for exactly one frame
//! ```
//!
//! The core owns no threads, no timers and no GUI. Every operation is
//! synchronous and measurable, and the whole crate is testable headless -
//! which is what the test suite does.
//!
//! # Stage 1 scope
//!
//! Present: text editing, cursor/selection, clipboard, undo with coalescing,
//! document revisions, encoding and line endings, open/save with atomic
//! replacement, viewport and snapshots, tabs, configuration, logging hooks,
//! command registry, event model, performance instrumentation.
//!
//! Absent by design (Foundation Stage): syntax highlighting, workspace search,
//! Git, terminal, language services, the filesystem watcher and the task
//! scheduler. Their contracts are documented in the specification; this crate
//! does not pretend to implement them.

pub mod commands;
pub mod config;
pub mod document;
pub mod editor;
pub mod encoding;
pub mod error;
pub mod events;
pub mod git;
pub mod highlight;
pub mod history;
pub mod language;
pub mod loading;
pub mod persistence;
pub mod render;
pub mod search;
pub mod selection;
pub mod workspace;
pub mod workspace_search;

pub use commands::{CommandArgs, CommandDescriptor, ShellRequest};
pub use config::{AppearanceConfig, EditorConfig, EffectiveConfig, PerformanceConfig};
pub use document::{
    ContentRevision, ContentState, DiskStamp, Document, DocumentId, DocumentSettings, EditResult,
    ExternalState, PersistenceState,
};
pub use editor::{EditorCore, OpenRequest, Position, TabPresentation, TextRange, MAX_RECENT_FILES};
pub use encoding::Encoding;
pub use error::{
    BinaryReason, ConfigError, EditorError, EncodingError, OpenDocumentError, PersistenceError,
    WorkspaceError,
};
pub use events::{Event, EventPayload, EventQueue};
pub use highlight::TokenKind;
pub use history::{Edit, EditHistory, EditKind, Transaction, TransactionId};
pub use language::{detect_language, Language};
pub use loading::{LoadActivity, LoadInjection, LoadRecord, LoadState, PendingLoad};
pub use persistence::{
    PendingSave, SaveActivity, SaveDisposition, SaveRecord, SaveRequestOutcome, SaveSnapshot,
};
pub use render::{
    build_snapshot, CursorPresentation, Decoration, DecorationKind, Diagnostic, DiagnosticSeverity,
    DocumentPresentation, Invalidation, RenderLine, RenderSnapshot, SelectionSpan, Viewport,
};
pub use selection::{Movement, MovementContext, Selection, SelectionSet};
pub use workspace::{EntryKind, FileEntry, Workspace, WorkspaceId};

// Re-exported so the shell does not need to depend on ls-buffer directly for
// ordinary work.
pub use ls_buffer::{ByteOffset, CharOffset, DisplayColumn, LineEnding, LineIndex, TextBuffer};

// Re-exported so a Resource Center can read admission and accounting state
// through `EditorCore::scheduler()` without the shell depending on
// ls-scheduler directly (an architecture test enforces that boundary: only
// the editor core submits work to it, but reading its own published numbers
// back out is not submitting work).
pub use ls_scheduler::{accounting, SubsystemId, TaskRecord, TaskState};

/// Version of the editor core, for logs and benchmark reports.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
