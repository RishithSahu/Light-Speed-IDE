//! Filesystem watch task plumbing (ADR-0017).
//!
//! `EditorCore` submits one [`ls_platform::watch::wait_for_change`] per
//! watched directory as an ordinary [`ls_scheduler::SubsystemId::WATCH`] task
//! and re-arms it every time it completes, so the watcher lives entirely
//! inside the same admission/completion path a document load or a git-status
//! task already uses instead of needing a thread of its own.

use std::path::PathBuf;

/// What one completed watch task discovered.
///
/// An empty `changed` list means the OS could not say exactly what changed
/// (its notification buffer overflowed): the caller re-checks every open
/// document under `directory` rather than trusting emptiness to mean nothing
/// happened.
#[derive(Clone, Debug)]
pub struct WatchedPaths {
    pub directory: PathBuf,
    pub recursive: bool,
    pub changed: Vec<PathBuf>,
}
