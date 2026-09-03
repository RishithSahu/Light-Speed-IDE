//! Shared diagnostic vocabulary for typed errors (specification section 39).
//!
//! Every LightSpeed subsystem defines its own error enum, but all of them
//! answer the same four questions: what code, which subsystem, what happened,
//! and can the caller do something about it. Keeping that vocabulary in the
//! logging crate means an error can always be reported the same way, without
//! forcing every subsystem to depend on every other subsystem.
//!
//! Cancellation is deliberately absent: cancellation is normal control flow,
//! not an error.

use std::fmt;

/// What a caller can do about a failure (specification section 39).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Recoverability {
    /// The operation failed but the subsystem is healthy; carry on.
    Recoverable,
    /// A retry has a reasonable chance of succeeding.
    Retryable,
    /// A human must decide (overwrite, reload, pick another path).
    UserActionRequired,
    /// This subsystem is no longer usable; the rest of the editor continues.
    FatalToSubsystem,
}

impl Recoverability {
    pub const fn label(self) -> &'static str {
        match self {
            Recoverability::Recoverable => "recoverable",
            Recoverability::Retryable => "retryable",
            Recoverability::UserActionRequired => "user_action_required",
            Recoverability::FatalToSubsystem => "fatal_to_subsystem",
        }
    }
}

/// Implemented by every typed error in the workspace.
pub trait LsError: fmt::Display {
    /// Stable, machine-readable identifier, e.g. `document.binary_rejected`.
    fn code(&self) -> &'static str;
    /// Subsystem that produced the error, e.g. `core`, `platform`.
    fn subsystem(&self) -> &'static str;
    /// What the caller can do about it.
    fn recoverability(&self) -> Recoverability;
    /// Underlying cause, when one exists.
    fn cause(&self) -> Option<&dyn std::error::Error> {
        None
    }
}

/// Logs an error at the level implied by its recoverability.
pub fn log_error(error: &dyn LsError) {
    let recoverability = error.recoverability();
    let message = error.to_string();
    let fields = [
        crate::Field::str("code", error.code()),
        crate::Field::str("recoverability", recoverability.label()),
    ];
    let level = match recoverability {
        Recoverability::Recoverable | Recoverability::Retryable => crate::Level::Warn,
        Recoverability::UserActionRequired | Recoverability::FatalToSubsystem => {
            crate::Level::Error
        }
    };
    if crate::enabled(level) {
        crate::emit(&crate::LogRecord {
            level,
            subsystem: error.subsystem(),
            event: "error",
            message: format_args!("{message}"),
            fields: &fields,
        });
    }
}
