//! Asynchronous document loading (amendment section 7, ADR-0015).
//!
//! Opening a file is split in two, and the split is the whole point:
//!
//! ```text
//! interactive thread          scheduler worker
//! ------------------          ----------------
//! canonicalize path
//! identity lookup / join
//! allocate DocumentId
//! register the loading tab
//! submit  ------------------> read bytes
//!                             detect binary
//!                             decode
//!                             detect line endings
//!                             normalize
//! pump    <------------------ publish LoadResult
//! construct Document
//! publish DocumentLoaded
//! ```
//!
//! Everything on the left is bounded work: a `stat`, two hash lookups and a
//! submission. Everything on the right is proportional to the file. No document
//! is ever mutated by a worker - a worker produces a value, and the interactive
//! thread applies it (amendment section 3.6).

use crate::document::{DiskStamp, DocumentId};
use crate::encoding::{self, Encoding};
use crate::error::OpenDocumentError;
use crate::workspace::Workspace;
use ls_buffer::line_ending::{self, LineEnding};
use ls_buffer::TextBuffer;
use ls_platform::CanonicalPath;
use ls_scheduler::{CancellationToken, TaskId};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Where a document is in its load.
///
/// This is deliberately separate from `ContentState`, `ExternalState` and
/// `PersistenceState` (baseline section 25): a document that is still loading
/// has none of those yet.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LoadState {
    /// A task is reading the file; the tab exists, the document does not.
    Loading,
    /// The document arrived and is editable.
    Loaded,
    /// The load failed; the tab carries the reason.
    Failed,
    /// The load was cancelled before it finished.
    Cancelled,
}

impl LoadState {
    pub const fn name(self) -> &'static str {
        match self {
            LoadState::Loading => "loading",
            LoadState::Loaded => "loaded",
            LoadState::Failed => "failed",
            LoadState::Cancelled => "cancelled",
        }
    }

    pub const fn is_settled(self) -> bool {
        !matches!(self, LoadState::Loading)
    }
}

/// Controlled misbehaviour for the development panel.
///
/// Loading is hard to demonstrate on a fast machine with a small file: the tab
/// is gone before anyone sees it. These knobs make the states reachable without
/// needing a 100 MB file to hand. Default is "behave normally".
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadInjection {
    /// Sleep this long inside the task, in cancellable slices.
    pub delay: Option<Duration>,
    /// Fail the load with this code instead of reading the file.
    pub fail_with: Option<&'static str>,
}

impl LoadInjection {
    pub const NONE: LoadInjection = LoadInjection { delay: None, fail_with: None };

    pub fn delayed(delay: Duration) -> Self {
        LoadInjection { delay: Some(delay), fail_with: None }
    }

    pub fn failing() -> Self {
        LoadInjection { delay: None, fail_with: Some("diagnostics.injected_failure") }
    }

    pub fn is_none(&self) -> bool {
        *self == LoadInjection::NONE
    }
}

/// A load the editor is waiting for.
#[derive(Clone, Debug)]
pub struct PendingLoad {
    pub task: TaskId,
    pub path: CanonicalPath,
    pub requested_at: Instant,
    /// How many requests are riding on this one task. Starts at 1; every
    /// same-path request while it is in flight adds one.
    pub joins: u32,
    pub cancellation: CancellationToken,
}

impl PendingLoad {
    pub fn elapsed(&self) -> Duration {
        self.requested_at.elapsed()
    }
}

/// What the worker produces. Plain data, `Send`, and free of any borrow on the
/// editor.
#[derive(Debug)]
pub struct LoadedData {
    pub document: DocumentId,
    pub path: CanonicalPath,
    /// The rope, built on the worker so the interactive thread never pays for
    /// it (amendment section 7). Line endings are already normalized.
    pub buffer: TextBuffer,
    pub encoding: Encoding,
    pub line_ending: LineEnding,
    pub mixed_line_endings: bool,
    pub stamp: DiskStamp,
    pub bytes_read: u64,
}

/// The task's outcome, as the interactive thread receives it.
#[derive(Debug)]
pub enum LoadResult {
    Loaded(Box<LoadedData>),
    Failed { document: DocumentId, error: OpenDocumentError },
    Cancelled { document: DocumentId },
}

impl LoadResult {
    pub fn document(&self) -> DocumentId {
        match self {
            LoadResult::Loaded(data) => data.document,
            LoadResult::Failed { document, .. } => *document,
            LoadResult::Cancelled { document } => *document,
        }
    }
}

/// Bytes of a file read before the first cancellation check.
///
/// Small enough that cancelling a 100 MB load does not wait for the whole file,
/// large enough that the check is not the cost of the read.
pub const READ_CHUNK: usize = 1024 * 1024;

/// Reads and decodes a file. Runs on a scheduler worker.
///
/// The cancellation token is polled between chunks and between phases, so a
/// cancelled load stops within roughly one chunk rather than at the end.
pub fn load_from_disk(
    workspace: &Workspace,
    document: DocumentId,
    path: &CanonicalPath,
    injection: LoadInjection,
    cancellation: &CancellationToken,
) -> LoadResult {
    if let Some(code) = injection.fail_with {
        return LoadResult::Failed {
            document,
            error: OpenDocumentError::Io {
                path: path.as_path().to_path_buf(),
                source: std::io::Error::other(format!("injected load failure ({code})")),
            },
        };
    }

    if let Some(delay) = injection.delay {
        // Slept in slices so an injected delay stays cancellable.
        let deadline = Instant::now() + delay;
        while Instant::now() < deadline {
            if cancellation.is_cancelled() {
                return LoadResult::Cancelled { document };
            }
            std::thread::sleep(Duration::from_millis(5).min(delay));
        }
    }

    if cancellation.is_cancelled() {
        return LoadResult::Cancelled { document };
    }

    let bytes =
        match workspace.read_file_cancellable(path.as_path(), &|| cancellation.is_cancelled()) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return LoadResult::Cancelled { document },
            Err(error) => {
                return LoadResult::Failed { document, error: OpenDocumentError::from(error) }
            }
        };

    if cancellation.is_cancelled() {
        return LoadResult::Cancelled { document };
    }

    if let Some(reason) = encoding::detect_binary(&bytes) {
        return LoadResult::Failed {
            document,
            error: OpenDocumentError::Binary { path: path.as_path().to_path_buf(), reason },
        };
    }

    let decoded = match encoding::decode(&bytes) {
        Ok(decoded) => decoded,
        Err(source) => {
            return LoadResult::Failed {
                document,
                error: OpenDocumentError::Encoding { path: path.as_path().to_path_buf(), source },
            }
        }
    };

    if cancellation.is_cancelled() {
        return LoadResult::Cancelled { document };
    }

    let analysis = line_ending::detect(&decoded.text);
    let normalized = line_ending::normalize(&decoded.text);
    // Building the rope here is the difference between a 110 ms frame and a
    // free one: it is proportional to the file, so it belongs on the worker.
    let buffer = TextBuffer::from_str(&normalized);
    let stamp = workspace
        .stamp(path.as_path())
        .unwrap_or(DiskStamp { modified: None, len_bytes: bytes.len() as u64 });

    LoadResult::Loaded(Box::new(LoadedData {
        document,
        path: path.clone(),
        buffer,
        encoding: decoded.encoding,
        line_ending: analysis.dominant,
        mixed_line_endings: analysis.mixed,
        stamp,
        bytes_read: bytes.len() as u64,
    }))
}

/// One finished (or in-flight) load, for the development panel and for tests.
#[derive(Clone, Debug)]
pub struct LoadRecord {
    pub document: DocumentId,
    pub task: TaskId,
    pub path: String,
    pub state: LoadState,
    /// Requests that shared this one load. `1` means nobody joined.
    pub joins: u32,
    /// Request to settlement, as the user experiences it.
    pub total: Option<Duration>,
    /// From the scheduler's accounting, when the task reported one.
    pub queue_wait: Option<Duration>,
    pub wall_time: Option<Duration>,
    pub bytes: u64,
    pub error: Option<String>,
}

impl LoadRecord {
    pub fn is_joined(&self) -> bool {
        self.joins > 1
    }
}

/// A bounded window of recent loads.
///
/// Bounded for the same reason every other queue is (amendment section 4): an
/// editor that runs all day must not accumulate a day of history.
#[derive(Debug)]
pub struct LoadActivity {
    entries: VecDeque<LoadRecord>,
    capacity: usize,
}

impl Default for LoadActivity {
    fn default() -> Self {
        LoadActivity::new(32)
    }
}

impl LoadActivity {
    pub fn new(capacity: usize) -> Self {
        LoadActivity { entries: VecDeque::new(), capacity: capacity.max(1) }
    }

    pub fn started(&mut self, record: LoadRecord) {
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(record);
    }

    /// Updates the newest entry for a document, which is the one still open.
    pub fn update<F: FnOnce(&mut LoadRecord)>(&mut self, document: DocumentId, apply: F) {
        if let Some(entry) = self.entries.iter_mut().rev().find(|entry| entry.document == document)
        {
            apply(entry);
        }
    }

    /// Newest first.
    pub fn recent(&self) -> impl Iterator<Item = &LoadRecord> {
        self.entries.iter().rev()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(document: u64, task: u64, state: LoadState) -> LoadRecord {
        LoadRecord {
            document: DocumentId::new(document),
            task: TaskId::new(task),
            path: format!("file{document}.txt"),
            state,
            joins: 1,
            total: None,
            queue_wait: None,
            wall_time: None,
            bytes: 0,
            error: None,
        }
    }

    #[test]
    fn load_states_settle_exactly_once_loading_is_over() {
        assert!(!LoadState::Loading.is_settled());
        assert!(LoadState::Loaded.is_settled());
        assert!(LoadState::Failed.is_settled());
        assert!(LoadState::Cancelled.is_settled());
    }

    #[test]
    fn injection_defaults_to_behaving_normally() {
        assert!(LoadInjection::default().is_none());
        assert!(!LoadInjection::failing().is_none());
        assert!(!LoadInjection::delayed(Duration::from_millis(1)).is_none());
    }

    #[test]
    fn activity_is_bounded_and_newest_first() {
        let mut activity = LoadActivity::new(3);
        for index in 0..6 {
            activity.started(record(index, index, LoadState::Loading));
        }
        assert_eq!(activity.len(), 3);
        let documents: Vec<u64> = activity.recent().map(|entry| entry.document.get()).collect();
        assert_eq!(documents, vec![5, 4, 3], "newest first, oldest dropped");
    }

    #[test]
    fn updating_touches_the_newest_entry_for_a_document() {
        let mut activity = LoadActivity::new(8);
        activity.started(record(1, 10, LoadState::Loading));
        activity.started(record(2, 11, LoadState::Loading));
        activity.started(record(1, 12, LoadState::Loading));

        activity.update(DocumentId::new(1), |entry| entry.state = LoadState::Loaded);
        let newest_for_one =
            activity.recent().find(|entry| entry.document == DocumentId::new(1)).unwrap();
        assert_eq!(newest_for_one.task, TaskId::new(12));
        assert_eq!(newest_for_one.state, LoadState::Loaded);

        // The older entry for the same document is untouched.
        let older =
            activity.recent().filter(|entry| entry.document == DocumentId::new(1)).nth(1).unwrap();
        assert_eq!(older.state, LoadState::Loading);
    }

    #[test]
    fn updating_an_unknown_document_is_harmless() {
        let mut activity = LoadActivity::new(4);
        activity.started(record(1, 1, LoadState::Loading));
        activity.update(DocumentId::new(99), |entry| entry.state = LoadState::Failed);
        assert_eq!(activity.recent().next().unwrap().state, LoadState::Loading);
    }

    #[test]
    fn a_joined_record_is_visible_as_joined() {
        let mut joined = record(1, 1, LoadState::Loading);
        assert!(!joined.is_joined(), "one request is not a join");
        joined.joins = 3;
        assert!(joined.is_joined());
    }
}
