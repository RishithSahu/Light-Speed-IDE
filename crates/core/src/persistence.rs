//! Asynchronous, revision-aware saving (amendment sections 8-10, ADR-0015).
//!
//! A save is performed against one exact version of a document, and finishing
//! says nothing about versions that came after it:
//!
//! ```text
//! interactive thread              scheduler worker
//! ------------------              ----------------
//! capture revision + token
//! clone the rope (O(1))
//! submit  ----------------------> encode
//!                                 temporary file
//!                                 flush + fsync
//!                                 atomic replace
//! pump    <---------------------- SaveOutcome { revision, token, stamp }
//! compare captured token
//!   equal   -> Clean
//!   changed -> still Dirty
//! ```
//!
//! # Two versions, two jobs (amendment section 8.1)
//!
//! * `content_revision` identifies the exact content the worker wrote. It is
//!   what makes a completion recognizable as stale.
//! * `TransactionId` decides whether the document is clean, because undoing
//!   back to the saved content must count as clean even though revisions only
//!   ever increase.

use crate::document::{DiskStamp, DocumentId};
use crate::encoding::{self, Encoding};
use crate::error::PersistenceError;
use crate::history::TransactionId;
use crate::workspace::Workspace;
use ls_buffer::line_ending::LineEnding;
use ls_buffer::TextBuffer;
use ls_platform::CanonicalPath;
use ls_scheduler::TaskId;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// An immutable copy of everything needed to write one document version.
///
/// This is what crosses the scheduler boundary instead of a borrow on the
/// editor. The rope clone is `O(1)`: the buffer is copy-on-write, so the
/// snapshot shares structure with the live document and the document stays
/// fully editable while it is being written.
#[derive(Clone, Debug)]
pub struct SaveSnapshot {
    pub document: DocumentId,
    /// The exact content version being written.
    pub revision: crate::document::ContentRevision,
    /// The history position this content corresponds to. Clean/dirty is decided
    /// by comparing this against the document's token at completion.
    pub token: TransactionId,
    pub path: CanonicalPath,
    pub encoding: Encoding,
    pub line_ending: LineEnding,
    /// Shared structure, not a second copy of the document.
    pub buffer: TextBuffer,
}

impl SaveSnapshot {
    /// Characters in the snapshot, for cost estimation and accounting.
    pub fn len_chars(&self) -> usize {
        self.buffer.len_chars()
    }

    pub fn len_bytes(&self) -> usize {
        self.buffer.len_bytes()
    }
}

/// What the worker produced.
#[derive(Debug)]
pub struct SaveOutcome {
    pub document: DocumentId,
    pub revision: crate::document::ContentRevision,
    pub token: TransactionId,
    /// Re-canonicalized after the write, because a Save As target only becomes
    /// a real path once the file exists.
    pub path: CanonicalPath,
    pub result: Result<DiskStamp, PersistenceError>,
    pub bytes_written: u64,
}

impl SaveOutcome {
    pub fn succeeded(&self) -> bool {
        self.result.is_ok()
    }
}

/// Writes a snapshot to disk. Runs on a scheduler worker.
///
/// The durability sequence is unchanged from Stage 1 (baseline section 29):
/// temporary file, write, flush, fsync, atomic replace. Only its execution
/// context moved. The encoder streams the rope chunk by chunk, so no
/// document-sized second allocation exists anywhere on this path.
pub fn write_snapshot(workspace: &Workspace, snapshot: &SaveSnapshot) -> SaveOutcome {
    let write_result = workspace.write_file_atomic_with(snapshot.path.as_path(), |writer| {
        encoding::encode_to(
            writer,
            snapshot.buffer.chunks(),
            snapshot.encoding,
            snapshot.line_ending,
        )
    });

    match write_result {
        Ok(()) => {
            // The file exists now, so identity is exact.
            let canonical = CanonicalPath::new(snapshot.path.as_path())
                .unwrap_or_else(|_| snapshot.path.clone());
            let stamp = workspace
                .stamp(canonical.as_path())
                .unwrap_or(DiskStamp { modified: None, len_bytes: 0 });
            let bytes_written = stamp.len_bytes;
            SaveOutcome {
                document: snapshot.document,
                revision: snapshot.revision,
                token: snapshot.token,
                path: canonical,
                result: Ok(stamp),
                bytes_written,
            }
        }
        Err(error) => SaveOutcome {
            document: snapshot.document,
            revision: snapshot.revision,
            token: snapshot.token,
            path: snapshot.path.clone(),
            result: Err(error),
            bytes_written: 0,
        },
    }
}

/// A save the editor is waiting for.
#[derive(Clone, Debug)]
pub struct PendingSave {
    pub task: TaskId,
    pub revision: crate::document::ContentRevision,
    pub token: TransactionId,
    pub path: CanonicalPath,
    pub requested_at: Instant,
}

impl PendingSave {
    pub fn elapsed(&self) -> Duration {
        self.requested_at.elapsed()
    }
}

/// How a save request was handled.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SaveDisposition {
    /// Submitted to the scheduler immediately.
    Started,
    /// A save for this document is already running; this one waits behind it.
    Queued,
    /// A save was already queued and has been replaced by this newer one.
    /// Nothing is lost: the queued snapshot was older content.
    SupersededQueued,
}

/// The result of asking for a save.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SaveRequestOutcome {
    pub document: DocumentId,
    /// `None` while the request is queued behind an in-flight save.
    pub task: Option<TaskId>,
    pub revision: crate::document::ContentRevision,
    pub disposition: SaveDisposition,
}

/// One finished (or in-flight) save, for the status area and for tests.
#[derive(Clone, Debug)]
pub struct SaveRecord {
    pub document: DocumentId,
    pub task: TaskId,
    pub path: String,
    pub revision: crate::document::ContentRevision,
    pub succeeded: Option<bool>,
    /// True when the document changed while the save was running, so the save
    /// landed stale and the document stayed dirty.
    pub stale: bool,
    pub bytes_written: u64,
    pub total: Option<Duration>,
    pub queue_wait: Option<Duration>,
    pub wall_time: Option<Duration>,
    pub error: Option<String>,
}

/// A bounded window of recent saves.
#[derive(Debug)]
pub struct SaveActivity {
    entries: VecDeque<SaveRecord>,
    capacity: usize,
}

impl Default for SaveActivity {
    fn default() -> Self {
        SaveActivity::new(32)
    }
}

impl SaveActivity {
    pub fn new(capacity: usize) -> Self {
        SaveActivity { entries: VecDeque::new(), capacity: capacity.max(1) }
    }

    pub fn started(&mut self, record: SaveRecord) {
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(record);
    }

    /// Updates the newest entry for a task.
    pub fn update<F: FnOnce(&mut SaveRecord)>(&mut self, task: TaskId, apply: F) {
        if let Some(entry) = self.entries.iter_mut().rev().find(|entry| entry.task == task) {
            apply(entry);
        }
    }

    /// Newest first.
    pub fn recent(&self) -> impl Iterator<Item = &SaveRecord> {
        self.entries.iter().rev()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::ContentRevision;

    fn record(document: u64, task: u64) -> SaveRecord {
        SaveRecord {
            document: DocumentId::new(document),
            task: TaskId::new(task),
            path: format!("file{document}.txt"),
            revision: ContentRevision::default(),
            succeeded: None,
            stale: false,
            bytes_written: 0,
            total: None,
            queue_wait: None,
            wall_time: None,
            error: None,
        }
    }

    #[test]
    fn a_snapshot_shares_the_rope_rather_than_copying_it() {
        // The whole point of the snapshot: O(1), no second allocation.
        let buffer = TextBuffer::from_str(&"a line of text\n".repeat(50_000));
        let snapshot = SaveSnapshot {
            document: DocumentId::new(1),
            revision: ContentRevision::default(),
            token: TransactionId::ROOT,
            path: CanonicalPath::unverified("/tmp/x.txt").unwrap(),
            encoding: Encoding::Utf8,
            line_ending: LineEnding::Lf,
            buffer: buffer.snapshot(),
        };
        assert_eq!(snapshot.len_chars(), buffer.len_chars());
        assert_eq!(snapshot.len_bytes(), buffer.len_bytes());
        // Equal contents, and the clone was a pointer copy.
        assert_eq!(snapshot.buffer, buffer);
    }

    #[test]
    fn a_snapshot_is_unaffected_by_later_edits() {
        let mut live = TextBuffer::from_str("original");
        let snapshot = live.snapshot();
        live.insert(ls_buffer::CharOffset::ZERO, "edited ");
        assert_eq!(snapshot.to_string(), "original");
        assert_eq!(live.to_string(), "edited original");
    }

    #[test]
    fn activity_is_bounded_and_newest_first() {
        let mut activity = SaveActivity::new(3);
        for index in 0..6 {
            activity.started(record(index, index));
        }
        assert_eq!(activity.len(), 3);
        let tasks: Vec<u64> = activity.recent().map(|entry| entry.task.get()).collect();
        assert_eq!(tasks, vec![5, 4, 3]);
    }

    #[test]
    fn updating_touches_the_matching_task() {
        let mut activity = SaveActivity::new(8);
        activity.started(record(1, 10));
        activity.started(record(1, 11));
        activity.update(TaskId::new(10), |entry| entry.succeeded = Some(true));

        let ten = activity.recent().find(|entry| entry.task == TaskId::new(10)).unwrap();
        assert_eq!(ten.succeeded, Some(true));
        let eleven = activity.recent().find(|entry| entry.task == TaskId::new(11)).unwrap();
        assert_eq!(eleven.succeeded, None, "the other entry is untouched");
    }

    #[test]
    fn dispositions_are_distinct() {
        assert_ne!(SaveDisposition::Started, SaveDisposition::Queued);
        assert_ne!(SaveDisposition::Queued, SaveDisposition::SupersededQueued);
    }
}
