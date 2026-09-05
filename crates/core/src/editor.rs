//! `EditorCore`: the API the shell talks to (specification section 30).
//!
//! The core owns documents, tabs, the clipboard interface, the event queue and
//! command dispatch. It has no window, no dialogs, no threads and no timers, so
//! every one of its operations is synchronous, measurable and testable without
//! opening a GUI.
//!
//! Things the core deliberately cannot do: show a dialog, quit the application,
//! toggle an overlay. Commands that need those hand a [`ShellRequest`] back to
//! the shell instead of reaching across the boundary.

use crate::commands::{self, CommandArgs, ShellRequest};
use crate::document::{ContentRevision, Document, DocumentId, EditResult, ExternalState};
use crate::error::{EditorError, OpenDocumentError, PersistenceError, WorkspaceError};
use crate::events::{Event, EventPayload, EventQueue};
use crate::git::{self, GitStatus};
use crate::history::{Edit, EditKind};
use crate::loading::{
    self, LoadActivity, LoadInjection, LoadRecord, LoadResult, LoadState, PendingLoad,
};
use crate::persistence::{
    self, PendingSave, SaveActivity, SaveDisposition, SaveOutcome, SaveRecord, SaveRequestOutcome,
    SaveSnapshot,
};
use crate::render::{self, RenderSnapshot, Viewport};
use crate::selection::{Movement, Selection};
use crate::watch::WatchedPaths;
use crate::workspace::{Workspace, WorkspaceId};
use crate::workspace_search::{self, WorkspaceSearchResult};
use crate::EffectiveConfig;
use ls_buffer::{line_ending, CharOffset, LineIndex};
use ls_log::diag::LsError;
use ls_platform::{CanonicalPath, Clipboard};
use ls_scheduler::{
    CancellationToken, CompletionOutcome, CostEstimate, ResourceClass, Scheduler, SchedulerConfig,
    SubsystemId, TaskFailure, TaskId, TaskOutcome, TaskProduct, TaskSpec, WorkspaceRef,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SUBSYSTEM: &str = "core";

/// How often a watch task re-checks its cancellation flag while it waits for
/// the OS to report a change (docs/adr/ADR-0017-filesystem-change-notification.md).
/// Short enough that closing a document or the workspace is noticed promptly;
/// long enough to spend almost all of that time asleep in the kernel rather
/// than spinning.
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// A line/column position (specification section 15: never a byte offset).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub line: LineIndex,
    /// Characters into the line.
    pub column: usize,
}

impl Position {
    pub fn new(line: usize, column: usize) -> Self {
        Position { line: LineIndex::new(line), column }
    }
}

/// A range between two positions.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TextRange {
    pub start: Position,
    pub end: Position,
}

/// What a tab shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabPresentation {
    pub id: DocumentId,
    pub title: String,
    pub tooltip: Option<String>,
    pub dirty: bool,
    pub active: bool,
    /// The file is still being read; there is no document behind this tab yet.
    pub loading: bool,
}

struct Metrics {
    edit: ls_perf::MetricHandle,
    cursor: ls_perf::MetricHandle,
    selection: ls_perf::MetricHandle,
    undo_redo: ls_perf::MetricHandle,
    tab_switch: ls_perf::MetricHandle,
    open: ls_perf::MetricHandle,
    save: ls_perf::MetricHandle,
}

impl Metrics {
    fn new() -> Self {
        Metrics {
            edit: ls_perf::metric(ls_perf::names::EDIT_APPLY),
            cursor: ls_perf::metric(ls_perf::names::CURSOR_MOVE),
            selection: ls_perf::metric(ls_perf::names::SELECTION_MOVE),
            undo_redo: ls_perf::metric(ls_perf::names::UNDO_REDO),
            tab_switch: ls_perf::metric(ls_perf::names::TAB_SWITCH),
            open: ls_perf::metric(ls_perf::names::DOCUMENT_OPEN),
            save: ls_perf::metric(ls_perf::names::DOCUMENT_SAVE),
        }
    }
}

/// Maps a selection through an edit so it keeps pointing at the same text.
fn transform_selection(selection: Selection, edit: &Edit) -> Selection {
    Selection {
        anchor: edit.transform(selection.anchor),
        head: edit.transform(selection.head),
        // The goal column is a movement artefact and does not survive an edit.
        goal_column: None,
    }
}

/// The scheduler policy the application runs with.
///
/// The platform can report per-thread CPU time and the scheduler cannot reach
/// the platform crate, so the source is injected here (amendment section 6).
pub fn default_scheduler_config() -> SchedulerConfig {
    SchedulerConfig {
        cpu_time_source: Some(ls_platform::process::thread_cpu_time),
        ..SchedulerConfig::default()
    }
}

/// Declares the Stage 1 performance contracts (specification section 48).
pub fn install_default_budgets() {
    use ls_perf::{names, set_budget, Budget};
    set_budget(names::INPUT_TO_STATE, Budget::from_millis(2, 5));
    set_budget(names::INPUT_TO_FRAME, Budget::from_millis(8, 16));
    set_budget(names::FRAME, Budget::from_millis(8, 16));
    set_budget(names::CURSOR_MOVE, Budget::from_millis(4, 10));
    set_budget(names::SELECTION_MOVE, Budget::from_millis(4, 10));
    set_budget(names::EDIT_APPLY, Budget::from_millis(2, 5));
    set_budget(names::UNDO_REDO, Budget::from_millis(2, 5));
    set_budget(names::TAB_SWITCH, Budget::from_millis(2, 5));
    set_budget(names::DOCUMENT_OPEN, Budget::from_millis(20, 50));
    set_budget(names::STARTUP_USABLE, Budget::from_millis(500, 1000));
}

/// The editor.
pub struct EditorCore {
    documents: HashMap<DocumentId, Document>,
    order: Vec<DocumentId>,
    active: Option<DocumentId>,
    by_path: HashMap<String, DocumentId>,
    next_document_id: u64,
    untitled_counter: u64,
    workspace: Workspace,
    clipboard: Box<dyn Clipboard>,
    config: EffectiveConfig,
    events: EventQueue,
    shell_requests: Vec<ShellRequest>,
    published_revisions: HashMap<DocumentId, ContentRevision>,
    page_lines: usize,
    last_error: Option<String>,
    metrics: Metrics,
    /// Admission authority for every document load (amendment section 3.4).
    scheduler: Scheduler,
    /// Loads the editor is waiting for, keyed by the tab that is showing them.
    loading: HashMap<DocumentId, PendingLoad>,
    /// Recent loads, for the development panel and for tests.
    activity: LoadActivity,
    /// The most recent load failure, so the synchronous helper can report it.
    last_failed_load: Option<(DocumentId, OpenDocumentError)>,
    /// At most one save in flight per document (amendment section 9).
    saving: HashMap<DocumentId, PendingSave>,
    /// At most one save queued per document. A newer request replaces the
    /// queued one rather than growing a chain.
    queued_saves: HashMap<DocumentId, SaveSnapshot>,
    save_activity: SaveActivity,
    /// The most recent save failure, so the synchronous helper can report it.
    last_failed_save: Option<(DocumentId, PersistenceError)>,
    /// Paths successfully resolved for opening, most-recent first. A shell
    /// preference, not document content -- persisted by the shell through
    /// `ls_platform::recents`, the same way it already persists nothing else
    /// about a document (specification section 12: the core holds state, the
    /// shell decides how to show it).
    recent_files: Vec<PathBuf>,
    /// Whether the find bar is shown. Independent of the query being empty:
    /// the bar can be open with nothing typed into it yet.
    find_open: bool,
    /// The one outstanding `git status` task, if a request is in flight.
    pending_git_status: Option<TaskId>,
    /// The last status this workspace reported.
    git_status: Option<GitStatus>,
    /// The one outstanding workspace search, if a request is in flight. A new
    /// search cancels and replaces it rather than letting two walks race.
    pending_search: Option<TaskId>,
    workspace_search_result: Option<WorkspaceSearchResult>,
    /// The active filesystem watch task for each watched directory, keyed by
    /// [`CanonicalPath::key`] (ADR-0017). Reconciled by
    /// [`EditorCore::sync_watchers`] against the workspace root and every
    /// open document's directory; re-armed by
    /// [`EditorCore::apply_watch_completion`] using the directory and
    /// recursion flag carried in the completion itself, so nothing beyond the
    /// `TaskId` needs to be kept here.
    watching: HashMap<String, TaskId>,
}

/// Files opened recently, most-recent first (capped at
/// [`ls_platform::recents::MAX_RECENT`]).
pub const MAX_RECENT_FILES: usize = ls_platform::recents::MAX_RECENT;

/// What a request to open a document produced.
///
/// A request never blocks: it either joins something already in progress or
/// starts a task, and the document arrives later through
/// [`EditorCore::pump_completions`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OpenRequest {
    /// The tab that will show this file. It exists immediately, in a loading
    /// state, even though the document does not exist yet.
    pub document: DocumentId,
    /// The task doing the reading. `None` when the file was already open.
    pub task: Option<TaskId>,
    /// True when this request attached to work that was already happening.
    pub joined: bool,
    /// True when the file was already open and nothing needed loading.
    pub already_open: bool,
}

impl EditorCore {
    /// Builds an editor using the platform clipboard.
    pub fn new(config: EffectiveConfig) -> Self {
        Self::with_clipboard(config, ls_platform::system_clipboard())
    }

    /// Builds an editor with an injected clipboard, which is what tests use.
    pub fn with_clipboard(config: EffectiveConfig, clipboard: Box<dyn Clipboard>) -> Self {
        Self::with_clipboard_and_scheduler(config, clipboard, default_scheduler_config())
    }

    /// Builds an editor with an explicit scheduler policy.
    ///
    /// Tests use this to shrink the admission queue so rejection is reachable;
    /// the application uses the default.
    pub fn with_clipboard_and_scheduler(
        config: EffectiveConfig,
        clipboard: Box<dyn Clipboard>,
        scheduler: SchedulerConfig,
    ) -> Self {
        install_default_budgets();
        ls_perf::set_enabled(config.performance.instrumentation);
        EditorCore {
            documents: HashMap::new(),
            order: Vec::new(),
            active: None,
            by_path: HashMap::new(),
            next_document_id: 1,
            untitled_counter: 0,
            workspace: Workspace::rootless(),
            clipboard,
            config,
            events: EventQueue::default(),
            shell_requests: Vec::new(),
            published_revisions: HashMap::new(),
            page_lines: 20,
            last_error: None,
            metrics: Metrics::new(),
            scheduler: Scheduler::new(scheduler),
            loading: HashMap::new(),
            activity: LoadActivity::default(),
            last_failed_load: None,
            saving: HashMap::new(),
            queued_saves: HashMap::new(),
            save_activity: SaveActivity::default(),
            last_failed_save: None,
            recent_files: Vec::new(),
            find_open: false,
            pending_git_status: None,
            git_status: None,
            pending_search: None,
            workspace_search_result: None,
            watching: HashMap::new(),
        }
    }

    // --- configuration and workspace ----------------------------------------

    pub fn config(&self) -> &EffectiveConfig {
        &self.config
    }

    /// Replaces the effective configuration. Open documents pick up the new
    /// editor settings; the shell reacts to appearance changes itself.
    pub fn set_config(&mut self, config: EffectiveConfig) {
        let settings = config.document_settings();
        self.config = config;
        ls_perf::set_enabled(self.config.performance.instrumentation);
        for document in self.documents.values_mut() {
            document.apply_settings(settings);
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// Opens a project directory. Does not scan it: traversal is lazy, and
    /// recursive traversal is a Foundation Stage background task.
    pub fn open_workspace(&mut self, root: &Path) -> Result<WorkspaceId, WorkspaceError> {
        let canonical = CanonicalPath::new(root).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => WorkspaceError::NotFound(root.to_path_buf()),
            _ => WorkspaceError::Io { path: root.to_path_buf(), source },
        })?;
        if !canonical.as_path().is_dir() {
            return Err(WorkspaceError::NotADirectory(root.to_path_buf()));
        }
        let id = WorkspaceId::new(self.workspace.id().get() + 1);
        let display = canonical.display_string();
        self.workspace = Workspace::with_root(id, canonical);
        self.sync_watchers();
        ls_log::info!(SUBSYSTEM, "workspace_opened", "workspace opened {display}");
        self.events.emit(SUBSYSTEM, EventPayload::WorkspaceOpened { root: display });
        Ok(id)
    }

    // --- git status (item 11: read-only status, no graph, no staging) --------

    /// Requests a `git status` for the current workspace root. A bounded,
    /// one-shot subprocess, so it runs as an ordinary `SubsystemId::GIT` task
    /// rather than needing a dedicated thread.
    pub fn request_git_status(&mut self) -> Result<TaskId, ls_scheduler::SubmitError> {
        let Some(root) = self.workspace.root().map(|c| c.as_path().to_path_buf()) else {
            return Err(ls_scheduler::SubmitError::ShuttingDown);
        };
        let spec = TaskSpec::new(
            SubsystemId::GIT,
            self.scheduler.base_priority(SubsystemId::GIT),
            ResourceClass::Process,
        )
        .with_workspace(WorkspaceRef(self.workspace.id().get()));

        let task = self.scheduler.submit(
            spec,
            Box::new(move |_cancellation| {
                let output = std::process::Command::new("git")
                    .args(["status", "--porcelain=v1", "-b"])
                    .current_dir(&root)
                    .output();
                match output {
                    Ok(output) if output.status.success() => {
                        let text = String::from_utf8_lossy(&output.stdout);
                        TaskOutcome::Completed(TaskProduct::new(git::parse_porcelain(&text)))
                    }
                    // Not a repository, git not installed, or the command
                    // failed: reported as "no status" rather than an error the
                    // user has to dismiss. A read-only status panel that
                    // cannot always produce a status is not a failure.
                    _ => TaskOutcome::Completed(TaskProduct::new(GitStatus::default())),
                }
            }),
        )?;
        self.pending_git_status = Some(task);
        Ok(task)
    }

    // --- diagnostics (item 9: LSP) --------------------------------------------

    /// Replaces the diagnostics for whichever open document has this path,
    /// if any is open. Diagnostics for a document that is not open (or has
    /// since been closed) are simply dropped -- there is nowhere to show
    /// them and no `RenderSnapshot` will ever be built for them.
    pub fn apply_diagnostics(&mut self, path: &Path, diagnostics: Vec<crate::render::Diagnostic>) {
        let Ok(canonical) = CanonicalPath::new(path) else { return };
        let Some(&id) = self.by_path.get(canonical.key()) else { return };
        if let Some(document) = self.documents.get_mut(&id) {
            document.set_diagnostics(diagnostics);
        }
    }

    pub fn git_status(&self) -> Option<&GitStatus> {
        self.git_status.as_ref()
    }

    pub fn is_git_status_pending(&self) -> bool {
        self.pending_git_status.is_some()
    }

    // --- workspace search (item 7: recursive text search) --------------------

    /// Requests a recursive search under the workspace root. A newer request
    /// cancels the one before it, so a fast typist searching incrementally
    /// never has two walks racing to publish a result.
    pub fn request_workspace_search(
        &mut self,
        query: String,
    ) -> Result<TaskId, ls_scheduler::SubmitError> {
        if let Some(previous) = self.pending_search.take() {
            self.scheduler.cancel(previous);
        }
        let Some(root) = self.workspace.root().map(|c| c.as_path().to_path_buf()) else {
            return Err(ls_scheduler::SubmitError::ShuttingDown);
        };
        let cancellation = CancellationToken::new();
        let spec = TaskSpec::new(
            SubsystemId::SEARCH,
            self.scheduler.base_priority(SubsystemId::SEARCH),
            ResourceClass::Cpu,
        )
        .with_cancellation(cancellation.clone())
        .with_workspace(WorkspaceRef(self.workspace.id().get()));

        let task = self.scheduler.submit(
            spec,
            Box::new(move |cancellation| {
                if cancellation.is_cancelled() {
                    return TaskOutcome::Cancelled;
                }
                let result = workspace_search::search(&root, &query);
                if cancellation.is_cancelled() {
                    TaskOutcome::Cancelled
                } else {
                    TaskOutcome::Completed(TaskProduct::new(result))
                }
            }),
        )?;
        self.pending_search = Some(task);
        Ok(task)
    }

    pub fn workspace_search_result(&self) -> Option<&WorkspaceSearchResult> {
        self.workspace_search_result.as_ref()
    }

    pub fn is_workspace_search_pending(&self) -> bool {
        self.pending_search.is_some()
    }

    // --- documents ------------------------------------------------------------

    /// Opens a file (specification section 24).
    ///
    /// Two references to the same file resolve to one document. This does not
    /// spawn processes, run Git, analyze, index or search.
    /// Requests a document, returning as soon as the work is admitted.
    ///
    /// The interactive part is bounded: canonicalize, look up identity,
    /// allocate an id, register a loading tab, submit. Reading, decoding and
    /// building the buffer happen on a scheduler worker (amendment section 7).
    ///
    /// # Path identity
    ///
    /// The path is canonicalized before anything else, so `src/main.rs`,
    /// `src/./main.rs` and `SRC/MAIN.RS` on a case-insensitive filesystem are
    /// one document. Identity covers loads that are still in flight: a request
    /// for a path that is already loading **joins** that task rather than
    /// starting a second read of the same file.
    pub fn request_open_document(&mut self, path: &Path) -> Result<OpenRequest, OpenDocumentError> {
        self.request_open_document_with(path, LoadInjection::NONE)
    }

    /// [`EditorCore::request_open_document`] with diagnostics injection, used
    /// by the development panel to make loading, failure and cancellation
    /// reachable on demand.
    pub fn request_open_document_with(
        &mut self,
        path: &Path,
        injection: LoadInjection,
    ) -> Result<OpenRequest, OpenDocumentError> {
        let canonical = CanonicalPath::new(path).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => OpenDocumentError::NotFound(path.to_path_buf()),
            std::io::ErrorKind::PermissionDenied => {
                OpenDocumentError::PermissionDenied(path.to_path_buf())
            }
            _ => OpenDocumentError::Io { path: path.to_path_buf(), source },
        })?;

        if canonical.as_path().is_dir() {
            return Err(OpenDocumentError::IsDirectory(canonical.into_path_buf()));
        }

        self.note_recent_file(canonical.as_path());

        // One document per file, however the path was spelled - including
        // while the file is still being read.
        if let Some(&existing) = self.by_path.get(canonical.key()) {
            return Ok(self.join_existing(existing));
        }

        let id = self.allocate_id();
        let cancellation = CancellationToken::new();
        let estimated_bytes =
            std::fs::metadata(canonical.as_path()).map(|meta| meta.len()).unwrap_or(0);

        let workspace = self.workspace.clone();
        let task_path = canonical.clone();
        let spec = TaskSpec::new(
            SubsystemId::DOCUMENT_IO,
            self.scheduler.base_priority(SubsystemId::DOCUMENT_IO),
            ResourceClass::Io,
        )
        .with_cost(CostEstimate::bytes(estimated_bytes))
        .with_cancellation(cancellation.clone())
        .with_workspace(WorkspaceRef(self.workspace.id().get()));

        let task = self
            .scheduler
            .submit(
                spec,
                Box::new(move |token| {
                    let result =
                        loading::load_from_disk(&workspace, id, &task_path, injection, token);
                    let bytes = match &result {
                        LoadResult::Loaded(data) => data.bytes_read,
                        _ => 0,
                    };
                    TaskOutcome::Completed(TaskProduct::new(result).with_io(bytes, 0))
                }),
            )
            .map_err(|error| OpenDocumentError::Rejected {
                path: canonical.as_path().to_path_buf(),
                reason: error.to_string(),
            })?;

        // Registration happens only after admission succeeded, so a rejected
        // request never leaves a tab behind.
        let display = canonical.display_string();
        self.by_path.insert(canonical.key().to_string(), id);
        self.loading.insert(
            id,
            PendingLoad {
                task,
                path: canonical,
                requested_at: Instant::now(),
                joins: 1,
                cancellation,
            },
        );
        self.order.push(id);
        self.active = Some(id);
        self.activity.started(LoadRecord {
            document: id,
            task,
            path: display.clone(),
            state: LoadState::Loading,
            joins: 1,
            total: None,
            queue_wait: None,
            wall_time: None,
            bytes: 0,
            error: None,
        });

        ls_log::debug!(
            SUBSYSTEM,
            "document_load_started",
            fields: [
                ls_log::Field::str("path", &display),
                ls_log::Field::uint("task", task.get()),
                ls_log::Field::uint("estimated_bytes", estimated_bytes),
            ],
            "document load submitted"
        );
        self.events.emit(
            SUBSYSTEM,
            EventPayload::DocumentLoadStarted { document: id, path: display, task: task.get() },
        );

        Ok(OpenRequest { document: id, task: Some(task), joined: false, already_open: false })
    }

    /// Attaches a request to a document that is already open or already loading.
    fn join_existing(&mut self, existing: DocumentId) -> OpenRequest {
        self.set_active(existing).ok();

        let Some(pending) = self.loading.get_mut(&existing) else {
            // Already fully loaded: nothing to wait for.
            return OpenRequest {
                document: existing,
                task: None,
                joined: true,
                already_open: true,
            };
        };

        pending.joins += 1;
        let joins = pending.joins;
        let task = pending.task;
        self.activity.update(existing, |record| record.joins = joins);
        ls_log::debug!(
            SUBSYSTEM,
            "document_load_joined",
            "request {joins} joined task {task} for an in-flight load"
        );
        self.events.emit(
            SUBSYSTEM,
            EventPayload::DocumentLoadJoined {
                document: existing,
                task: task.get(),
                requests: joins,
            },
        );
        OpenRequest { document: existing, task: Some(task), joined: true, already_open: false }
    }

    /// Applies finished background work to editor state.
    ///
    /// This is the **only** place a task result becomes a document, and it runs
    /// on the interactive thread (amendment section 3.6). Returns how many
    /// completions were applied.
    pub fn pump_completions(&mut self) -> usize {
        let completions = self.scheduler.drain_completions();
        let mut applied = 0;

        for completion in completions {
            if completion.subsystem == SubsystemId::GIT {
                applied += self.apply_git_completion(completion);
                continue;
            }
            if completion.subsystem == SubsystemId::SEARCH {
                applied += self.apply_search_completion(completion);
                continue;
            }
            if completion.subsystem == SubsystemId::WATCH {
                applied += self.apply_watch_completion(completion);
                continue;
            }
            if completion.subsystem != SubsystemId::DOCUMENT_IO {
                // Every subsystem that can complete work has a handler above
                // or here; anything else is a caller mistake worth seeing
                // rather than silently dropping.
                ls_log::warn!(
                    SUBSYSTEM,
                    "unexpected_completion",
                    "completion from subsystem {} has no handler",
                    completion.subsystem
                );
                continue;
            }

            let record = completion.record.clone();
            let task = completion.task;
            match completion.outcome {
                CompletionOutcome::Completed(product) => {
                    // One subsystem, two kinds of work: a load produces a
                    // document, a save produces a disk stamp.
                    match product.downcast::<LoadResult>() {
                        Ok(result) => {
                            self.apply_load_result(*result, &record);
                            applied += 1;
                        }
                        Err(value) => match value.downcast::<SaveOutcome>() {
                            Ok(outcome) => {
                                self.apply_save_outcome(*outcome, &record);
                                applied += 1;
                            }
                            Err(_) => ls_log::error!(
                                SUBSYSTEM,
                                "unexpected_product",
                                "document task {task} returned an unrecognized value"
                            ),
                        },
                    }
                }
                CompletionOutcome::Cancelled => {
                    if let Some(document) = self.document_for_task(task) {
                        self.settle_load(document, LoadState::Cancelled, None, &record);
                        applied += 1;
                    }
                }
                CompletionOutcome::Failed(failure) => {
                    if let Some(document) = self.document_for_task(task) {
                        let path = self
                            .loading
                            .get(&document)
                            .map(|pending| pending.path.as_path().to_path_buf())
                            .unwrap_or_default();
                        let error = OpenDocumentError::Io {
                            path,
                            source: std::io::Error::other(failure.to_string()),
                        };
                        self.settle_load(document, LoadState::Failed, Some(error), &record);
                        applied += 1;
                    }
                }
            }
        }

        // Loads, saves and closes can all change which directories matter
        // (a new path opened, a document closed, a save-as moved one), so
        // reconcile watches once per drain rather than at every call site.
        self.sync_watchers();

        applied
    }

    fn apply_git_completion(&mut self, completion: ls_scheduler::TaskCompletion) -> usize {
        if self.pending_git_status != Some(completion.task) {
            return 0;
        }
        self.pending_git_status = None;
        if let CompletionOutcome::Completed(product) = completion.outcome {
            if let Ok(status) = product.downcast::<GitStatus>() {
                self.git_status = Some(*status);
                return 1;
            }
        }
        0
    }

    fn apply_search_completion(&mut self, completion: ls_scheduler::TaskCompletion) -> usize {
        if self.pending_search != Some(completion.task) {
            return 0;
        }
        self.pending_search = None;
        if let CompletionOutcome::Completed(product) = completion.outcome {
            if let Ok(result) = product.downcast::<WorkspaceSearchResult>() {
                self.workspace_search_result = Some(*result);
                return 1;
            }
        }
        0
    }

    // --- filesystem watching (ADR-0017: replaces the shell's poll) -----------

    /// Applies a completed watch task: refreshes the documents it says
    /// changed, then re-arms the watch (unless the directory stopped being
    /// desired, in which case [`EditorCore::sync_watchers`] already cancelled
    /// it and this completion arrives as [`CompletionOutcome::Cancelled`]).
    fn apply_watch_completion(&mut self, completion: ls_scheduler::TaskCompletion) -> usize {
        let Some(key) = self
            .watching
            .iter()
            .find(|(_, &task)| task == completion.task)
            .map(|(key, _)| key.clone())
        else {
            return 0;
        };

        match completion.outcome {
            CompletionOutcome::Completed(product) => {
                let Ok(watched) = product.downcast::<WatchedPaths>() else {
                    self.watching.remove(&key);
                    return 0;
                };
                let applied = self.apply_watched_paths(&key, &watched);
                match self.submit_watch(watched.directory.clone(), watched.recursive) {
                    Ok(task) => {
                        self.watching.insert(key, task);
                    }
                    Err(error) => {
                        self.watching.remove(&key);
                        ls_log::warn!(
                            SUBSYSTEM,
                            "watch_not_rearmed",
                            "could not keep watching {}: {error}",
                            watched.directory.display()
                        );
                    }
                }
                applied
            }
            CompletionOutcome::Cancelled => 0,
            CompletionOutcome::Failed(failure) => {
                self.watching.remove(&key);
                ls_log::warn!(SUBSYSTEM, "watch_failed", "filesystem watch failed: {failure}");
                0
            }
        }
    }

    /// Refreshes every open document a completed watch says changed.
    ///
    /// An empty `changed` list means the OS notification buffer overflowed:
    /// every open document under the watched directory is re-checked instead
    /// of trusting emptiness to mean nothing happened.
    fn apply_watched_paths(&mut self, dir_key: &str, watched: &WatchedPaths) -> usize {
        let mut applied = 0;

        if watched.changed.is_empty() {
            let candidates: Vec<DocumentId> = self
                .documents
                .iter()
                .filter_map(|(id, document)| {
                    let path = document.path()?;
                    let under = if watched.recursive {
                        self.workspace.root().is_some_and(|root| path.relative_to(root).is_some())
                    } else {
                        path.parent().is_some_and(|parent| parent.key() == dir_key)
                    };
                    under.then_some(*id)
                })
                .collect();
            for id in candidates {
                if self.refresh_external_state(id).is_some() {
                    applied += 1;
                }
            }
            return applied;
        }

        for name in &watched.changed {
            let full = watched.directory.join(name);
            let Ok(canonical) = CanonicalPath::unverified(&full) else { continue };
            let Some(&id) = self.by_path.get(canonical.key()) else { continue };
            if self.refresh_external_state(id).is_some() {
                applied += 1;
            }
        }
        applied
    }

    /// The directories that should have an active watch right now: the
    /// workspace root, watched recursively, plus the parent directory of
    /// every open document that path does not already fall under (watched
    /// directly, since the recursive workspace watch already covers it).
    fn desired_watch_directories(&self) -> HashMap<String, (PathBuf, bool)> {
        let mut desired = HashMap::new();
        if let Some(root) = self.workspace.root() {
            desired.insert(root.key().to_string(), (root.as_path().to_path_buf(), true));
        }
        for document in self.documents.values() {
            let Some(path) = document.path() else { continue };
            if let Some(root) = self.workspace.root() {
                if path.relative_to(root).is_some() {
                    continue;
                }
            }
            let Some(parent) = path.parent() else { continue };
            desired
                .entry(parent.key().to_string())
                .or_insert_with(|| (parent.as_path().to_path_buf(), false));
        }
        desired
    }

    /// Reconciles active watches against [`EditorCore::desired_watch_directories`]:
    /// cancels ones no longer needed, starts ones newly needed. Idempotent, so
    /// callers do not need to reason about what changed -- only that something
    /// might have (specification section 25; ADR-0017).
    fn sync_watchers(&mut self) {
        let desired = self.desired_watch_directories();

        let stale: Vec<String> =
            self.watching.keys().filter(|key| !desired.contains_key(*key)).cloned().collect();
        for key in stale {
            if let Some(task) = self.watching.remove(&key) {
                self.scheduler.cancel(task);
            }
        }

        for (key, (directory, recursive)) in desired {
            if self.watching.contains_key(&key) {
                continue;
            }
            match self.submit_watch(directory.clone(), recursive) {
                Ok(task) => {
                    self.watching.insert(key, task);
                }
                Err(error) => {
                    ls_log::warn!(
                        SUBSYSTEM,
                        "watch_not_started",
                        "could not watch {}: {error}",
                        directory.display()
                    );
                }
            }
        }
    }

    /// Submits one bounded wait for a change under `directory` as a
    /// [`SubsystemId::WATCH`] task. The task body owns exactly one directory
    /// handle for its own lifetime -- there is no thread of its own
    /// (ADR-0017; architecture test `no_subsystem_creates_its_own_workers`).
    fn submit_watch(
        &mut self,
        directory: PathBuf,
        recursive: bool,
    ) -> Result<TaskId, ls_scheduler::SubmitError> {
        let spec = TaskSpec::new(
            SubsystemId::WATCH,
            self.scheduler.base_priority(SubsystemId::WATCH),
            ResourceClass::Io,
        )
        .with_workspace(WorkspaceRef(self.workspace.id().get()));

        self.scheduler.submit(
            spec,
            Box::new(move |cancellation| {
                let outcome = ls_platform::watch::wait_for_change(
                    &directory,
                    recursive,
                    WATCH_POLL_INTERVAL,
                    &|| cancellation.is_cancelled(),
                );
                match outcome {
                    Ok(ls_platform::watch::WatchOutcome::Changed(changed)) => {
                        TaskOutcome::Completed(TaskProduct::new(WatchedPaths {
                            directory,
                            recursive,
                            changed,
                        }))
                    }
                    Ok(ls_platform::watch::WatchOutcome::Cancelled) => TaskOutcome::Cancelled,
                    Err(error) => {
                        TaskOutcome::Failed(TaskFailure::new("watch.failed", error.to_string()))
                    }
                }
            }),
        )
    }

    fn document_for_task(&self, task: TaskId) -> Option<DocumentId> {
        self.loading.iter().find(|(_, pending)| pending.task == task).map(|(document, _)| *document)
    }

    fn apply_load_result(&mut self, result: LoadResult, record: &ls_scheduler::TaskRecord) {
        let document = result.document();
        match result {
            LoadResult::Loaded(data) => {
                let Some(pending) = self.loading.remove(&document) else {
                    // The tab was closed while the file was being read.
                    return;
                };
                let lines_and_bytes = self.install_loaded_document(document, *data);
                let (lines, bytes, display) = lines_and_bytes;
                let total = pending.requested_at.elapsed();

                self.activity.update(document, |entry| {
                    entry.state = LoadState::Loaded;
                    entry.total = Some(total);
                    entry.queue_wait = Some(record.queue_wait);
                    entry.wall_time = Some(record.wall_time);
                    entry.bytes = bytes;
                });

                ls_log::info!(
                    SUBSYSTEM,
                    "document_opened",
                    fields: [
                        ls_log::Field::str("path", &display),
                        ls_log::Field::uint("bytes", bytes),
                        ls_log::Field::uint("lines", lines as u64),
                        ls_log::Field::float("ms", total.as_secs_f64() * 1000.0),
                        ls_log::Field::uint("joined_requests", pending.joins as u64),
                    ],
                    "opened document"
                );
                self.events.emit(
                    SUBSYSTEM,
                    EventPayload::DocumentOpened { document, path: display, bytes, lines },
                );
            }
            LoadResult::Failed { document, error } => {
                self.settle_load(document, LoadState::Failed, Some(error), record);
            }
            LoadResult::Cancelled { document } => {
                self.settle_load(document, LoadState::Cancelled, None, record);
            }
        }
    }

    /// Builds the document from loaded data and puts it in the tab.
    fn install_loaded_document(
        &mut self,
        id: DocumentId,
        data: crate::loading::LoadedData,
    ) -> (usize, u64, String) {
        let display = data.path.display_string();
        let document = Document::from_buffer(
            id,
            data.path,
            data.buffer,
            data.encoding,
            data.line_ending,
            data.mixed_line_endings,
            data.stamp,
            self.config.document_settings(),
        );
        let lines = document.text().len_lines();
        self.documents.insert(id, document);
        (lines, data.bytes_read, display)
    }

    /// Removes a tab whose load did not produce a document.
    fn settle_load(
        &mut self,
        document: DocumentId,
        state: LoadState,
        error: Option<OpenDocumentError>,
        record: &ls_scheduler::TaskRecord,
    ) {
        let Some(pending) = self.loading.remove(&document) else { return };
        let total = pending.requested_at.elapsed();
        let message = error.as_ref().map(|error| error.to_string());

        self.by_path.remove(pending.path.key());
        if let Some(position) = self.order.iter().position(|open| *open == document) {
            self.order.remove(position);
            if self.active == Some(document) {
                self.active = self
                    .order
                    .get(position)
                    .or_else(|| self.order.get(position.saturating_sub(1)))
                    .copied();
            }
        }

        self.activity.update(document, |entry| {
            entry.state = state;
            entry.total = Some(total);
            entry.queue_wait = Some(record.queue_wait);
            entry.wall_time = Some(record.wall_time);
            entry.error = message.clone();
        });

        match state {
            LoadState::Cancelled => {
                ls_log::info!(
                    SUBSYSTEM,
                    "document_load_cancelled",
                    "load of {} cancelled after {:.1} ms",
                    pending.path.display_string(),
                    total.as_secs_f64() * 1000.0
                );
                self.events.emit(SUBSYSTEM, EventPayload::DocumentLoadCancelled { document });
            }
            _ => {
                if let Some(error) = error {
                    ls_log::diag::log_error(&error);
                    self.last_error = Some(error.to_string());
                    self.events.emit(
                        SUBSYSTEM,
                        EventPayload::DocumentLoadFailed { document, code: error.code() },
                    );
                    self.last_failed_load = Some((document, error));
                }
            }
        }
    }

    /// Cancels an in-flight load. The tab disappears when the cancellation
    /// completes, which the caller observes through `pump_completions`.
    pub fn cancel_open(&mut self, document: DocumentId) -> bool {
        let Some(pending) = self.loading.get(&document) else { return false };
        let task = pending.task;
        pending.cancellation.cancel();
        self.scheduler.cancel(task);
        true
    }

    /// Cancels every load in flight, for shutdown.
    pub fn cancel_all_loads(&mut self) -> usize {
        let documents: Vec<DocumentId> = self.loading.keys().copied().collect();
        documents.iter().filter(|id| self.cancel_open(**id)).count()
    }

    /// Whether this tab is still waiting for its file.
    pub fn is_loading(&self, document: DocumentId) -> bool {
        self.loading.contains_key(&document)
    }

    pub fn loading_count(&self) -> usize {
        self.loading.len()
    }

    /// The load in flight for a tab, if any.
    pub fn pending_load(&self, document: DocumentId) -> Option<&PendingLoad> {
        self.loading.get(&document)
    }

    /// Recent loads, newest first.
    pub fn load_activity(&self) -> &LoadActivity {
        &self.activity
    }

    /// The scheduler, for installing a completion waker and for diagnostics.
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// Opens a document and waits for it.
    ///
    /// This is the same path as [`EditorCore::request_open_document`] with a
    /// pump loop bolted on, kept for tests and benchmarks that want a value
    /// rather than an event. **The application does not use it**: blocking the
    /// interactive thread on file I/O is the thing Stage 1.1 exists to remove.
    pub fn open_document(&mut self, path: &Path) -> Result<DocumentId, OpenDocumentError> {
        let timer = self.metrics.open.timer();
        let request = self.request_open_document(path)?;
        if request.already_open {
            timer.cancel();
            return Ok(request.document);
        }

        let deadline = Instant::now() + Duration::from_secs(120);
        while self.is_loading(request.document) {
            if self.pump_completions() == 0 {
                if Instant::now() > deadline {
                    return Err(OpenDocumentError::Io {
                        path: path.to_path_buf(),
                        source: std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "the load did not finish",
                        ),
                    });
                }
                std::thread::yield_now();
            }
        }
        timer.stop();

        if self.documents.contains_key(&request.document) {
            return Ok(request.document);
        }
        match self.last_failed_load.take() {
            Some((document, error)) if document == request.document => Err(error),
            other => {
                self.last_failed_load = other;
                Err(OpenDocumentError::Io {
                    path: path.to_path_buf(),
                    source: std::io::Error::other("the load was cancelled"),
                })
            }
        }
    }

    /// Creates an empty untitled document.
    pub fn new_document(&mut self) -> DocumentId {
        self.untitled_counter += 1;
        let name = format!("Untitled-{}", self.untitled_counter);
        let id = self.allocate_id();
        let document = Document::untitled(id, name, self.config.document_settings());
        self.documents.insert(id, document);
        self.order.push(id);
        self.active = Some(id);
        id
    }

    /// Closes a tab. Unsaved content is the shell's problem to confirm first.
    /// Closes a document, refusing to discard unsaved edits.
    ///
    /// Losing a user's work is not something a Ctrl+W should be able to do by
    /// accident, so the caller has to acknowledge it explicitly through
    /// [`EditorCore::close_document_discarding_changes`].
    pub fn close_document(&mut self, id: DocumentId) -> Result<(), EditorError> {
        // A tab that is still loading has no unsaved work to protect: closing
        // it cancels the load, and the tab disappears when that lands.
        if self.loading.contains_key(&id) {
            self.cancel_open(id);
            return Ok(());
        }
        let dirty = self.documents.get(&id).map(|document| document.is_dirty());
        match dirty {
            None => return Err(EditorError::UnknownDocument(id)),
            Some(true) => return Err(EditorError::UnsavedChanges(id)),
            Some(false) => {}
        }
        self.close_document_discarding_changes(id)
    }

    /// Closes a document even if it has unsaved edits. The caller is stating
    /// that the user has been asked.
    pub fn close_document_discarding_changes(&mut self, id: DocumentId) -> Result<(), EditorError> {
        let document = self.documents.remove(&id).ok_or(EditorError::UnknownDocument(id))?;
        if let Some(path) = document.path() {
            self.by_path.remove(path.key());
        }
        self.published_revisions.remove(&id);
        let position = self.order.iter().position(|&open| open == id);
        if let Some(position) = position {
            self.order.remove(position);
            if self.active == Some(id) {
                self.active = self
                    .order
                    .get(position)
                    .or_else(|| self.order.get(position.saturating_sub(1)))
                    .copied();
            }
        }
        self.sync_watchers();
        self.events.emit(SUBSYSTEM, EventPayload::DocumentClosed { document: id });
        Ok(())
    }

    /// Moves `path` to the front of the recent list, de-duplicating and
    /// capping it. Pure in-memory bookkeeping; persisting it to disk is the
    /// shell's job, the same way persisting window position would be.
    fn note_recent_file(&mut self, path: &Path) {
        self.recent_files.retain(|existing| existing != path);
        self.recent_files.insert(0, path.to_path_buf());
        self.recent_files.truncate(MAX_RECENT_FILES);
    }

    /// Files opened recently, most-recent first.
    pub fn recent_files(&self) -> &[PathBuf] {
        &self.recent_files
    }

    /// Replaces the recent list wholesale, for seeding it from disk at
    /// startup.
    pub fn set_recent_files(&mut self, files: Vec<PathBuf>) {
        self.recent_files = files;
        self.recent_files.truncate(MAX_RECENT_FILES);
    }

    fn allocate_id(&mut self) -> DocumentId {
        let id = DocumentId::new(self.next_document_id);
        self.next_document_id += 1;
        id
    }

    pub fn document(&self, id: DocumentId) -> Option<&Document> {
        self.documents.get(&id)
    }

    pub fn document_mut(&mut self, id: DocumentId) -> Option<&mut Document> {
        self.documents.get_mut(&id)
    }

    pub fn active(&self) -> Option<DocumentId> {
        self.active
    }

    pub fn active_document(&self) -> Option<&Document> {
        self.active.and_then(|id| self.documents.get(&id))
    }

    pub fn active_document_mut(&mut self) -> Option<&mut Document> {
        match self.active {
            Some(id) => self.documents.get_mut(&id),
            None => None,
        }
    }

    /// Switches tabs. Documents stay loaded, so this is a pointer change rather
    /// than a reload (specification section 68).
    pub fn set_active(&mut self, id: DocumentId) -> Result<(), EditorError> {
        // A tab whose file is still loading is selectable: it exists, it is
        // just not a document yet.
        if !self.documents.contains_key(&id) && !self.loading.contains_key(&id) {
            return Err(EditorError::UnknownDocument(id));
        }
        let timer = self.metrics.tab_switch.timer();
        if self.active != Some(id) {
            self.active = Some(id);
            if let Some(document) = self.documents.get_mut(&id) {
                // The new tab has not been drawn yet, so its whole viewport is stale.
                document.invalidate_all();
            }
        }
        timer.stop();
        Ok(())
    }

    /// Moves `delta` tabs from the active one, wrapping around.
    pub fn cycle_tab(&mut self, delta: isize) {
        if self.order.is_empty() {
            return;
        }
        let current =
            self.active.and_then(|id| self.order.iter().position(|&open| open == id)).unwrap_or(0)
                as isize;
        let count = self.order.len() as isize;
        let next = ((current + delta) % count + count) % count;
        let id = self.order[next as usize];
        self.set_active(id).ok();
    }

    /// Activates the `number`th tab (1-based, matching Ctrl+1..9), doing
    /// nothing if there is no such tab.
    pub fn go_to_tab(&mut self, number: usize) {
        let Some(index) = number.checked_sub(1) else { return };
        if let Some(&id) = self.order.get(index) {
            self.set_active(id).ok();
        }
    }

    pub fn tabs(&self) -> &[DocumentId] {
        &self.order
    }

    /// Closes every tab that is not dirty (and cancels every one still
    /// loading). A dirty tab is left open rather than silently discarded --
    /// "close all" is not license to lose work a plain "close" would refuse.
    /// Returns `(closed, left_open)`.
    pub fn close_all_clean_tabs(&mut self) -> (usize, usize) {
        let mut closed = 0;
        let mut left_open = 0;
        for id in self.order.clone() {
            match self.close_document(id) {
                Ok(()) => closed += 1,
                Err(_) => left_open += 1,
            }
        }
        (closed, left_open)
    }

    pub fn tab_presentations(&self) -> Vec<TabPresentation> {
        self.order
            .iter()
            .filter_map(|&id| {
                let active = self.active == Some(id);
                if let Some(pending) = self.loading.get(&id) {
                    return Some(TabPresentation {
                        id,
                        title: pending.path.file_name(),
                        tooltip: Some(pending.path.display_string()),
                        dirty: false,
                        active,
                        loading: true,
                    });
                }
                let document = self.documents.get(&id)?;
                Some(TabPresentation {
                    id,
                    title: document.display_name().to_string(),
                    tooltip: document.path().map(|path| self.workspace.display_path(path)),
                    dirty: document.is_dirty(),
                    active,
                    loading: false,
                })
            })
            .collect()
    }

    // --- the section 30 editing contract -------------------------------------

    pub fn insert(
        &mut self,
        document: DocumentId,
        position: Position,
        text: &str,
    ) -> Result<EditResult, EditorError> {
        let timer = self.metrics.edit.timer();
        let document_ref =
            self.documents.get_mut(&document).ok_or(EditorError::UnknownDocument(document))?;
        let at = document_ref.text().position_at(position.line, position.column);
        let edit = Edit::insert(at, text);
        // A programmatic edit somewhere else in the document must not yank the
        // caret along with it: the selection is mapped through the edit so it
        // still points at the same text (specification section 15, and the
        // brief's cursor-stability example).
        let selection = transform_selection(document_ref.selections().primary(), &edit);
        let result = document_ref.apply_edit(
            edit,
            EditKind::Programmatic,
            crate::selection::SelectionSet::new(selection),
        );
        timer.stop();
        self.events
            .emit(SUBSYSTEM, EventPayload::DocumentEdited { document, revision: result.revision });
        Ok(result)
    }

    pub fn delete(
        &mut self,
        document: DocumentId,
        range: TextRange,
    ) -> Result<EditResult, EditorError> {
        let timer = self.metrics.edit.timer();
        let document_ref =
            self.documents.get_mut(&document).ok_or(EditorError::UnknownDocument(document))?;
        let buffer = document_ref.text();
        let start = buffer.position_at(range.start.line, range.start.column);
        let end = buffer.position_at(range.end.line, range.end.column);
        if end < start {
            return Err(EditorError::InvalidRange { start: start.get(), end: end.get() });
        }
        let removed = buffer.slice(start..end);
        let edit = Edit::delete(start, removed);
        let selection = transform_selection(document_ref.selections().primary(), &edit);
        let result = document_ref.apply_edit(
            edit,
            EditKind::Programmatic,
            crate::selection::SelectionSet::new(selection),
        );
        timer.stop();
        self.events
            .emit(SUBSYSTEM, EventPayload::DocumentEdited { document, revision: result.revision });
        Ok(result)
    }

    pub fn undo(&mut self, document: DocumentId) -> Result<EditResult, EditorError> {
        let timer = self.metrics.undo_redo.timer();
        let document_ref =
            self.documents.get_mut(&document).ok_or(EditorError::UnknownDocument(document))?;
        let result = document_ref.undo();
        timer.stop();
        match result {
            Some(result) => {
                self.events.emit(
                    SUBSYSTEM,
                    EventPayload::DocumentEdited { document, revision: result.revision },
                );
                Ok(result)
            }
            // Nothing to undo is not a failure; report the current revision.
            None => Ok(EditResult {
                revision: self.documents[&document].revision(),
                invalidation: Default::default(),
            }),
        }
    }

    pub fn redo(&mut self, document: DocumentId) -> Result<EditResult, EditorError> {
        let timer = self.metrics.undo_redo.timer();
        let document_ref =
            self.documents.get_mut(&document).ok_or(EditorError::UnknownDocument(document))?;
        let result = document_ref.redo();
        timer.stop();
        match result {
            Some(result) => {
                self.events.emit(
                    SUBSYSTEM,
                    EventPayload::DocumentEdited { document, revision: result.revision },
                );
                Ok(result)
            }
            None => Ok(EditResult {
                revision: self.documents[&document].revision(),
                invalidation: Default::default(),
            }),
        }
    }

    // --- interactive operations ----------------------------------------------

    /// Types text at the caret, as a keystroke would.
    pub fn type_text(&mut self, text: &str) {
        let timer = self.metrics.edit.timer();
        let Some(id) = self.active else { return };
        let Some(document) = self.documents.get_mut(&id) else { return };
        let result = document.insert(text, EditKind::Typing);
        timer.stop();
        self.events.emit(
            SUBSYSTEM,
            EventPayload::DocumentEdited { document: id, revision: result.revision },
        );
    }

    /// Inserts one indentation step.
    pub fn insert_tab(&mut self) {
        let settings = self.config.document_settings();
        if settings.insert_spaces {
            let spaces = " ".repeat(settings.tab_width);
            self.type_text(&spaces);
        } else {
            self.type_text("\t");
        }
    }

    /// Removes one indent step from the lines the selection touches --
    /// Shift+Tab.
    pub fn dedent(&mut self) {
        let timer = self.metrics.edit.timer();
        let Some(id) = self.active else { return };
        let Some(document) = self.documents.get_mut(&id) else { return };
        let result = document.dedent();
        timer.stop();
        if let Some(result) = result {
            self.events.emit(
                SUBSYSTEM,
                EventPayload::DocumentEdited { document: id, revision: result.revision },
            );
        }
    }

    pub fn delete_backward(&mut self) {
        let timer = self.metrics.edit.timer();
        let Some(id) = self.active else { return };
        let Some(document) = self.documents.get_mut(&id) else { return };
        let result = document.backspace();
        timer.stop();
        if let Some(result) = result {
            self.events.emit(
                SUBSYSTEM,
                EventPayload::DocumentEdited { document: id, revision: result.revision },
            );
        }
    }

    pub fn delete_forward(&mut self) {
        let timer = self.metrics.edit.timer();
        let Some(id) = self.active else { return };
        let Some(document) = self.documents.get_mut(&id) else { return };
        let result = document.delete_forward();
        timer.stop();
        if let Some(result) = result {
            self.events.emit(
                SUBSYSTEM,
                EventPayload::DocumentEdited { document: id, revision: result.revision },
            );
        }
    }

    pub fn delete_word_backward(&mut self) {
        let Some(id) = self.active else { return };
        let Some(document) = self.documents.get_mut(&id) else { return };
        if let Some(result) = document.delete_word_left() {
            self.events.emit(
                SUBSYSTEM,
                EventPayload::DocumentEdited { document: id, revision: result.revision },
            );
        }
    }

    pub fn delete_word_forward(&mut self) {
        let Some(id) = self.active else { return };
        let Some(document) = self.documents.get_mut(&id) else { return };
        if let Some(result) = document.delete_word_right() {
            self.events.emit(
                SUBSYSTEM,
                EventPayload::DocumentEdited { document: id, revision: result.revision },
            );
        }
    }

    pub fn undo_active(&mut self) {
        if let Some(id) = self.active {
            let _ = self.undo(id);
        }
    }

    pub fn redo_active(&mut self) {
        if let Some(id) = self.active {
            let _ = self.redo(id);
        }
    }

    /// Moves the caret, optionally extending the selection.
    pub fn move_cursor(&mut self, movement: Movement, extend: bool) -> Result<(), EditorError> {
        let metric =
            if extend { self.metrics.selection.clone() } else { self.metrics.cursor.clone() };
        let timer = metric.timer();
        let page_lines = self.page_lines;
        let Some(id) = self.active else { return Ok(()) };
        let Some(document) = self.documents.get_mut(&id) else { return Ok(()) };

        document.move_cursor(movement, extend, page_lines);
        let selection = document.selections().primary();
        let line = document.text().char_to_line(selection.head);
        let column = selection.head - document.text().line_range(line).start;
        let selected = selection.len_chars();
        timer.stop();

        self.events.emit(
            SUBSYSTEM,
            EventPayload::CursorChanged { document: id, line: line.get(), column },
        );
        if selected > 0 {
            self.events
                .emit(SUBSYSTEM, EventPayload::SelectionChanged { document: id, chars: selected });
        }
        Ok(())
    }

    /// Places the caret at an offset, as a mouse click does.
    pub fn set_selection(&mut self, selection: Selection) {
        let Some(id) = self.active else { return };
        let Some(document) = self.documents.get_mut(&id) else { return };
        document.set_selection(selection);
    }

    pub fn select_all(&mut self) {
        if let Some(document) = self.active_document_mut() {
            document.select_all();
        }
    }

    /// Go to line/column, as `Ctrl+G` and future diagnostics do.
    pub fn go_to(&mut self, line: usize, column: usize) {
        let Some(id) = self.active else { return };
        let Some(document) = self.documents.get_mut(&id) else { return };
        document.move_to(LineIndex::new(line), column, false);
    }

    // --- find (in-document search) --------------------------------------------

    /// The active document's find state, if there is one to search.
    pub fn find_state(&self) -> Option<&crate::search::FindState> {
        self.active_document().map(|document| document.find())
    }

    pub fn is_find_open(&self) -> bool {
        self.find_open
    }

    pub fn open_find(&mut self) {
        self.find_open = true;
    }

    /// Sets the query and jumps to the nearest match, reusing the existing
    /// selection highlight for "here is the one you're on" (specification
    /// section 12: one mechanism per concern, not a parallel one for find).
    pub fn set_find_query(&mut self, query: String) {
        let Some(document) = self.active_document_mut() else { return };
        document.set_find_query(query);
        document.select_current_find_match();
    }

    pub fn find_next(&mut self) {
        let Some(document) = self.active_document_mut() else { return };
        document.advance_find(1);
        document.select_current_find_match();
    }

    pub fn find_previous(&mut self) {
        let Some(document) = self.active_document_mut() else { return };
        document.advance_find(-1);
        document.select_current_find_match();
    }

    /// Ends the search, clearing its highlights. The selection it left behind
    /// is left alone -- closing find should not also discard where the user
    /// ended up.
    pub fn close_find(&mut self) {
        self.find_open = false;
        if let Some(document) = self.active_document_mut() {
            document.clear_find();
        }
    }

    // --- clipboard ------------------------------------------------------------

    pub fn copy(&mut self) -> Result<(), EditorError> {
        let Some(document) = self.active_document() else { return Ok(()) };
        let text = document.selected_text();
        if text.is_empty() {
            return Ok(());
        }
        self.clipboard.write_text(&text).map_err(EditorError::Clipboard)
    }

    pub fn cut(&mut self) -> Result<(), EditorError> {
        let Some(id) = self.active else { return Ok(()) };
        let text = match self.documents.get(&id) {
            Some(document) => document.selected_text(),
            None => return Ok(()),
        };
        if text.is_empty() {
            return Ok(());
        }
        self.clipboard.write_text(&text).map_err(EditorError::Clipboard)?;
        if let Some(document) = self.documents.get_mut(&id) {
            let result = document.delete_selection();
            self.events.emit(
                SUBSYSTEM,
                EventPayload::DocumentEdited { document: id, revision: result.revision },
            );
        }
        Ok(())
    }

    pub fn paste_from_clipboard(&mut self) -> Result<(), EditorError> {
        let text = self.clipboard.read_text().map_err(EditorError::Clipboard)?;
        if !text.is_empty() {
            self.paste_text(&text);
        }
        Ok(())
    }

    /// Pastes text that came from outside the editor, normalizing its line
    /// endings to the buffer's internal representation.
    pub fn paste_text(&mut self, text: &str) {
        let normalized = line_ending::normalize(text);
        let Some(id) = self.active else { return };
        let Some(document) = self.documents.get_mut(&id) else { return };
        let result = document.insert(&normalized, EditKind::Paste);
        self.events.emit(
            SUBSYSTEM,
            EventPayload::DocumentEdited { document: id, revision: result.revision },
        );
    }

    // --- persistence ----------------------------------------------------------

    /// Saves a document to its own path.
    /// Requests a save, returning as soon as the work is admitted.
    ///
    /// The interactive thread does only bounded work: validate, capture the
    /// revision and transaction token, clone the rope (`O(1)`), submit. The
    /// encode, write, flush, fsync and atomic replace happen on a worker
    /// (amendment section 9).
    pub fn request_save(&mut self, id: DocumentId) -> Result<SaveRequestOutcome, PersistenceError> {
        let path = match self.documents.get(&id) {
            Some(document) => document.path().cloned(),
            None => return Err(PersistenceError::NoPath),
        };
        let Some(path) = path else { return Err(PersistenceError::NoPath) };
        self.request_save_to(id, path)
    }

    /// Requests a save to a new path and adopts it when the save lands.
    pub fn request_save_as(
        &mut self,
        id: DocumentId,
        path: PathBuf,
    ) -> Result<SaveRequestOutcome, PersistenceError> {
        let canonical = CanonicalPath::unverified(&path).map_err(|source| {
            PersistenceError::Platform(ls_platform::PlatformError::io(
                "persistence.invalid_path",
                format!("invalid save path {}", path.display()),
                source,
            ))
        })?;
        self.request_save_to(id, canonical)
    }

    fn request_save_to(
        &mut self,
        id: DocumentId,
        path: CanonicalPath,
    ) -> Result<SaveRequestOutcome, PersistenceError> {
        let snapshot = self.capture_save_snapshot(id, path)?;
        let revision = snapshot.revision;

        // One writer per document. A second request waits; a third replaces
        // the one waiting, because its content is newer.
        if self.saving.contains_key(&id) {
            let superseded = self.queued_saves.insert(id, snapshot).is_some();
            let disposition = if superseded {
                SaveDisposition::SupersededQueued
            } else {
                SaveDisposition::Queued
            };
            ls_log::debug!(
                SUBSYSTEM,
                "save_queued",
                "a save is already running for {id}; this one waits ({disposition:?})"
            );
            return Ok(SaveRequestOutcome { document: id, task: None, revision, disposition });
        }

        let task = self.submit_save(snapshot)?;
        Ok(SaveRequestOutcome {
            document: id,
            task: Some(task),
            revision,
            disposition: SaveDisposition::Started,
        })
    }

    /// Captures the immutable snapshot that crosses the scheduler boundary.
    ///
    /// The rope clone is `O(1)`: the buffer is copy-on-write, so this shares
    /// structure with the live document instead of copying it.
    fn capture_save_snapshot(
        &mut self,
        id: DocumentId,
        path: CanonicalPath,
    ) -> Result<SaveSnapshot, PersistenceError> {
        let document = self.documents.get_mut(&id).ok_or(PersistenceError::NoPath)?;
        // The user's Save press closes the edit group; this is the only history
        // mutation on the whole save path, and it happens here, not at
        // completion.
        document.mark_saving();
        Ok(SaveSnapshot {
            document: id,
            revision: document.revision(),
            token: document.transaction_token(),
            path,
            encoding: document.encoding(),
            line_ending: document.line_ending(),
            buffer: document.text().snapshot(),
        })
    }

    fn submit_save(&mut self, snapshot: SaveSnapshot) -> Result<TaskId, PersistenceError> {
        let id = snapshot.document;
        let display = snapshot.path.display_string();
        let revision = snapshot.revision;
        let token = snapshot.token;
        let path = snapshot.path.clone();
        let estimated = snapshot.len_bytes() as u64;
        let workspace = self.workspace.clone();

        let spec = TaskSpec::new(
            SubsystemId::DOCUMENT_IO,
            self.scheduler.base_priority(SubsystemId::DOCUMENT_IO),
            ResourceClass::Io,
        )
        .with_cost(CostEstimate::bytes(estimated))
        .with_workspace(WorkspaceRef(self.workspace.id().get()));

        let task = self
            .scheduler
            .submit(
                spec,
                Box::new(move |_token| {
                    // Saving is not cancellable: a half-written document is
                    // worse than a slow one, and the atomic replace is the
                    // only point where anything becomes visible.
                    let outcome = persistence::write_snapshot(&workspace, &snapshot);
                    let written = outcome.bytes_written;
                    TaskOutcome::Completed(TaskProduct::new(outcome).with_io(0, written))
                }),
            )
            .map_err(|error| {
                PersistenceError::Platform(ls_platform::PlatformError::new(
                    "persistence.rejected",
                    format!("cannot save right now: {error}"),
                    ls_log::diag::Recoverability::Retryable,
                ))
            })?;

        self.saving
            .insert(id, PendingSave { task, revision, token, path, requested_at: Instant::now() });
        self.save_activity.started(SaveRecord {
            document: id,
            task,
            path: display.clone(),
            revision,
            succeeded: None,
            stale: false,
            bytes_written: 0,
            total: None,
            queue_wait: None,
            wall_time: None,
            error: None,
        });

        ls_log::debug!(
            SUBSYSTEM,
            "document_save_started",
            fields: [
                ls_log::Field::str("path", &display),
                ls_log::Field::uint("task", task.get()),
                ls_log::Field::uint("revision", revision.get()),
            ],
            "document save submitted"
        );
        self.events.emit(
            SUBSYSTEM,
            EventPayload::DocumentSaveStarted { document: id, task: task.get(), revision },
        );
        Ok(task)
    }

    /// Applies a finished save. Interactive thread only.
    fn apply_save_outcome(&mut self, outcome: SaveOutcome, record: &ls_scheduler::TaskRecord) {
        let id = outcome.document;
        let Some(pending) = self.saving.remove(&id) else { return };
        let total = pending.requested_at.elapsed();
        let task = pending.task;
        // The save metric measures request to completion, which is what a user
        // waits for even though the work happened elsewhere.
        self.metrics.save.record(total);

        match outcome.result {
            Ok(stamp) => {
                let display = outcome.path.display_string();
                let key = outcome.path.key().to_string();
                let stale = self
                    .documents
                    .get(&id)
                    .map(|document| document.transaction_token() != outcome.token)
                    .unwrap_or(false);

                if let Some(document) = self.documents.get_mut(&id) {
                    // The captured token decides clean/dirty; the revision is
                    // the identity of what was written (amendment section 8.1).
                    document.mark_saved_at(outcome.path, stamp, outcome.token);
                }
                // Save As adopts the new path only once the file exists.
                self.by_path.retain(|_, &mut open| open != id);
                self.by_path.insert(key, id);

                self.save_activity.update(task, |entry| {
                    entry.succeeded = Some(true);
                    entry.stale = stale;
                    entry.bytes_written = outcome.bytes_written;
                    entry.total = Some(total);
                    entry.queue_wait = Some(record.queue_wait);
                    entry.wall_time = Some(record.wall_time);
                });

                ls_log::info!(
                    SUBSYSTEM,
                    "document_saved",
                    fields: [
                        ls_log::Field::str("path", &display),
                        ls_log::Field::uint("revision", outcome.revision.get()),
                        ls_log::Field::bool("stale", stale),
                        ls_log::Field::float("ms", total.as_secs_f64() * 1000.0),
                    ],
                    "saved document"
                );
                self.events.emit(
                    SUBSYSTEM,
                    EventPayload::DocumentSaved {
                        document: id,
                        path: display,
                        revision: outcome.revision,
                    },
                );
            }
            Err(error) => {
                if let Some(document) = self.documents.get_mut(&id) {
                    document.mark_save_failed();
                }
                let message = error.to_string();
                self.save_activity.update(task, |entry| {
                    entry.succeeded = Some(false);
                    entry.total = Some(total);
                    entry.queue_wait = Some(record.queue_wait);
                    entry.wall_time = Some(record.wall_time);
                    entry.error = Some(message.clone());
                });
                ls_log::diag::log_error(&error);
                self.last_error = Some(message);
                self.events.emit(
                    SUBSYSTEM,
                    EventPayload::DocumentSaveFailed { document: id, code: error.code() },
                );
                self.last_failed_save = Some((id, error));
            }
        }

        // A save that was waiting behind this one starts now.
        if let Some(queued) = self.queued_saves.remove(&id) {
            if let Err(error) = self.submit_save(queued) {
                ls_log::diag::log_error(&error);
                self.last_error = Some(error.to_string());
            }
        }
    }

    /// Whether a save is in flight for this document.
    pub fn is_saving(&self, document: DocumentId) -> bool {
        self.saving.contains_key(&document)
    }

    pub fn saving_count(&self) -> usize {
        self.saving.len()
    }

    /// Whether a newer save is waiting behind the one in flight.
    pub fn has_queued_save(&self, document: DocumentId) -> bool {
        self.queued_saves.contains_key(&document)
    }

    pub fn pending_save(&self, document: DocumentId) -> Option<&PendingSave> {
        self.saving.get(&document)
    }

    /// Recent saves, newest first.
    pub fn save_activity(&self) -> &SaveActivity {
        &self.save_activity
    }

    /// Saves a document and waits for it.
    ///
    /// The same path as [`EditorCore::request_save`] with a pump loop, kept for
    /// tests and benchmarks. **The application does not use it.**
    pub fn save(&mut self, id: DocumentId) -> Result<(), PersistenceError> {
        let outcome = self.request_save(id)?;
        self.await_save(outcome.document)
    }

    /// Save As, then wait. Tests and benchmarks only.
    pub fn save_as(&mut self, id: DocumentId, path: PathBuf) -> Result<(), PersistenceError> {
        let outcome = self.request_save_as(id, path)?;
        self.await_save(outcome.document)
    }

    fn await_save(&mut self, id: DocumentId) -> Result<(), PersistenceError> {
        let deadline = Instant::now() + Duration::from_secs(120);
        while self.is_saving(id) || self.has_queued_save(id) {
            if self.pump_completions() == 0 {
                if Instant::now() > deadline {
                    return Err(PersistenceError::Platform(ls_platform::PlatformError::new(
                        "persistence.timeout",
                        "the save did not finish",
                        ls_log::diag::Recoverability::Retryable,
                    )));
                }
                std::thread::yield_now();
            }
        }
        match self.last_failed_save.take() {
            Some((document, error)) if document == id => Err(error),
            other => {
                self.last_failed_save = other;
                Ok(())
            }
        }
    }

    /// Saves the active document, asking the shell for a path if it has none.
    pub fn save_active(&mut self) {
        let Some(id) = self.active else { return };
        let has_path = self.documents.get(&id).and_then(|d| d.path()).is_some();
        if !has_path {
            self.request_shell(ShellRequest::SaveAsDialog);
            return;
        }
        if let Err(error) = self.request_save(id) {
            self.last_error = Some(error.to_string());
        }
    }

    pub fn save_active_as(&mut self, path: PathBuf) {
        let Some(id) = self.active else { return };
        if let Err(error) = self.request_save_as(id, path) {
            self.last_error = Some(error.to_string());
        }
    }

    /// Compares a document against the file on disk.
    ///
    /// Called from [`EditorCore::apply_watched_paths`] whenever a filesystem
    /// watch task reports a change (ADR-0017). Stays public and keeps taking a
    /// `DocumentId` and returning a state because tests exercise it directly,
    /// which is exactly the shape an event-driven caller needs too
    /// (specification section 25).
    pub fn refresh_external_state(&mut self, id: DocumentId) -> Option<ExternalState> {
        let path = self.documents.get(&id)?.path()?.as_path().to_path_buf();
        let stamp = self.workspace.stamp(&path).ok();
        let document = self.documents.get_mut(&id)?;
        let state = match (stamp, document.disk_stamp()) {
            (None, _) => ExternalState::Missing,
            (Some(current), Some(known)) if &current == known => ExternalState::Unchanged,
            (Some(_), _) if document.is_dirty() => ExternalState::Conflict,
            (Some(_), _) => ExternalState::ExternallyChanged,
        };
        document.set_external_state(state);
        Some(state)
    }

    // --- dispatch, snapshots, events -----------------------------------------

    /// Runs a command by id (specification section 12).
    pub fn execute(&mut self, command_id: &str, args: CommandArgs) -> Result<(), EditorError> {
        let command = commands::find(command_id)
            .ok_or_else(|| EditorError::UnknownCommand(command_id.to_string()))?;
        if !(command.enabled)(self) {
            return Err(EditorError::CommandNotEnabled(command.id));
        }
        (command.execute)(self, args)
    }

    /// Whether a command is currently applicable, for menus and toolbars.
    pub fn is_command_enabled(&self, command_id: &str) -> bool {
        commands::find(command_id).is_some_and(|command| (command.enabled)(self))
    }

    /// Builds the immutable snapshot for one frame (specification section 26).
    pub fn render_snapshot(
        &mut self,
        id: DocumentId,
        viewport: Viewport,
    ) -> Option<Arc<RenderSnapshot>> {
        let document = self.documents.get_mut(&id)?;
        let snapshot = Arc::new(render::build_snapshot(document, viewport));
        let previous = self.published_revisions.insert(id, snapshot.content_revision);
        if previous != Some(snapshot.content_revision) {
            self.events.emit(
                SUBSYSTEM,
                EventPayload::RenderSnapshotPublished {
                    document: id,
                    revision: snapshot.content_revision,
                },
            );
        }
        Some(snapshot)
    }

    /// Tells the core how tall the viewport is, so PageUp/PageDown match it.
    pub fn set_page_lines(&mut self, lines: usize) {
        self.page_lines = lines.max(1);
    }

    pub fn page_lines(&self) -> usize {
        self.page_lines
    }

    pub fn drain_events(&mut self) -> Vec<Event> {
        self.events.drain()
    }

    pub fn dropped_events(&self) -> u64 {
        self.events.dropped()
    }

    pub fn request_shell(&mut self, request: ShellRequest) {
        self.shell_requests.push(request);
    }

    pub fn take_shell_requests(&mut self) -> Vec<ShellRequest> {
        std::mem::take(&mut self.shell_requests)
    }

    /// Most recent user-facing failure, for the status bar.
    pub fn take_last_error(&mut self) -> Option<String> {
        self.last_error.take()
    }

    pub fn report_open_failure(&mut self, error: OpenDocumentError) {
        ls_log::diag::log_error(&error);
        self.last_error = Some(error.to_string());
    }

    /// Emits an event for every performance contract currently being missed
    /// (specification sections 37, 48).
    pub fn check_performance_budgets(&mut self) {
        let snapshot = ls_perf::snapshot();
        for metric in snapshot.failing() {
            let Some(budget) = metric.budget else { continue };
            self.events.emit(
                SUBSYSTEM,
                EventPayload::PerformanceBudgetExceeded {
                    metric: metric.name,
                    p95_micros: metric.stats.p95.as_micros() as u64,
                    threshold_micros: budget.failure_p95.as_micros() as u64,
                },
            );
        }
    }

    /// Character offset for a line/column pair in a document.
    pub fn offset_of(&self, id: DocumentId, position: Position) -> Option<CharOffset> {
        let document = self.documents.get(&id)?;
        Some(document.text().position_at(position.line, position.column))
    }
}

impl std::fmt::Debug for EditorCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorCore")
            .field("documents", &self.order.len())
            .field("active", &self.active)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::ContentState;
    use ls_platform::MemoryClipboard;

    fn editor() -> EditorCore {
        EditorCore::with_clipboard(EffectiveConfig::default(), Box::new(MemoryClipboard::new()))
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lightspeed-editor-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn text_of(editor: &EditorCore, id: DocumentId) -> String {
        editor.document(id).unwrap().text().to_string()
    }

    #[test]
    fn a_new_document_becomes_the_active_tab() {
        let mut editor = editor();
        let first = editor.new_document();
        let second = editor.new_document();
        assert_eq!(editor.active(), Some(second));
        assert_eq!(editor.tabs(), &[first, second]);
        assert_eq!(editor.tab_presentations()[1].title, "Untitled-2");
    }

    #[test]
    fn opening_the_same_file_twice_returns_one_document() {
        let dir = scratch("identity");
        let path = dir.join("a.txt");
        std::fs::write(&path, b"hello").unwrap();

        let mut editor = editor();
        let first = editor.open_document(&path).unwrap();
        let indirect = dir.join(".").join("a.txt");
        let second = editor.open_document(&indirect).unwrap();

        assert_eq!(first, second, "specification section 24: one document per file");
        assert_eq!(editor.tabs().len(), 1);
    }

    #[test]
    fn opening_a_file_decodes_and_normalizes_it() {
        let dir = scratch("open");
        let path = dir.join("crlf.txt");
        std::fs::write(&path, b"one\r\ntwo\r\n").unwrap();

        let mut editor = editor();
        let id = editor.open_document(&path).unwrap();
        let document = editor.document(id).unwrap();

        assert_eq!(document.text().to_string(), "one\ntwo\n", "stored with LF internally");
        assert_eq!(document.line_ending(), ls_buffer::LineEnding::CrLf, "style is remembered");
        assert_eq!(document.content_state(), ContentState::Clean);
        assert_eq!(document.revision().get(), 0, "a freshly opened document is at revision 0");
    }

    #[test]
    fn opening_a_binary_file_is_refused() {
        let dir = scratch("binary");
        let path = dir.join("image.png");
        std::fs::write(&path, [0x89, b'P', b'N', b'G', 0x00, 0x1A]).unwrap();

        let mut editor = editor();
        let error = editor.open_document(&path).unwrap_err();
        assert!(matches!(error, OpenDocumentError::Binary { .. }));
        assert!(editor.tabs().is_empty(), "nothing is opened");
    }

    #[test]
    fn opening_a_missing_file_is_typed() {
        let mut editor = editor();
        let error = editor.open_document(Path::new("no-such-file-here.txt")).unwrap_err();
        assert!(matches!(error, OpenDocumentError::NotFound(_)));
    }

    #[test]
    fn opening_a_directory_is_refused() {
        let dir = scratch("dir");
        let mut editor = editor();
        let error = editor.open_document(&dir).unwrap_err();
        assert!(matches!(error, OpenDocumentError::IsDirectory(_)));
    }

    #[test]
    fn saving_writes_the_original_line_endings_back() {
        let dir = scratch("save-crlf");
        let path = dir.join("crlf.txt");
        std::fs::write(&path, b"one\r\ntwo\r\n").unwrap();

        let mut editor = editor();
        let id = editor.open_document(&path).unwrap();
        editor.go_to(0, 3);
        editor.type_text("!");
        editor.save(id).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"one!\r\ntwo\r\n");
        assert!(!editor.document(id).unwrap().is_dirty());
    }

    #[test]
    fn save_as_adopts_the_new_path() {
        let dir = scratch("save-as");
        let path = dir.join("new.txt");

        let mut editor = editor();
        let id = editor.new_document();
        editor.type_text("fresh content");
        editor.save_as(id, path.clone()).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh content");
        let document = editor.document(id).unwrap();
        assert_eq!(document.display_name(), "new.txt");
        assert!(!document.is_dirty());
        // The new path is now the document's identity.
        let reopened = editor.open_document(&path).unwrap();
        assert_eq!(reopened, id);
    }

    #[test]
    fn saving_an_untitled_document_asks_the_shell_for_a_path() {
        let mut editor = editor();
        editor.new_document();
        editor.type_text("x");
        editor.execute("file.save", CommandArgs::None).unwrap();
        assert_eq!(editor.take_shell_requests(), vec![ShellRequest::SaveAsDialog]);
    }

    #[test]
    fn the_section_30_contract_edits_by_position() {
        let mut editor = editor();
        let id = editor.new_document();
        editor.insert(id, Position::new(0, 0), "hello\nworld").unwrap();
        assert_eq!(text_of(&editor, id), "hello\nworld");

        editor
            .delete(id, TextRange { start: Position::new(1, 0), end: Position::new(1, 5) })
            .unwrap();
        assert_eq!(text_of(&editor, id), "hello\n");

        editor.undo(id).unwrap();
        assert_eq!(text_of(&editor, id), "hello\nworld");
        editor.redo(id).unwrap();
        assert_eq!(text_of(&editor, id), "hello\n");
    }

    #[test]
    fn editing_an_unknown_document_is_an_error() {
        let mut editor = editor();
        let missing = DocumentId::new(999);
        assert!(matches!(
            editor.insert(missing, Position::new(0, 0), "x"),
            Err(EditorError::UnknownDocument(_))
        ));
        assert!(matches!(editor.undo(missing), Err(EditorError::UnknownDocument(_))));
    }

    #[test]
    fn undo_with_an_empty_history_reports_the_current_revision() {
        let mut editor = editor();
        let id = editor.new_document();
        let result = editor.undo(id).unwrap();
        assert_eq!(result.revision.get(), 0);
    }

    #[test]
    fn clipboard_round_trip_through_commands() {
        let mut editor = editor();
        let id = editor.new_document();
        editor.type_text("copy me");
        editor.select_all();
        editor.execute("edit.copy", CommandArgs::None).unwrap();

        editor.execute("cursor.document_end", CommandArgs::None).unwrap();
        editor.execute("edit.paste", CommandArgs::None).unwrap();
        assert_eq!(text_of(&editor, id), "copy mecopy me");
    }

    #[test]
    fn cut_removes_the_selection_and_fills_the_clipboard() {
        let mut editor = editor();
        let id = editor.new_document();
        editor.type_text("keep cut");
        editor.set_selection(Selection::new(CharOffset::new(4), CharOffset::new(8)));
        editor.cut().unwrap();
        assert_eq!(text_of(&editor, id), "keep");
        editor.paste_from_clipboard().unwrap();
        assert_eq!(text_of(&editor, id), "keep cut");
    }

    #[test]
    fn pasted_text_is_normalized() {
        let mut editor = editor();
        let id = editor.new_document();
        editor.paste_text("windows\r\nclipboard\r\n");
        assert_eq!(text_of(&editor, id), "windows\nclipboard\n");
    }

    #[test]
    fn disabled_commands_are_refused() {
        let mut editor = editor();
        editor.new_document();
        // Nothing is selected, so copy is not applicable.
        assert!(!editor.is_command_enabled("edit.copy"));
        assert!(matches!(
            editor.execute("edit.copy", CommandArgs::None),
            Err(EditorError::CommandNotEnabled("edit.copy"))
        ));
    }

    #[test]
    fn unknown_commands_are_refused() {
        let mut editor = editor();
        assert!(matches!(
            editor.execute("nope.nothing", CommandArgs::None),
            Err(EditorError::UnknownCommand(_))
        ));
    }

    #[test]
    fn tab_cycling_wraps_around() {
        let mut editor = editor();
        let first = editor.new_document();
        let second = editor.new_document();
        editor.cycle_tab(1);
        assert_eq!(editor.active(), Some(first), "wraps past the end");
        editor.cycle_tab(-1);
        assert_eq!(editor.active(), Some(second), "wraps past the start");
    }

    #[test]
    fn closing_a_tab_activates_a_neighbour() {
        let mut editor = editor();
        let first = editor.new_document();
        let second = editor.new_document();
        editor.close_document(second).unwrap();
        assert_eq!(editor.active(), Some(first));
        editor.close_document(first).unwrap();
        assert_eq!(editor.active(), None);
        assert!(editor.tabs().is_empty());
    }

    #[test]
    fn switching_tabs_does_not_reload_from_disk() {
        let dir = scratch("tab-switch");
        let path = dir.join("a.txt");
        std::fs::write(&path, b"on disk").unwrap();

        let mut editor = editor();
        let file = editor.open_document(&path).unwrap();
        editor.type_text("edited ");
        let other = editor.new_document();

        // The file changes on disk while the tab is in the background.
        std::fs::write(&path, b"changed underneath").unwrap();
        editor.set_active(file).unwrap();

        assert_eq!(
            text_of(&editor, file),
            "edited on disk",
            "tab switching must not touch the filesystem"
        );
        assert_eq!(editor.tabs(), &[file, other]);
    }

    #[test]
    fn external_changes_are_detected_as_a_conflict_when_dirty() {
        let dir = scratch("external");
        let path = dir.join("a.txt");
        std::fs::write(&path, b"original").unwrap();

        let mut editor = editor();
        let id = editor.open_document(&path).unwrap();
        assert_eq!(editor.refresh_external_state(id), Some(ExternalState::Unchanged));

        editor.type_text("!");
        std::fs::write(&path, b"changed by another program").unwrap();
        assert_eq!(editor.refresh_external_state(id), Some(ExternalState::Conflict));

        std::fs::remove_file(&path).unwrap();
        assert_eq!(editor.refresh_external_state(id), Some(ExternalState::Missing));
    }

    #[test]
    fn events_describe_what_happened() {
        let dir = scratch("events");
        let path = dir.join("a.txt");
        std::fs::write(&path, b"content").unwrap();

        let mut editor = editor();
        let id = editor.open_document(&path).unwrap();
        editor.type_text("x");
        editor.save(id).unwrap();

        let names: Vec<&str> = editor.drain_events().iter().map(|e| e.payload.name()).collect();
        assert!(names.contains(&"document_opened"));
        assert!(names.contains(&"document_edited"));
        assert!(names.contains(&"document_saved"));
        assert!(editor.drain_events().is_empty(), "draining consumes the queue");
    }

    #[test]
    fn snapshots_are_built_for_the_requested_viewport() {
        let mut editor = editor();
        let id = editor.new_document();
        editor.type_text("one\ntwo\nthree");

        let viewport = Viewport { visible_lines: 2, ..Default::default() };
        let snapshot = editor.render_snapshot(id, viewport).unwrap();
        assert_eq!(snapshot.lines.len(), 2);
        assert_eq!(snapshot.total_lines, 3);
        assert_eq!(snapshot.document_id, id);
    }

    #[test]
    fn page_movement_follows_the_viewport_height() {
        let mut editor = editor();
        let id = editor.new_document();
        let text: String = (0..200).map(|i| format!("line {i}\n")).collect();
        editor.paste_text(&text);
        editor.go_to(100, 0);
        editor.set_page_lines(30);

        editor.execute("cursor.page_up", CommandArgs::None).unwrap();
        let document = editor.document(id).unwrap();
        let line = document.text().char_to_line(document.selections().primary().head);
        assert_eq!(line, LineIndex::new(70));
    }

    #[test]
    fn configuration_changes_reach_open_documents() {
        let mut editor = editor();
        let id = editor.new_document();
        let mut config = EffectiveConfig::default();
        config.editor.tab_width = 8;
        config.editor.insert_spaces = true;
        editor.set_config(config);

        editor.insert_tab();
        assert_eq!(text_of(&editor, id), " ".repeat(8));
        assert_eq!(editor.document(id).unwrap().settings().tab_width, 8);
    }

    #[test]
    fn performance_budget_violations_are_reported_as_events() {
        let mut editor = editor();
        ls_perf::reset();
        let handle = ls_perf::metric(ls_perf::names::EDIT_APPLY);
        for _ in 0..20 {
            handle.record(std::time::Duration::from_millis(50));
        }
        editor.check_performance_budgets();
        let events = editor.drain_events();
        assert!(
            events.iter().any(|e| e.payload.name() == "performance_budget_exceeded"),
            "a failing contract must be observable"
        );
        ls_perf::reset();
    }
}
