//! Application shell (specification sections 8, 59).
//!
//! Owns the window, routes input through the command registry, keeps the
//! scroll position for each open document, and asks the core for one immutable
//! snapshot per frame. It contains no editing logic of its own: every action a
//! key can trigger is a registered command, and every document mutation happens
//! inside `EditorCore`.

use crate::devpanel::{self, Heartbeat};
use crate::keymap::{self, Binding};
use crate::layout::Layout;
use crate::menu::{self, MenuHit, MenuState};
use crate::renderer::{Frame, Renderer};
use crate::resources;
use crate::tabs::{self, TabGeometry, TabHit};
use crate::theme::Theme;
use ls_core::{
    CommandArgs, ContentState, DocumentId, EditorCore, EffectiveConfig, EventPayload, LineIndex,
    LoadInjection, PersistenceState, RenderSnapshot, ShellRequest, Viewport,
};
use ls_platform::ProcessSampler;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

const SUBSYSTEM: &str = "shell";

/// How long a transient status message stays on screen.
const STATUS_MESSAGE_TIME: Duration = Duration::from_secs(6);

/// Lines scrolled per wheel notch.
const WHEEL_LINES: f32 = 3.0;

/// Half a blink cycle (ADR-0013). Each transition invalidates only the caret,
/// so a blinking caret costs two small redraws a second rather than a frame
/// loop that never sleeps.
const CARET_BLINK: Duration = Duration::from_millis(500);

/// How often open documents are checked against disk.
///
/// **Explicitly temporary (docs/adr/ADR-0017-filesystem-change-notification.md).**
/// A poll, not a native filesystem watcher (ReadDirectoryChangesW / inotify):
/// cheap today (one `stat` per open tab every 1.5s, riding the same
/// timer-driven-redraw mechanism the caret already proved out) but not the
/// target architecture. Do not read this as "polling is the design" -- read
/// the ADR before assuming this can stay forever.
const EXTERNAL_WATCH_INTERVAL: Duration = Duration::from_millis(1500);

/// A double click has to be two clicks close together in time and space.
const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(400);
const DOUBLE_CLICK_SLOP: f32 = 4.0;

/// Which part of the window input is currently addressed to.
///
/// The shell used to have no answer to this question, and two defects came out
/// of that. Wheel events were routed by whatever happened to be under the
/// pointer, and the editor only "took" the scroll once a click had moved the
/// caret -- because every frame re-ran `ensure_cursor_visible`, which pulled the
/// view straight back to wherever the caret was. Making the target explicit
/// separates four things the window had been conflating:
///
/// ```text
/// window focus       the OS gave us the window        (winit tells us)
/// input focus        who consumes keys and the wheel  (this type)
/// document selection which document is active         (EditorCore)
/// pointer position   what is under the mouse          (hit testing)
/// ```
///
/// Opening a file or activating a tab grants the editor input focus outright.
/// No click is needed, and nothing polls for it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum InputFocus {
    /// Keys and the wheel go to the active document.
    #[default]
    Editor,
    /// A dropdown is open and owns input until it closes.
    Menu,
    /// A confirmation is up and owns input until it is answered.
    Prompt,
    /// The find bar owns keys; the wheel still scrolls the editor underneath
    /// it, which is why this is not folded into `Prompt`.
    Find,
    /// Typing a workspace-search query.
    SearchQuery,
    /// A navigable list owns the keyboard: the file tree, search results, or
    /// git status. Arrow keys move the selection, Enter acts on it.
    List,
    /// The command-runner panel owns the keyboard.
    Terminal,
}

/// Where a wheel event should go.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WheelTarget {
    /// Scroll the active document.
    Editor,
    /// The tab bar, the menu bar, or the status bar: no editor scrolling.
    Chrome,
    /// A modal-ish surface owns input; nothing scrolls.
    Blocked,
}

/// Routes a wheel event from the pointer position and the current focus.
///
/// A pure function so the routing table can be asserted directly:
///
/// ```text
/// pointer over editor / gutter / scrollbar -> editor scrolls
/// pointer over tab bar or menu bar         -> nothing scrolls
/// a dropdown or a confirmation is up       -> nothing scrolls
/// ```
pub fn wheel_target(layout: &Layout, focus: InputFocus, x: f32, y: f32) -> WheelTarget {
    match focus {
        InputFocus::Menu
        | InputFocus::Prompt
        | InputFocus::SearchQuery
        | InputFocus::List
        | InputFocus::Terminal => WheelTarget::Blocked,
        // The find bar takes the keyboard, not the mouse: looking elsewhere in
        // the document while search results are up is normal, not a reason to
        // swallow the wheel.
        InputFocus::Editor | InputFocus::Find => {
            if layout.text.contains(x, y)
                || layout.gutter.contains(x, y)
                || layout.scrollbar.contains(x, y)
            {
                WheelTarget::Editor
            } else {
                WheelTarget::Chrome
            }
        }
    }
}

/// Appends one directory's visible rows to a file tree, recursing into
/// whichever children are in `expanded`. A pure function of core state (for
/// the same reason `wheel_target` and `should_apply_lsp_diagnostics` are):
/// the tree's shape is worth asserting on directly, without a window.
///
/// Bounded by what the user actually opened, not by workspace size: `depth`
/// only grows by descending into an expanded directory, so a workspace with
/// ten thousand files but three expanded folders costs three `read_dir`
/// calls, not ten thousand.
fn append_tree_level(
    core: &EditorCore,
    expanded: &std::collections::HashSet<PathBuf>,
    dir: &Path,
    depth: usize,
    rows: &mut Vec<ListRow>,
) {
    let indent = "  ".repeat(depth);
    match core.workspace().enumerate_children(dir) {
        Ok(entries) => {
            for entry in entries {
                match entry.kind {
                    ls_core::EntryKind::Directory => {
                        let is_expanded = expanded.contains(&entry.path);
                        let glyph = if is_expanded { '\u{25be}' } else { '\u{25b8}' };
                        rows.push(ListRow {
                            label: format!("{indent}{glyph} {}", entry.name),
                            action: Some(ListAction::ToggleDirectory(entry.path.clone())),
                        });
                        if is_expanded {
                            append_tree_level(core, expanded, &entry.path, depth + 1, rows);
                        }
                    }
                    _ => {
                        rows.push(ListRow {
                            label: format!("{indent}  {}", entry.name),
                            action: Some(ListAction::OpenFile(entry.path.clone())),
                        });
                    }
                }
            }
        }
        Err(error) => {
            rows.push(ListRow { label: format!("{indent}{error}"), action: None });
        }
    }
}

/// Whether an incoming diagnostics version should replace what is applied.
///
/// A pure function for the same reason `wheel_target` is: the staleness rule
/// is worth asserting on directly. `latest_applied` is the version currently
/// shown for this path (`None` if nothing has been applied yet); `incoming`
/// is the version the notification that just arrived claims to be for
/// (`None` if the server never echoes one, in which case there is nothing to
/// compare and it is applied unconditionally -- the pre-existing behavior).
pub fn should_apply_lsp_diagnostics(latest_applied: Option<u64>, incoming: Option<u64>) -> bool {
    match incoming {
        None => true,
        Some(version) => version >= latest_applied.unwrap_or(0),
    }
}

/// How the user answered a confirmation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PromptAnswer {
    Save,
    Discard,
    Cancel,
}

/// What a row in a navigable list panel does when chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ListAction {
    OpenFile(PathBuf),
    /// Opens the file and moves the caret to this 1-based line.
    OpenFileAt(PathBuf, usize),
    /// Expands this directory if it was collapsed, or collapses it if it was
    /// open -- a real tree, where several branches can be open across the
    /// whole workspace at once, not a single-directory drill-down.
    ToggleDirectory(PathBuf),
}

/// One row of a navigable list panel. `action: None` marks a row that is
/// informational only (a "no results" line, a status message).
#[derive(Clone, Debug)]
struct ListRow {
    label: String,
    action: Option<ListAction>,
}

/// Which navigable list is on screen. Items 6, 7 and 11 (file tree, workspace
/// search, git status) share one mechanism rather than three: each is "a list
/// of rows, pick one", so they share the input handling and the overlay
/// rendering, and differ only in how their rows are produced.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ListKind {
    FileTree,
    SearchResults,
    GitStatus,
}

/// What the editor is waiting for the user to decide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prompt {
    /// A dirty document is being closed: Save / Don't Save / Cancel.
    ConfirmClose { document: DocumentId, name: String, quitting: bool },
}

/// Something that happened off the event-loop thread and needs its attention.
///
/// This is the only way background work reaches the shell: a worker publishes a
/// completion and wakes the loop, the loop pumps, and every resulting state
/// change happens here on the event-loop thread (amendment section 3.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserEvent {
    /// A scheduler task finished; drain and apply completions.
    TaskCompleted,
    /// The terminal's child process produced output; drain it into the
    /// scrollback. Carries no data itself -- the bytes live in the shared
    /// buffer `Terminal` owns, the same split the completion waker uses.
    TerminalOutput,
    /// The LSP server published diagnostics; drain and apply them. Same
    /// split as the two events above: the reader thread only appends to a
    /// shared buffer and wakes the loop.
    LspDiagnostics,
}

/// How prominently a status message should read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Severity {
    Success,
    Warning,
    Error,
}

/// Scroll position of one document, in pixels so that touchpads can scroll
/// smoothly and the first visible line falls out of the arithmetic.
#[derive(Copy, Clone, Debug, Default)]
struct View {
    scroll_y: f32,
    scroll_x: f32,
}

pub struct LightSpeed {
    core: EditorCore,
    theme: Theme,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    views: HashMap<DocumentId, View>,
    modifiers: ModifiersState,
    overlay_visible: bool,
    sampler: ProcessSampler,
    process_stats: ls_platform::ProcessStats,
    last_sample: Instant,
    /// When the input that is still waiting for a frame arrived.
    pending_input: Option<Instant>,
    metrics: ShellMetrics,
    process_start: Instant,
    first_frame_reported: bool,
    status_message: Option<(String, Instant, Severity)>,
    pointer: (f32, f32),
    dragging_selection: bool,
    dragging_scrollbar: bool,
    quit_confirm_pending: bool,
    /// A document to close once its in-flight save completes, and whether the
    /// application should exit afterwards.
    close_after_save: Option<(DocumentId, bool)>,
    should_exit: bool,
    last_layout: Option<Layout>,
    last_snapshot: Option<Arc<RenderSnapshot>>,
    overlay_rows: Vec<String>,
    startup_paths: Vec<PathBuf>,
    /// Sends [`UserEvent`]s from a worker back onto the event-loop thread.
    proxy: EventLoopProxy<UserEvent>,
    dev_panel_visible: bool,
    dev_panel_rows: Vec<String>,
    resource_center_visible: bool,
    resource_center_rows: Vec<String>,
    heartbeat: Heartbeat,
    menu: MenuState,
    menu_geometry: Option<menu::MenuGeometry>,
    /// The tab bar's rectangles for the frame that is on screen. Computed once
    /// per frame and handed to both the renderer and the click handler.
    tab_geometry: TabGeometry,
    /// Who consumes keys and the wheel.
    focus: InputFocus,
    /// The document the shell last saw as active. Compared after every state
    /// change so that whatever moved the selection -- a click, Ctrl+Tab, a
    /// finished load, a closed tab -- hands editing focus to the new document
    /// without each of those paths having to remember to.
    last_active: Option<DocumentId>,
    /// Set when the caret moved and the view should follow it. Scrolling with
    /// the wheel deliberately does not set it, which is what lets the user look
    /// somewhere else without the view snapping back.
    reveal_cursor: bool,
    show_status_bar: bool,
    /// Caret visibility and when it next flips.
    caret_visible: bool,
    caret_deadline: Instant,
    /// The confirmation the user is being asked for, if any.
    prompt: Option<Prompt>,
    /// When and where the last click landed, and how many consecutive clicks
    /// have now happened there -- 1 places the caret, 2 selects a word, 3
    /// selects a line. A click elsewhere, or a slow one, resets to 1.
    last_click: Option<(Instant, f32, f32, u32)>,
    window_title: String,
    /// Path the diagnostics commands act on: the last file that was opened.
    diagnostics_path: Option<PathBuf>,
    /// When open documents are next checked against disk.
    next_watch_check: Instant,
    /// External state as of the last check, so a transition (not just "still
    /// changed") is what triggers a status message.
    last_external_state: HashMap<DocumentId, ls_core::ExternalState>,
    /// Where the recent-files list is persisted, if the platform gives us
    /// somewhere standard to put it. `None` just means the feature is
    /// in-memory only for this run -- never a reason to fail startup.
    recent_files_path: Option<PathBuf>,
    /// Which navigable list (file tree / search results / git status) is
    /// shown, if any.
    active_list: Option<ListKind>,
    /// Where the file tree is rooted: the workspace root, or the active
    /// document's directory if no workspace has been opened.
    file_tree_root: Option<PathBuf>,
    /// Which directories are expanded. A real tree, not a single-directory
    /// drill-down: several branches across the workspace can be open at
    /// once, exactly as many are collapsed by default (only the root's
    /// immediate children are read until the user asks for more), so
    /// building the visible rows only ever touches directories the user
    /// actually opened -- one `read_dir` per expanded entry, not a walk of
    /// the workspace.
    expanded_dirs: std::collections::HashSet<PathBuf>,
    list_selected: usize,
    /// The workspace-search query as it is being typed, before Enter submits
    /// it. `None` when the search bar is not open.
    search_query_input: Option<String>,
    /// A line to jump to once the file a list row just requested finishes
    /// opening (it may still be loading asynchronously).
    pending_jump: Option<(PathBuf, usize)>,
    terminal: Option<crate::terminal::Terminal>,
    terminal_visible: bool,
    terminal_scrollback: String,
    terminal_input: String,
    /// The one running language server, if a recognized document has been
    /// opened. Keyed by nothing -- Stage 1.1 runs at most one, for whichever
    /// language last needed it, since only Rust has a server configured.
    lsp: Option<crate::lsp::LspClient>,
    /// Documents the server has already been told about, so `didOpen` is
    /// sent exactly once per document.
    lsp_opened: std::collections::HashSet<DocumentId>,
    /// Persistence state as of the last check, so a save *completing* (not
    /// every frame while it stays completed) is what triggers a resync.
    lsp_last_persistence: HashMap<DocumentId, ls_core::PersistenceState>,
    /// The highest diagnostics version applied per path, so an
    /// out-of-order server response for an older edit cannot overwrite a
    /// newer one.
    lsp_applied_version: HashMap<PathBuf, u64>,
}

struct ShellMetrics {
    frame: ls_perf::MetricHandle,
    input_to_state: ls_perf::MetricHandle,
    input_to_frame: ls_perf::MetricHandle,
    snapshot: ls_perf::MetricHandle,
    startup: ls_perf::MetricHandle,
}

impl ShellMetrics {
    fn new() -> Self {
        ShellMetrics {
            frame: ls_perf::metric(ls_perf::names::FRAME),
            input_to_state: ls_perf::metric(ls_perf::names::INPUT_TO_STATE),
            input_to_frame: ls_perf::metric(ls_perf::names::INPUT_TO_FRAME),
            snapshot: ls_perf::metric(ls_perf::names::SNAPSHOT_BUILD),
            startup: ls_perf::metric(ls_perf::names::STARTUP_USABLE),
        }
    }
}

impl LightSpeed {
    pub fn new(
        config: EffectiveConfig,
        startup_paths: Vec<PathBuf>,
        process_start: Instant,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Self {
        ls_core::editor::install_default_budgets();
        let mut sampler = ProcessSampler::new();
        let process_stats = sampler.sample();

        let mut core = EditorCore::new(config);
        let recent_files_path = ls_platform::recents::default_path();
        if let Some(path) = &recent_files_path {
            core.set_recent_files(ls_platform::recents::load(path));
        }

        LightSpeed {
            core,
            theme: Theme::dark(),
            window: None,
            renderer: None,
            views: HashMap::new(),
            modifiers: ModifiersState::empty(),
            overlay_visible: false,
            sampler,
            process_stats,
            last_sample: Instant::now(),
            pending_input: None,
            metrics: ShellMetrics::new(),
            process_start,
            first_frame_reported: false,
            status_message: None,
            pointer: (0.0, 0.0),
            dragging_selection: false,
            dragging_scrollbar: false,
            quit_confirm_pending: false,
            close_after_save: None,
            should_exit: false,
            last_layout: None,
            last_snapshot: None,
            overlay_rows: Vec::new(),
            startup_paths,
            proxy,
            dev_panel_visible: false,
            dev_panel_rows: Vec::new(),
            resource_center_visible: false,
            resource_center_rows: Vec::new(),
            heartbeat: Heartbeat::new(),
            menu: MenuState::default(),
            menu_geometry: None,
            tab_geometry: TabGeometry::default(),
            focus: InputFocus::Editor,
            last_active: None,
            reveal_cursor: true,
            show_status_bar: true,
            caret_visible: true,
            caret_deadline: Instant::now() + CARET_BLINK,
            prompt: None,
            last_click: None,
            window_title: String::new(),
            diagnostics_path: None,
            recent_files_path,
            next_watch_check: Instant::now() + EXTERNAL_WATCH_INTERVAL,
            last_external_state: HashMap::new(),
            active_list: None,
            file_tree_root: None,
            expanded_dirs: std::collections::HashSet::new(),
            list_selected: 0,
            search_query_input: None,
            pending_jump: None,
            terminal: None,
            terminal_visible: false,
            terminal_scrollback: String::new(),
            terminal_input: String::new(),
            lsp: None,
            lsp_opened: std::collections::HashSet::new(),
            lsp_last_persistence: HashMap::new(),
            lsp_applied_version: HashMap::new(),
        }
    }

    /// Checks every open document against disk, on a timer.
    ///
    /// Runs from `about_to_wait`, the same place the caret ticks, so it costs
    /// nothing while the window is idle beyond the wakeup itself.
    fn poll_external_changes(&mut self) {
        if Instant::now() < self.next_watch_check {
            return;
        }
        self.next_watch_check = Instant::now() + EXTERNAL_WATCH_INTERVAL;

        for id in self.core.tabs().to_vec() {
            let Some(state) = self.core.refresh_external_state(id) else { continue };
            let previous = self.last_external_state.insert(id, state);
            if previous == Some(state) {
                continue;
            }
            let name = self
                .core
                .document(id)
                .map(|document| document.display_name().to_string())
                .unwrap_or_default();
            match state {
                ls_core::ExternalState::ExternallyChanged => {
                    self.set_status(format!("{name} changed on disk"), Severity::Warning);
                }
                ls_core::ExternalState::Missing => {
                    self.set_status(format!("{name} was deleted or moved"), Severity::Warning);
                }
                ls_core::ExternalState::Conflict => {
                    self.set_status(
                        format!("{name} changed on disk and has unsaved edits here"),
                        Severity::Warning,
                    );
                }
                ls_core::ExternalState::Unchanged => {}
            }
        }
        self.request_redraw();
    }

    /// The File menu's dynamic tail: recently opened files, from core state.
    fn recent_rows(&self) -> Vec<menu::RecentRow> {
        self.core
            .recent_files()
            .iter()
            .map(|path| {
                let label = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                menu::RecentRow { label, path: path.clone() }
            })
            .collect()
    }

    /// Best-effort: a failure to persist the recent list must never interrupt
    /// the user's actual work, so it goes to the log and nowhere else.
    fn save_recent_files(&self) {
        let Some(path) = &self.recent_files_path else { return };
        if let Err(error) = ls_platform::recents::save(path, self.core.recent_files()) {
            ls_log::warn!(SUBSYSTEM, "recent_files_not_saved", "{error}");
        }
    }

    /// Routes scheduler completions onto the event loop.
    ///
    /// The waker runs on a worker thread and does exactly one thing: post a
    /// user event. It never touches editor state.
    fn install_completion_waker(&self) {
        let proxy = self.proxy.clone();
        self.core.scheduler().set_completion_waker(Arc::new(move || {
            // A closed event loop is not an error: the editor is shutting down.
            let _ = proxy.send_event(UserEvent::TaskCompleted);
        }));
    }

    /// Applies finished background work and refreshes anything that depends on
    /// it. Runs only on the event-loop thread.
    fn pump_background_work(&mut self) {
        let applied = self.core.pump_completions();
        if applied == 0 {
            return;
        }

        // A close that was waiting on a save can finish now, but only if the
        // save actually cleaned the document (a stale save leaves it dirty).
        if let Some((document, quitting)) = self.close_after_save {
            if !self.core.is_saving(document) && !self.core.has_queued_save(document) {
                self.close_after_save = None;
                match self.core.close_document(document) {
                    Ok(()) => {
                        self.views.remove(&document);
                        if quitting {
                            self.should_exit = true;
                        }
                    }
                    Err(_) => self.set_status(
                        "The save did not finish cleanly; the document is still open",
                        Severity::Warning,
                    ),
                }
            }
        }

        self.after_state_change();
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn set_status(&mut self, message: impl Into<String>, severity: Severity) {
        self.status_message = Some((message.into(), Instant::now(), severity));
    }

    /// Color for the status line, so a failure does not read like a hint.
    fn status_color(&self) -> crate::theme::Color {
        match self.live_status_severity() {
            Some(Severity::Error) => self.theme.error,
            Some(Severity::Warning) => self.theme.warning,
            Some(Severity::Success) => self.theme.ok,
            None => self.theme.status_text,
        }
    }

    fn live_status_severity(&self) -> Option<Severity> {
        self.status_message
            .as_ref()
            .filter(|(_, at, _)| at.elapsed() < STATUS_MESSAGE_TIME)
            .map(|(_, _, severity)| *severity)
    }

    /// Runs a command and measures how long the editor took to reach its new
    /// state (specification section 48: input to editor state).
    fn run_command(&mut self, id: &str, args: CommandArgs) {
        let started = Instant::now();
        // Closing is the one action with a confirmation step, and asking the
        // user is the shell's job: the core provides both a refusing and an
        // explicit discarding close, and the shell decides which one applies.
        if id == "file.close_tab" {
            if let Some(active) = self.core.active() {
                self.close_tab(active);
                self.metrics.input_to_state.record(started.elapsed());
                self.after_state_change();
                return;
            }
        }
        match self.core.execute(id, args) {
            Ok(()) => {}
            Err(error) => {
                ls_log::debug!(SUBSYSTEM, "command_rejected", "{id}: {error}");
                self.set_status(error.to_string(), Severity::Warning);
            }
        }
        self.metrics.input_to_state.record(started.elapsed());
        self.after_state_change();
    }

    fn after_state_change(&mut self) {
        // Focus is derived from whatever surfaces are up, recomputed on every
        // state change rather than tracked at each place that could open or
        // close one -- opening find from the Edit menu, for instance, needs no
        // separate call here to hand it the keyboard.
        self.refresh_focus();
        self.reveal_cursor = true;
        self.apply_pending_jump();
        self.sync_lsp_for_active_document();
        let active = self.core.active();
        if active != self.last_active {
            self.last_active = active;
            if let Some(id) = active {
                self.adopt_new_document(id);
            }
        }
        self.handle_shell_requests();
        self.drain_events();
        if let Some(error) = self.core.take_last_error() {
            self.set_status(error, Severity::Error);
        }
        self.request_redraw();
    }

    /// Called when a document finishes loading or is newly created.
    ///
    /// Opening a file is what makes it the editing target; requiring a click
    /// first was the reason the wheel appeared dead on a freshly opened file.
    fn adopt_new_document(&mut self, id: DocumentId) {
        self.views.entry(id).or_default();
        if self.core.active() == Some(id) {
            self.focus_editor();
        }
    }

    fn handle_shell_requests(&mut self) {
        for request in self.core.take_shell_requests() {
            match request {
                ShellRequest::OpenFileDialog => self.show_open_dialog(),
                ShellRequest::SaveAsDialog => self.show_save_as_dialog(),
                ShellRequest::TogglePerformanceOverlay => {
                    self.overlay_visible = !self.overlay_visible;
                }
                ShellRequest::ToggleDevPanel => {
                    self.dev_panel_visible = !self.dev_panel_visible;
                }
                ShellRequest::ToggleResourceCenter => {
                    self.resource_center_visible = !self.resource_center_visible;
                }
                ShellRequest::ToggleFileTree => self.toggle_file_tree(),
                ShellRequest::OpenFolderDialog => self.show_open_folder_dialog(),
                ShellRequest::WorkspaceSearch => self.open_search_query(),
                ShellRequest::ToggleGitStatus => self.toggle_git_status(),
                ShellRequest::ToggleTerminal => self.toggle_terminal(),
                ShellRequest::ToggleStatusBar => {
                    self.show_status_bar = !self.show_status_bar;
                }
                ShellRequest::DiagnosticsDuplicateStorm => self.diagnostics_duplicate_storm(),
                ShellRequest::DiagnosticsSlowLoad => self.diagnostics_injected_load(
                    LoadInjection::delayed(Duration::from_millis(2500)),
                    "Slow-loading",
                ),
                ShellRequest::DiagnosticsFailingLoad => {
                    self.diagnostics_injected_load(LoadInjection::failing(), "Failing load of")
                }
                ShellRequest::Quit => self.request_quit(),
            }
        }
    }

    fn drain_events(&mut self) {
        for event in self.core.drain_events() {
            match &event.payload {
                EventPayload::PerformanceBudgetExceeded {
                    metric,
                    p95_micros,
                    threshold_micros,
                } => {
                    ls_log::warn!(
                        SUBSYSTEM,
                        "performance_budget_exceeded",
                        "{metric} p95 {p95_micros}us exceeds {threshold_micros}us"
                    );
                }
                EventPayload::DocumentSaveFailed { code, .. } => {
                    ls_log::warn!(SUBSYSTEM, "save_failed", "save failed: {code}");
                }
                _ => {}
            }
        }
    }

    fn window_handle_id(&self) -> Option<isize> {
        #[cfg(windows)]
        {
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let window = self.window.as_ref()?;
            match window.window_handle().ok()?.as_raw() {
                RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
                _ => None,
            }
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    fn show_open_dialog(&mut self) {
        let owner = self.window_handle_id();
        match ls_platform::dialog::open_file(owner, "Open File", None) {
            Ok(Some(path)) => self.open_path(path),
            Ok(None) => {}
            Err(error) => {
                ls_log::diag::log_error(&error);
                self.set_status(error.to_string(), Severity::Error);
            }
        }
    }

    fn show_save_as_dialog(&mut self) {
        let owner = self.window_handle_id();
        let suggested =
            self.core.active_document().map(|document| document.display_name().to_string());
        match ls_platform::dialog::save_file(owner, "Save As", None, suggested.as_deref()) {
            Ok(Some(path)) => {
                self.core.save_active_as(path);
                self.after_state_change();
            }
            Ok(None) => {}
            Err(error) => {
                ls_log::diag::log_error(&error);
                self.set_status(error.to_string(), Severity::Error);
            }
        }
    }

    /// Asks for a document. Returns immediately: the file is read on a
    /// scheduler worker and arrives through [`UserEvent::TaskCompleted`].
    pub fn open_path(&mut self, path: PathBuf) {
        self.open_path_with(path, LoadInjection::NONE);
    }

    fn open_path_with(&mut self, path: PathBuf, injection: LoadInjection) {
        match self.core.request_open_document_with(&path, injection) {
            Ok(request) => {
                self.adopt_new_document(request.document);
                self.diagnostics_path = Some(path.clone());
                self.save_recent_files();
                let message = if request.already_open {
                    format!("{} is already open", path.display())
                } else if request.joined {
                    format!("Joined the load already running for {}", path.display())
                } else {
                    format!("Loading {}...", path.display())
                };
                self.set_status(message, Severity::Success);
            }
            Err(error) => {
                let message = error.to_string();
                self.core.report_open_failure(error);
                self.set_status(message, Severity::Error);
            }
        }
        self.request_redraw();
    }

    /// Diagnostics: several requests for one path, issued back to back.
    ///
    /// With a delay injected they all land while the first is still reading, so
    /// the join path is visible in the panel rather than only in a test.
    fn diagnostics_duplicate_storm(&mut self) {
        let Some(path) = self.diagnostics_path.clone() else {
            self.set_status("Open a file first (Ctrl+O)", Severity::Warning);
            return;
        };
        self.close_diagnostics_target(&path);

        let injection = LoadInjection::delayed(Duration::from_millis(1200));
        let mut joined = 0;
        for index in 0..5 {
            let injection = if index == 0 { injection } else { LoadInjection::NONE };
            match self.core.request_open_document_with(&path, injection) {
                Ok(request) => {
                    self.views.entry(request.document).or_default();
                    if request.joined {
                        joined += 1;
                    }
                }
                Err(error) => {
                    self.set_status(error.to_string(), Severity::Error);
                    return;
                }
            }
        }
        self.dev_panel_visible = true;
        self.set_status(
            format!("Issued 5 requests for one path; {joined} joined the same task"),
            Severity::Success,
        );
        self.request_redraw();
    }

    fn diagnostics_injected_load(&mut self, injection: LoadInjection, description: &str) {
        let Some(path) = self.diagnostics_path.clone() else {
            self.set_status("Open a file first (Ctrl+O)", Severity::Warning);
            return;
        };
        self.close_diagnostics_target(&path);
        self.dev_panel_visible = true;
        self.set_status(format!("{description} {}", path.display()), Severity::Warning);
        self.open_path_with(path, injection);
    }

    /// Closes the document for a path so a diagnostics load actually reloads it
    /// instead of returning the copy that is already open.
    fn close_diagnostics_target(&mut self, path: &std::path::Path) {
        let open = self.core.tabs().iter().copied().find(|id| {
            self.core
                .document(*id)
                .and_then(|document| document.path().map(|p| p.as_path() == path))
                .unwrap_or(false)
        });
        if let Some(id) = open {
            let _ = self.core.close_document_discarding_changes(id);
            self.views.remove(&id);
        }
    }

    fn request_quit(&mut self) {
        let dirty = self
            .core
            .tabs()
            .iter()
            .filter(|id| self.core.document(**id).map(|d| d.is_dirty()).unwrap_or(false))
            .count();
        if dirty > 0 && !self.quit_confirm_pending {
            self.quit_confirm_pending = true;
            self.set_status(
                format!("{dirty} unsaved document(s). Press Ctrl+Q again to quit without saving."),
                Severity::Warning,
            );
            self.request_redraw();
            return;
        }
        self.quit_confirm_pending = false;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        self.should_exit = true;
    }

    /// Cursor line and display column of the active document.
    fn cursor_line_column(&self) -> Option<(usize, usize)> {
        let document = self.core.active_document()?;
        let head = document.selections().primary().head;
        let buffer = document.text();
        let line = buffer.char_to_line(head).get();
        let line_start = buffer.line_to_char(LineIndex::new(line));
        let column = ls_buffer::unicode::display_column_in(
            buffer,
            line_start,
            head,
            self.core.config().editor.tab_width,
        )
        .get();
        Some((line, column))
    }

    /// Scrolls the minimum amount that brings the caret back on screen.
    ///
    /// Only runs when something actually moved the caret. Running it every
    /// frame is what made the wheel appear broken: the view was pulled back to
    /// the caret before the scrolled frame was ever presented.
    fn ensure_cursor_visible(&mut self, layout: &Layout) {
        if !self.reveal_cursor {
            return;
        }
        self.reveal_cursor = false;
        let Some(id) = self.core.active() else { return };
        let Some((line, column)) = self.cursor_line_column() else { return };
        let total_lines =
            self.core.document(id).map(|document| document.text().len_lines()).unwrap_or(1);

        let line_height = layout.metrics.line_height;
        let digit_width = layout.metrics.digit_width;
        let text_height = layout.text.height;
        let text_width = layout.text.width;
        let view = self.views.entry(id).or_default();

        let top = line as f32 * line_height;
        if top < view.scroll_y {
            view.scroll_y = top;
        } else if top + line_height > view.scroll_y + text_height {
            view.scroll_y = top + line_height - text_height;
        }
        let max_scroll = (total_lines as f32 * line_height - text_height).max(0.0);
        view.scroll_y = view.scroll_y.clamp(0.0, max_scroll);

        let caret_x = column as f32 * digit_width;
        if caret_x < view.scroll_x {
            view.scroll_x = (caret_x - digit_width * 4.0).max(0.0);
        } else if caret_x + digit_width > view.scroll_x + text_width {
            view.scroll_x = caret_x + digit_width * 2.0 - text_width;
        }
        view.scroll_x = view.scroll_x.max(0.0);
    }

    fn scroll_by_lines(&mut self, lines: f32) {
        let Some(id) = self.core.active() else { return };
        let Some(layout) = self.last_layout else { return };
        let total_lines =
            self.core.document(id).map(|document| document.text().len_lines()).unwrap_or(1);
        let line_height = layout.metrics.line_height;
        let max_scroll = (total_lines as f32 * line_height - layout.text.height).max(0.0);
        let view = self.views.entry(id).or_default();
        view.scroll_y = (view.scroll_y - lines * line_height).clamp(0.0, max_scroll);
        self.request_redraw();
    }

    fn scroll_by_pixels(&mut self, dx: f32, dy: f32) {
        let Some(id) = self.core.active() else { return };
        let Some(layout) = self.last_layout else { return };
        let total_lines =
            self.core.document(id).map(|document| document.text().len_lines()).unwrap_or(1);
        let max_scroll =
            (total_lines as f32 * layout.metrics.line_height - layout.text.height).max(0.0);
        let view = self.views.entry(id).or_default();
        view.scroll_y = (view.scroll_y - dy).clamp(0.0, max_scroll);
        view.scroll_x = (view.scroll_x - dx).max(0.0);
        self.request_redraw();
    }

    fn on_mouse_press(&mut self, x: f32, y: f32) {
        let Some(layout) = self.last_layout else { return };

        // The menu is drawn over everything, so it gets the click first.
        if let Some(geometry) = &self.menu_geometry {
            match menu::hit(geometry, self.menu, x, y, &self.recent_rows()) {
                MenuHit::Title(index) => {
                    self.menu.toggle(index);
                    self.refresh_focus();
                    self.request_redraw();
                    return;
                }
                MenuHit::Command(command) => {
                    self.menu.close();
                    self.refresh_focus();
                    // A dimmed item is inert; the registry, not the menu, is
                    // the authority on what can run.
                    if self.core.is_command_enabled(command) {
                        self.run_command(command, CommandArgs::None);
                    } else {
                        self.request_redraw();
                    }
                    return;
                }
                MenuHit::OpenRecent(path) => {
                    self.menu.close();
                    self.refresh_focus();
                    self.open_path(path);
                    return;
                }
                MenuHit::Swallowed => return,
                MenuHit::None => {
                    if self.menu.is_open() {
                        // Clicking away closes the menu and does nothing else.
                        self.menu.close();
                        self.refresh_focus();
                        self.request_redraw();
                        return;
                    }
                }
            }
        }

        if self.prompt.is_some() {
            // The confirmation owns input until it is answered.
            return;
        }

        // A second click in the same place selects the word under it; a
        // third selects the line. A fourth stays at "line", rather than
        // growing a click count nothing consumes.
        let click_count = match self.last_click {
            Some((at, last_x, last_y, count))
                if at.elapsed() < DOUBLE_CLICK_TIME
                    && (last_x - x).abs() < DOUBLE_CLICK_SLOP
                    && (last_y - y).abs() < DOUBLE_CLICK_SLOP =>
            {
                (count + 1).min(3)
            }
            _ => 1,
        };
        self.last_click = Some((Instant::now(), x, y, click_count));

        if layout.tab_bar.contains(x, y) {
            // The close control is its own region, resolved before the body, so
            // closing a tab never activates it on the way past.
            match tabs::hit(&self.tab_geometry, x, y) {
                TabHit::Close(id) => self.close_tab(id),
                TabHit::Body(id) => self.activate_tab(id),
                TabHit::None => {}
            }
            return;
        }

        if layout.scrollbar.contains(x, y) {
            self.dragging_scrollbar = true;
            self.scroll_to_scrollbar_position(y, &layout);
            return;
        }

        if layout.sidebar_visible && layout.sidebar.contains(x, y) && self.active_list.is_some() {
            // Row 0 is the header, drawn but not itself a list entry.
            let row = ((y - layout.sidebar.y) / layout.metrics.line_height) as usize;
            if row >= 1 {
                let rows = self.list_rows();
                let index = row - 1;
                if index < rows.len() {
                    self.list_selected = index;
                    self.activate_list_selection();
                }
            }
            return;
        }

        if layout.text.contains(x, y) || layout.gutter.contains(x, y) {
            self.place_cursor_at(x, y, self.modifiers.contains(ModifiersState::SHIFT));
            match click_count {
                2 => self.select_word_at_cursor(),
                3 => self.select_line_at_cursor(),
                _ => self.dragging_selection = true,
            }
            self.wake_caret();
        }
    }

    /// Selects the word the caret is in, using the existing movement commands
    /// rather than a second definition of what a word is.
    fn select_word_at_cursor(&mut self) {
        let _ = self.core.execute("cursor.word_left", CommandArgs::None);
        let _ = self.core.execute("cursor.word_right.select", CommandArgs::None);
        self.after_state_change();
    }

    /// Selects the whole line the caret is on (triple-click), the same way:
    /// existing movement commands, not a separate definition of a line.
    fn select_line_at_cursor(&mut self) {
        let _ = self.core.execute("cursor.line_start", CommandArgs::None);
        let _ = self.core.execute("cursor.line_end.select", CommandArgs::None);
        self.after_state_change();
    }

    fn scroll_to_scrollbar_position(&mut self, y: f32, layout: &Layout) {
        let Some(id) = self.core.active() else { return };
        let total_lines =
            self.core.document(id).map(|document| document.text().len_lines()).unwrap_or(1);
        let track = layout.scrollbar;
        let progress = ((y - track.y) / track.height.max(1.0)).clamp(0.0, 1.0);
        let max_scroll =
            (total_lines as f32 * layout.metrics.line_height - layout.text.height).max(0.0);
        self.views.entry(id).or_default().scroll_y = max_scroll * progress;
        self.request_redraw();
    }

    fn place_cursor_at(&mut self, x: f32, y: f32, extend: bool) {
        let (Some(layout), Some(renderer), Some(snapshot)) =
            (self.last_layout, self.renderer.as_ref(), self.last_snapshot.as_ref())
        else {
            return;
        };
        let Some(id) = self.core.active() else { return };
        let view = self.views.get(&id).copied().unwrap_or_default();
        let scroll_fraction = view.scroll_y % layout.metrics.line_height;
        let (line, column) =
            renderer.position_at_point(&layout, snapshot, x, y, scroll_fraction, view.scroll_x);
        let started = Instant::now();
        if let Some(document) = self.core.active_document_mut() {
            document.move_to(LineIndex::new(line), column, extend);
        }
        self.metrics.input_to_state.record(started.elapsed());
        self.request_redraw();
    }

    /// Closes a tab. Unsaved work needs a second press to confirm, the same
    /// way quitting does: no dialog, but nothing is discarded by accident.
    /// Closes a tab. A clean document closes immediately; a dirty one asks.
    fn close_tab(&mut self, id: DocumentId) {
        match self.core.close_document(id) {
            Ok(()) => {
                self.views.remove(&id);
                self.last_external_state.remove(&id);
                self.request_redraw();
            }
            Err(ls_core::EditorError::UnsavedChanges(_)) => {
                let name = self
                    .core
                    .document(id)
                    .map(|document| document.display_name().to_string())
                    .unwrap_or_default();
                self.prompt = Some(Prompt::ConfirmClose { document: id, name, quitting: false });
                self.refresh_focus();
                self.request_redraw();
            }
            Err(error) => self.set_status(error.to_string(), Severity::Error),
        }
    }

    /// Answers the open confirmation.
    fn resolve_prompt(&mut self, answer: PromptAnswer) {
        let Some(prompt) = self.prompt.take() else { return };
        self.refresh_focus();
        let Prompt::ConfirmClose { document, name, quitting } = prompt;
        match answer {
            PromptAnswer::Save => {
                // Close once the save lands, so nothing is discarded and the
                // UI does not pretend the file is already persisted.
                match self.core.request_save(document) {
                    Ok(_) => {
                        self.close_after_save = Some((document, quitting));
                        self.set_status(format!("Saving {name}..."), Severity::Warning);
                    }
                    Err(error) => self.set_status(error.to_string(), Severity::Error),
                }
            }
            PromptAnswer::Discard => {
                let _ = self.core.close_document_discarding_changes(document);
                self.views.remove(&document);
                if quitting {
                    self.should_exit = true;
                }
            }
            PromptAnswer::Cancel => self.set_status("Close cancelled", Severity::Warning),
        }
        self.request_redraw();
    }

    /// Left half of the status bar.
    fn status_left(&self) -> String {
        if let Some((message, at, _)) = &self.status_message {
            if at.elapsed() < STATUS_MESSAGE_TIME {
                return message.clone();
            }
        }
        if let Some(summary) = devpanel::status_summary(&self.core) {
            return summary;
        }
        // Everything here is read from core state; the status bar keeps none
        // of its own.
        match self.core.active_document() {
            Some(document) => {
                let (line, column) = self.cursor_line_column().unwrap_or((0, 0));
                let mut line_ending = document.line_ending().label().to_string();
                if document.has_mixed_line_endings() {
                    line_ending.push_str(" (mixed)");
                }
                format!(
                    "Ln {}, Col {}    {}    {}    {}    {}",
                    line + 1,
                    column + 1,
                    document.encoding().label(),
                    line_ending,
                    document.language().name(),
                    self.document_state_label(),
                )
            }
            None => match self.core.active() {
                Some(_) => format!("{}    {}", self.document_state_label(), keymap::HINTS),
                None => format!("No document open.   {}", keymap::HINTS),
            },
        }
    }

    /// Right half of the status bar.
    fn status_right(&self) -> String {
        let frame = self.metrics.frame.stats();
        let input = self.metrics.input_to_state.stats();
        let mut parts: Vec<String> = Vec::with_capacity(6);
        if let Some(document) = self.core.active_document() {
            if let Some(path) = document.path() {
                parts.push(path.display_string());
            }
        }
        parts.push(format!("RAM {:.0} MB", self.process_stats.rss_mb()));
        parts.push(format!("CPU {:.0}%", self.process_stats.cpu_percent));
        if input.count > 0 {
            parts.push(format!("input {}", ls_perf::format_duration(input.p95)));
        }
        if frame.count > 0 {
            parts.push(format!("frame {}", ls_perf::format_duration(frame.p95)));
        }
        parts.join("   ")
    }

    /// Gives the editor input focus and puts the caret back on screen.
    ///
    /// Called when a document is opened and when a tab is activated, so the
    /// wheel and the keyboard address the new document immediately rather than
    /// after a click.
    fn focus_editor(&mut self) {
        self.menu.close();
        self.focus = InputFocus::Editor;
        self.reveal_cursor = true;
        self.wake_caret();
        self.request_redraw();
    }

    /// Recomputes the input target from the surfaces that are up.
    ///
    /// Focus is derived, not accumulated: there is no way for it to be left
    /// pointing at a menu that has since closed.
    fn refresh_focus(&mut self) {
        self.focus = if self.prompt.is_some() {
            InputFocus::Prompt
        } else if self.menu.is_open() {
            InputFocus::Menu
        } else if self.terminal_visible {
            InputFocus::Terminal
        } else if self.search_query_input.is_some() {
            InputFocus::SearchQuery
        } else if self.active_list.is_some() {
            InputFocus::List
        } else if self.core.is_find_open() {
            InputFocus::Find
        } else {
            InputFocus::Editor
        };
    }

    /// Activates a tab and moves editing focus to it.
    ///
    /// The document is identified by `DocumentId`, never by tab position: the
    /// tab list can change shape between the frame that was drawn and the click
    /// that lands on it.
    fn activate_tab(&mut self, id: DocumentId) {
        if self.core.active() == Some(id) {
            self.focus_editor();
            return;
        }
        match self.core.set_active(id) {
            Ok(()) => {
                self.focus_editor();
                self.after_state_change();
            }
            Err(error) => self.set_status(error.to_string(), Severity::Error),
        }
    }

    /// Makes the caret solid and pushes the next blink out.
    ///
    /// Typing and moving should never blink mid-gesture: the caret is where the
    /// user is looking.
    fn wake_caret(&mut self) {
        self.caret_visible = true;
        self.caret_deadline = Instant::now() + CARET_BLINK;
    }

    /// Flips the caret if its timer expired. Returns true when it changed, so
    /// the caller can invalidate just that region.
    fn tick_caret(&mut self) -> bool {
        if Instant::now() < self.caret_deadline {
            return false;
        }
        self.caret_visible = !self.caret_visible;
        self.caret_deadline = Instant::now() + CARET_BLINK;
        true
    }

    /// The confirmation strip's text, phrased so all three answers are visible
    /// without a modal dialog (specification section 16).
    fn prompt_text(&self) -> Option<String> {
        let Prompt::ConfirmClose { name, .. } = self.prompt.as_ref()?;
        Some(format!("{name} has unsaved changes.    [S] Save     [D] Don't Save     [Esc] Cancel"))
    }

    /// The find bar's text, when it is open: the query typed so far and a
    /// running "3 of 12" (or "No results"), so the count is always current
    /// rather than only updating when a match is found.
    fn find_bar_text(&self) -> Option<String> {
        if !self.core.is_find_open() {
            return None;
        }
        let find = self.core.find_state()?;
        let position = match find.position() {
            Some((current, total)) => format!("{current} of {total}"),
            None if find.query().is_empty() => String::new(),
            None => "No results".to_string(),
        };
        Some(format!(
            "Find: {}    {position}    [Enter] Next  [Shift+Enter] Prev  [Esc] Close",
            find.query()
        ))
    }

    /// One banner strip's text: a close confirmation and the find bar are
    /// mutually exclusive (`refresh_focus` gives the confirmation priority),
    /// so they share the single overlay strip rather than needing two.
    fn search_query_bar_text(&self) -> Option<String> {
        let query = self.search_query_input.as_ref()?;
        Some(format!("Search workspace: {query}    [Enter] Search  [Esc] Cancel"))
    }

    fn banner_text(&self) -> Option<String> {
        self.prompt_text().or_else(|| self.find_bar_text()).or_else(|| self.search_query_bar_text())
    }

    /// Builds the rows for whichever list is currently shown, fresh every
    /// call. All three lists are cheap to (re)build: git status and search
    /// results just format state `EditorCore` already has, and the file tree
    /// only reads directories the user actually expanded (never a recursive
    /// walk), so nothing here is worth caching and nothing can go stale.
    fn list_rows(&self) -> Vec<ListRow> {
        let root = self.core.workspace().root().map(|c| c.as_path().to_path_buf());
        match self.active_list {
            None => Vec::new(),
            Some(ListKind::FileTree) => match &self.file_tree_root {
                Some(tree_root) => {
                    let mut rows = Vec::new();
                    self.append_tree_level(tree_root, 0, &mut rows);
                    if rows.is_empty() {
                        rows.push(ListRow { label: "(empty)".to_string(), action: None });
                    }
                    rows
                }
                None => Vec::new(),
            },
            Some(ListKind::GitStatus) => match self.core.git_status() {
                Some(status) if status.is_clean() => {
                    vec![ListRow { label: "Working tree clean".to_string(), action: None }]
                }
                Some(status) => status
                    .files
                    .iter()
                    .map(|file| {
                        let flag = if file.staged { "staged  " } else { "        " };
                        let full = root.as_ref().map(|r| r.join(&file.path));
                        ListRow {
                            label: format!("{flag}{:?}  {}", file.state, file.path.display()),
                            action: full.map(ListAction::OpenFile),
                        }
                    })
                    .collect(),
                None if self.core.is_git_status_pending() => {
                    vec![ListRow { label: "Checking git status...".to_string(), action: None }]
                }
                None => vec![ListRow {
                    label: "Not a git repository, or git is not installed".to_string(),
                    action: None,
                }],
            },
            Some(ListKind::SearchResults) => match self.core.workspace_search_result() {
                Some(result) if result.hits.is_empty() => vec![ListRow {
                    label: format!("No matches for \"{}\"", result.query),
                    action: None,
                }],
                Some(result) => {
                    let mut rows: Vec<ListRow> = result
                        .hits
                        .iter()
                        .map(|hit| ListRow {
                            label: format!(
                                "{}:{}  {}",
                                hit.path.display(),
                                hit.line_number,
                                hit.preview
                            ),
                            action: Some(ListAction::OpenFileAt(hit.path.clone(), hit.line_number)),
                        })
                        .collect();
                    if result.truncated {
                        rows.push(ListRow {
                            label: "... more results exist; narrow the query".to_string(),
                            action: None,
                        });
                    }
                    rows
                }
                None if self.core.is_workspace_search_pending() => {
                    vec![ListRow { label: "Searching...".to_string(), action: None }]
                }
                None => Vec::new(),
            },
        }
    }

    /// Appends one directory's visible rows, recursing into whichever of its
    /// children are themselves expanded. `depth` only ever grows by walking
    /// into directories the user opened, so this cannot run away on a huge
    /// workspace the way a "expand everything" tree would.
    fn append_tree_level(&self, dir: &Path, depth: usize, rows: &mut Vec<ListRow>) {
        append_tree_level(&self.core, &self.expanded_dirs, dir, depth, rows);
    }

    /// Opens the file tree, rooted at the workspace root, or at the active
    /// document's directory if no workspace has been opened yet.
    fn toggle_file_tree(&mut self) {
        if self.active_list == Some(ListKind::FileTree) {
            self.active_list = None;
            self.refresh_focus();
            self.request_redraw();
            return;
        }
        let start = self.core.workspace().root().map(|c| c.as_path().to_path_buf()).or_else(|| {
            self.core
                .active_document()
                .and_then(|d| d.path())
                .and_then(|p| p.as_path().parent().map(|parent| parent.to_path_buf()))
        });
        let Some(start) = start else {
            self.set_status("Open a file first, or use File > Open Folder", Severity::Warning);
            return;
        };
        self.file_tree_root = Some(start);
        self.list_selected = 0;
        self.active_list = Some(ListKind::FileTree);
        self.refresh_focus();
        self.request_redraw();
    }

    /// Opens the native folder picker and, if the user chooses one, opens it
    /// as the workspace root and shows the explorer. This never depends on a
    /// document already being open -- the picker itself is where the folder
    /// comes from.
    fn show_open_folder_dialog(&mut self) {
        let owner = self.window_handle_id();
        let initial_dir =
            self.core.workspace().root().map(|c| c.as_path().to_path_buf()).or_else(|| {
                self.core
                    .active_document()
                    .and_then(|d| d.path())
                    .and_then(|p| p.as_path().parent().map(|parent| parent.to_path_buf()))
            });
        match ls_platform::dialog::open_folder(owner, "Open Folder", initial_dir.as_deref()) {
            Ok(Some(dir)) => {
                if let Err(error) = self.core.open_workspace(&dir) {
                    self.set_status(error.to_string(), Severity::Error);
                    return;
                }
                self.file_tree_root = Some(dir);
                self.list_selected = 0;
                self.active_list = Some(ListKind::FileTree);
                self.refresh_focus();
                self.request_redraw();
            }
            Ok(None) => {}
            Err(error) => {
                ls_log::diag::log_error(&error);
                self.set_status(error.to_string(), Severity::Error);
            }
        }
    }

    fn toggle_git_status(&mut self) {
        if self.active_list == Some(ListKind::GitStatus) {
            self.active_list = None;
            self.refresh_focus();
            self.request_redraw();
            return;
        }
        if let Err(error) = self.core.request_git_status() {
            self.set_status(error.to_string(), Severity::Error);
            return;
        }
        self.active_list = Some(ListKind::GitStatus);
        self.list_selected = 0;
        self.refresh_focus();
        self.request_redraw();
    }

    fn open_search_query(&mut self) {
        self.active_list = None;
        self.search_query_input = Some(String::new());
        self.refresh_focus();
        self.request_redraw();
    }

    /// Shows or hides the command runner. The child process, once spawned,
    /// keeps running while the panel is hidden -- hiding is not closing.
    fn toggle_terminal(&mut self) {
        if self.terminal_visible {
            self.terminal_visible = false;
            self.refresh_focus();
            self.request_redraw();
            return;
        }
        if self.terminal.is_none() {
            let proxy = self.proxy.clone();
            match crate::terminal::Terminal::spawn(move || {
                let _ = proxy.send_event(UserEvent::TerminalOutput);
            }) {
                Ok(terminal) => self.terminal = Some(terminal),
                Err(error) => {
                    self.set_status(format!("could not start a shell: {error}"), Severity::Error);
                    return;
                }
            }
        }
        self.terminal_visible = true;
        self.refresh_focus();
        self.request_redraw();
    }

    fn drain_terminal_output(&mut self) {
        let Some(terminal) = self.terminal.as_ref() else { return };
        let text = terminal.drain_output();
        if text.is_empty() {
            return;
        }
        self.terminal_scrollback.push_str(&text);
        let cap = 64 * 1024;
        if self.terminal_scrollback.len() > cap {
            let cut = self.terminal_scrollback.len() - cap;
            // Cut on a character boundary, not mid-codepoint.
            let boundary = (cut..self.terminal_scrollback.len())
                .find(|&i| self.terminal_scrollback.is_char_boundary(i))
                .unwrap_or(cut);
            self.terminal_scrollback.drain(..boundary);
        }
        if self.terminal_visible {
            self.request_redraw();
        }
    }

    /// Starts (or reuses) a language server for the active document if one
    /// is configured for its language, and keeps it synced: `didOpen` once,
    /// then a full-text resync each time a save completes.
    fn sync_lsp_for_active_document(&mut self) {
        let Some(id) = self.core.active() else { return };
        if self.core.is_loading(id) {
            return;
        }
        let Some(document) = self.core.document(id) else { return };
        // An untitled document has no URI to report itself under.
        let Some(path) = document.path().map(|p| p.as_path().to_path_buf()) else { return };
        let language = document.language();

        if let Some(client) = self.lsp.as_mut() {
            if !client.is_alive() {
                // The server crashed or exited; a future recognized document
                // gets a fresh one rather than silently having none forever.
                self.lsp = None;
                self.lsp_opened.clear();
            }
        }

        if !self.lsp_opened.contains(&id) {
            if self.lsp.is_none() {
                let proxy = self.proxy.clone();
                let root = self
                    .core
                    .workspace()
                    .root()
                    .map(|c| c.as_path().to_path_buf())
                    .or_else(|| path.parent().map(|p| p.to_path_buf()))
                    .unwrap_or_else(|| PathBuf::from("."));
                self.lsp = crate::lsp::LspClient::spawn(language, &root, move || {
                    let _ = proxy.send_event(UserEvent::LspDiagnostics);
                });
            }
            if let Some(client) = self.lsp.as_mut() {
                client.notify_opened(&path, language, &document.text().to_string());
            }
            // Marked regardless of whether a client actually started: there
            // is no server for most languages, and retrying that check on
            // every keystroke would be wasted work for a "no" that never
            // changes for this document.
            self.lsp_opened.insert(id);
            return;
        }

        let persistence = document.persistence_state();
        let previous = self.lsp_last_persistence.insert(id, persistence);
        if previous != Some(persistence) && persistence == ls_core::PersistenceState::SaveSucceeded
        {
            if let Some(client) = self.lsp.as_mut() {
                client.notify_saved(&path, &document.text().to_string());
            }
        }
    }

    /// Applies diagnostics the server sent, gated against staleness: the
    /// same class of bug the async save path already solved with
    /// `ContentRevision` (a stale save must not report a document clean),
    /// here for a stale diagnostic that must not overwrite a newer one.
    ///
    /// A slow re-analysis for an old `didChange` can complete *after* a
    /// faster one for a newer edit, so "arrived later" does not mean "is
    /// newer". What does mean that is the version number this client put in
    /// the request -- rust-analyzer echoes it back in
    /// `PublishDiagnosticsParams.version`. Diagnostics naming a version older
    /// than the last one already applied for that path are dropped rather
    /// than allowed to overwrite it. A server that omits the (optional)
    /// version field gets applied unconditionally, which is the pre-existing
    /// behavior and the best any client can do without that signal.
    fn drain_lsp_diagnostics(&mut self) {
        let Some(client) = self.lsp.as_ref() else { return };
        let updates = client.drain_diagnostics();
        if updates.is_empty() {
            return;
        }
        for (path, version, diagnostics) in updates {
            let latest = self.lsp_applied_version.get(&path).copied();
            if !should_apply_lsp_diagnostics(latest, version) {
                continue;
            }
            if let Some(version) = version {
                self.lsp_applied_version.insert(path.clone(), version);
            }
            self.core.apply_diagnostics(&path, diagnostics);
        }
        self.request_redraw();
    }

    fn send_terminal_line(&mut self) {
        let line = std::mem::take(&mut self.terminal_input);
        if let Some(terminal) = self.terminal.as_mut() {
            if !terminal.is_alive() {
                self.set_status("The shell has exited", Severity::Warning);
                self.terminal = None;
                return;
            }
            self.terminal_scrollback.push_str("> ");
            self.terminal_scrollback.push_str(&line);
            self.terminal_scrollback.push('\n');
            terminal.send_line(&line);
        }
    }

    /// The terminal panel's rows: a fixed window of the scrollback plus the
    /// line being typed, in the same `Vec<String>` shape every other panel
    /// uses.
    fn terminal_rows(&self) -> Vec<String> {
        let mut rows = Vec::new();
        rows.push("Terminal  (Enter to run, F11 to hide)".to_string());
        let visible_lines = 12;
        let tail: Vec<&str> = self.terminal_scrollback.lines().collect();
        let start = tail.len().saturating_sub(visible_lines);
        rows.extend(tail[start..].iter().map(|line| line.to_string()));
        rows.push(format!("> {}", self.terminal_input));
        rows
    }

    /// Once a file a list row opened finishes loading, moves the caret to the
    /// line the row named. Opening is asynchronous, so this cannot happen
    /// inline with the click -- it runs on every pump until the load settles.
    fn apply_pending_jump(&mut self) {
        let Some((path, line)) = self.pending_jump.clone() else { return };
        let Some(active) = self.core.active() else { return };
        if self.core.is_loading(active) {
            return;
        }
        let Some(document) = self.core.document(active) else {
            self.pending_jump = None;
            return;
        };
        if document.path().map(|p| p.as_path()) != Some(path.as_path()) {
            // The active tab changed before the load settled; give up quietly
            // rather than jumping in the wrong document.
            self.pending_jump = None;
            return;
        }
        self.core.go_to(line.saturating_sub(1), 0);
        self.pending_jump = None;
    }

    /// Runs whatever a list row's selection means.
    fn activate_list_selection(&mut self) {
        let rows = self.list_rows();
        let Some(row) = rows.get(self.list_selected) else { return };
        match row.action.clone() {
            Some(ListAction::OpenFile(path)) => {
                // The sidebar is a docked panel, not a modal picker -- opening
                // a file from the explorer or git status leaves it open, the
                // way VS Code's does, instead of yanking it away every time.
                // `open_path` already moves keyboard focus to the editor (via
                // `adopt_new_document`), so this must not call
                // `refresh_focus` afterwards -- that would see `active_list`
                // still set and hand focus straight back to the list.
                self.open_path(path);
                self.request_redraw();
                return;
            }
            Some(ListAction::OpenFileAt(path, line)) => {
                self.pending_jump = Some((path.clone(), line));
                self.open_path(path);
                self.request_redraw();
                return;
            }
            Some(ListAction::ToggleDirectory(dir)) => {
                if !self.expanded_dirs.remove(&dir) {
                    self.expanded_dirs.insert(dir);
                }
                // The row count changes (a subtree appeared or disappeared);
                // keep the selection in range rather than pointing past the
                // end of a collapsed branch.
                let len = self.list_rows().len();
                if len > 0 {
                    self.list_selected = self.list_selected.min(len - 1);
                }
            }
            None => {}
        }
        self.refresh_focus();
        self.request_redraw();
    }

    /// When the loop should wake next, if anything is on a timer.
    fn next_wakeup(&self) -> Option<Instant> {
        // The caret and the disk-change poll are the only timers; everything
        // else is event-driven. Whichever fires first is when the loop wakes.
        let caret =
            (self.core.active().is_some() && self.prompt.is_none()).then_some(self.caret_deadline);
        let watch = (!self.core.tabs().is_empty()).then_some(self.next_watch_check);
        match (caret, watch) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Window title, from authoritative document state.
    fn compute_title(&self) -> String {
        match self.core.active() {
            Some(active) => {
                if self.core.is_loading(active) {
                    let name = self
                        .core
                        .pending_load(active)
                        .map(|pending| pending.path.file_name())
                        .unwrap_or_else(|| "Loading".to_string());
                    return format!("LightSpeed IDE - {name} (loading)");
                }
                match self.core.document(active) {
                    Some(document) => {
                        let dirty = if document.is_dirty() { " *" } else { "" };
                        format!("LightSpeed IDE - {}{dirty}", document.display_name())
                    }
                    None => "LightSpeed IDE".to_string(),
                }
            }
            None => "LightSpeed IDE".to_string(),
        }
    }

    /// The document's state word for the status bar, read from core state
    /// rather than inferred from anything the UI keeps.
    fn document_state_label(&self) -> &'static str {
        let Some(active) = self.core.active() else { return "" };
        if self.core.is_loading(active) {
            return "Loading";
        }
        if self.core.is_saving(active) {
            return "Saving...";
        }
        let Some(document) = self.core.document(active) else { return "" };
        match (document.persistence_state(), document.content_state()) {
            (PersistenceState::SaveFailed, _) => "Save failed",
            (PersistenceState::SaveSucceeded, ContentState::Clean) => "Saved",
            (_, ContentState::Dirty) => "Dirty",
            (_, ContentState::Clean) => "Clean",
        }
    }

    /// Whether the docked sidebar (explorer, search, or git status) should
    /// be shown this frame. Typing a search query shows it before any
    /// request has gone out, so the query itself is never invisible.
    fn sidebar_visible(&self) -> bool {
        self.active_list.is_some() || self.search_query_input.is_some()
    }

    /// Rows for the docked sidebar: a header (row 0, never itself
    /// selectable) followed by the active list's rows, or -- while the user
    /// is typing a workspace-search query -- the live query as an editable
    /// line.
    fn sidebar_rows(&self) -> Vec<String> {
        if let Some(query) = &self.search_query_input {
            return vec![
                "Search  (Enter to run, Esc to cancel)".to_string(),
                format!("  {query}\u{2588}"),
            ];
        }
        let Some(kind) = self.active_list else { return Vec::new() };
        let title = match kind {
            ListKind::FileTree => "Explorer",
            ListKind::SearchResults => "Search Results",
            ListKind::GitStatus => "Source Control",
        };
        let mut rows = vec![format!("{title}  (Up/Down, Enter, Esc)")];
        for row in self.list_rows() {
            rows.push(row.label);
        }
        rows
    }

    /// Which sidebar row is selected, in the row numbering `sidebar_rows`
    /// uses (the header is row 0). `None` while typing a query: the input
    /// line is not a "selection" in the list sense.
    fn sidebar_selected_row(&self) -> Option<usize> {
        if self.search_query_input.is_some() {
            return None;
        }
        self.active_list.map(|_| self.list_selected + 1)
    }

    /// Rows for the floating performance/dev-tool panel, top-right. The
    /// explorer/search/git list lives in the docked sidebar instead (see
    /// `sidebar_rows`), not here -- this panel is diagnostics, not the UI the
    /// user works in day to day.
    fn panel_rows(&self) -> Vec<String> {
        let mut rows = Vec::new();
        if self.terminal_visible {
            rows.extend(self.terminal_rows());
        }
        if self.dev_panel_visible {
            if !rows.is_empty() {
                rows.push(String::new());
            }
            rows.extend(self.dev_panel_rows.iter().cloned());
        }
        if self.resource_center_visible {
            if !rows.is_empty() {
                rows.push(String::new());
            }
            rows.extend(self.resource_center_rows.iter().cloned());
        }
        if self.overlay_visible {
            if !rows.is_empty() {
                rows.push(String::new());
            }
            rows.extend(self.overlay_rows.iter().cloned());
        }
        rows
    }

    /// Builds the developer overlay (specification section 18 of the brief,
    /// section 56 of the contract).
    fn build_overlay(&mut self) {
        self.overlay_rows.clear();
        let snapshot = ls_perf::snapshot();
        self.overlay_rows.push("LightSpeed Performance".to_string());
        self.overlay_rows.push(format!(
            "{:<22} {:>8} {:>8} {:>8} {:>6}",
            "metric", "p50", "p95", "p99", "state"
        ));
        for metric in &snapshot.metrics {
            if metric.stats.count == 0 {
                continue;
            }
            self.overlay_rows.push(format!(
                "{:<22} {:>8} {:>8} {:>8} {:>6}",
                metric.name,
                ls_perf::format_duration(metric.stats.p50),
                ls_perf::format_duration(metric.stats.p95),
                ls_perf::format_duration(metric.stats.p99),
                metric.status.label(),
            ));
        }
        self.overlay_rows.push(String::new());
        self.overlay_rows.push(format!(
            "RSS {:.1} MB   peak {:.1} MB   CPU {:.1}%",
            self.process_stats.rss_mb(),
            self.process_stats.peak_rss_mb(),
            self.process_stats.cpu_percent
        ));
        let documents = self.core.tabs().len();
        let dropped = self.core.dropped_events();
        self.overlay_rows.push(format!("documents {documents}   dropped events {dropped}"));
        if let Some(renderer) = self.renderer.as_mut() {
            let reshaped = renderer.take_reshaped_count();
            let quads = renderer.quad_count();
            self.overlay_rows.push(format!("quads {quads}   regions reshaped {reshaped}"));
        }
        if let Some(document) = self.core.active_document() {
            self.overlay_rows.push(format!(
                "revision {}   undo {}   redo {}   lines {}",
                document.revision().get(),
                document.undo_depth(),
                document.redo_depth(),
                document.text().len_lines()
            ));
        }
    }

    fn redraw(&mut self) {
        let frame_started = Instant::now();
        // The window handle is cloned rather than borrowed: the rest of this
        // function needs `&mut self` to update editor-facing state.
        let Some(window) = self.window.clone() else { return };
        let (width, height, metrics) = match self.renderer.as_ref() {
            Some(renderer) => (renderer.size().0, renderer.size().1, renderer.metrics()),
            None => return,
        };
        let scale = window.scale_factor() as f32;

        let total_lines =
            self.core.active_document().map(|document| document.text().len_lines()).unwrap_or(1);
        let digits = total_lines.to_string().len();
        let layout = Layout::with_chrome(
            width,
            height,
            scale,
            metrics,
            digits,
            self.core.config().appearance.show_line_numbers,
            self.show_status_bar,
            self.sidebar_visible(),
        );
        self.last_layout = Some(layout);
        self.core.set_page_lines(layout.visible_lines());
        self.ensure_cursor_visible(&layout);

        // Refresh process statistics about once a second: sampling them every
        // frame would be measuring the measurement.
        if self.last_sample.elapsed() >= Duration::from_secs(1) {
            self.process_stats = self.sampler.sample();
            self.last_sample = Instant::now();
            ls_perf::gauge("process.rss_mb", self.process_stats.rss_mb());
            ls_perf::gauge("process.cpu_percent", self.process_stats.cpu_percent);
            self.core.check_performance_budgets();
            self.drain_events();
        }

        let snapshot = match self.core.active() {
            Some(id) => {
                let view = self.views.get(&id).copied().unwrap_or_default();
                let viewport = Viewport {
                    first_line: LineIndex::new(
                        (view.scroll_y / layout.metrics.line_height).floor() as usize,
                    ),
                    visible_lines: layout.visible_lines_with_partial(),
                    first_column: ls_core::DisplayColumn::new(
                        (view.scroll_x / layout.metrics.digit_width).floor() as usize,
                    ),
                    visible_columns: layout.visible_columns(),
                };
                let started = Instant::now();
                let snapshot = self.core.render_snapshot(id, viewport);
                self.metrics.snapshot.record(started.elapsed());
                snapshot
            }
            None => None,
        };
        self.last_snapshot = snapshot.clone();

        self.heartbeat.tick();
        let title = self.compute_title();
        if title != self.window_title {
            window.set_title(&title);
            self.window_title = title;
        }

        let recent_rows = self.recent_rows();
        let geometry = menu::geometry(
            layout.menu_bar,
            self.menu,
            layout.metrics.digit_width,
            layout.metrics.line_height,
            layout.scale,
            &recent_rows,
        );
        self.menu_geometry = Some(geometry.clone());
        if self.overlay_visible {
            self.build_overlay();
        }
        if self.dev_panel_visible {
            self.dev_panel_rows = devpanel::lines(&self.core, &self.heartbeat);
        }
        if self.resource_center_visible {
            self.resource_center_rows = resources::lines(&self.core, &self.process_stats);
        }

        let panel_rows = self.panel_rows();
        let sidebar_rows = self.sidebar_rows();
        let sidebar_selected_row = self.sidebar_selected_row();
        let status_left = self.status_left();
        let status_right = self.status_right();
        let status_color = self.status_color();
        // One tab computation per frame, shared by drawing and hit testing.
        // Storing it here is what keeps a click resolving against the same
        // rectangles the user is looking at.
        let presentations = self.core.tab_presentations();
        self.tab_geometry = tabs::geometry(
            layout.tab_bar,
            &presentations,
            layout.metrics.digit_width,
            layout.scale,
        );
        let tabs = self.tab_geometry.clone();
        let view =
            self.core.active().and_then(|id| self.views.get(&id).copied()).unwrap_or_default();

        let menu_enabled: Vec<bool> = match self.menu.open {
            Some(open) => {
                let mut enabled: Vec<bool> = menu::MENUS[open]
                    .items
                    .iter()
                    .map(|item| menu::is_enabled(&self.core, item))
                    .collect();
                // Opening a recent file is always allowed; only the registry
                // command items above are ever refused.
                if open == menu::FILE_MENU_INDEX {
                    enabled.extend(std::iter::repeat_n(true, recent_rows.len()));
                }
                enabled
            }
            None => Vec::new(),
        };
        let prompt_text = self.banner_text();
        let menu_state = self.menu;
        let empty_geometry =
            menu::MenuGeometry { titles: Vec::new(), dropdown: None, items: Vec::new() };
        let menu_geometry = self.menu_geometry.as_ref().unwrap_or(&empty_geometry);
        let caret_visible = self.caret_visible;

        let renderer = self.renderer.as_mut().expect("renderer checked above");
        let frame = Frame {
            layout,
            theme: &self.theme,
            snapshot: snapshot.as_deref(),
            tabs: &tabs,
            status_left: &status_left,
            status_right: &status_right,
            overlay: (!panel_rows.is_empty()).then_some(panel_rows.as_slice()),
            status_color,
            scroll_fraction: view.scroll_y % layout.metrics.line_height,
            horizontal_offset: view.scroll_x,
            menu: menu_state,
            menu_geometry,
            menu_enabled: &menu_enabled,
            recent_files: &recent_rows,
            caret_visible,
            prompt: prompt_text.as_deref(),
            placeholder: "  LightSpeed IDE\n\n  Ctrl+O  open a file\n  Ctrl+N  new file\n  F12     performance overlay",
            sidebar: (!sidebar_rows.is_empty()).then_some(sidebar_rows.as_slice()),
            sidebar_selected_row,
        };

        match renderer.render(&frame) {
            Ok(()) => {}
            Err(error) => {
                ls_log::warn!(SUBSYSTEM, "frame_failed", "frame not presented: {error:?}");
            }
        }

        let elapsed = frame_started.elapsed();
        self.metrics.frame.record(elapsed);
        if let Some(input_at) = self.pending_input.take() {
            self.metrics.input_to_frame.record(input_at.elapsed());
        }
        if !self.first_frame_reported {
            self.first_frame_reported = true;
            let startup = self.process_start.elapsed();
            self.metrics.startup.record(startup);
            // Memory is a contract too (specification section 50), so the first
            // frame reports the resident set it took to get there.
            self.process_stats = self.sampler.sample();
            let documents = self.core.tabs().len();
            ls_log::info!(
                SUBSYSTEM,
                "startup_complete",
                fields: [
                    ls_log::Field::float("millis", startup.as_secs_f64() * 1000.0),
                    ls_log::Field::float("rss_mb", self.process_stats.rss_mb()),
                    ls_log::Field::uint("documents", documents as u64),
                ],
                "editor usable"
            );
        }
        // Live numbers keep the loop turning: the performance overlay, the
        // loading panel, and any load in flight (whose elapsed time and
        // heartbeat are the evidence that the loop is not blocked).
        if self.overlay_visible
            || devpanel::wants_continuous_frames(&self.core, self.dev_panel_visible)
        {
            self.request_redraw();
        }
    }

    /// Set when a quit has been confirmed; checked by the event loop.
    fn take_exit(&mut self) -> bool {
        std::mem::take(&mut self.should_exit)
    }
}

impl ApplicationHandler<UserEvent> for LightSpeed {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("LightSpeed IDE")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                ls_log::error!(SUBSYSTEM, "window_failed", "could not create a window: {error}");
                event_loop.exit();
                return;
            }
        };
        let window_created = self.process_start.elapsed();
        ls_log::info!(
            SUBSYSTEM,
            "window_created",
            fields: [ls_log::Field::float("millis", window_created.as_secs_f64() * 1000.0)],
            "window created"
        );

        let appearance = self.core.config().appearance.clone();
        let renderer = pollster::block_on(Renderer::new(
            window.clone(),
            event_loop.owned_display_handle(),
            &appearance.font_family,
            appearance.font_size * window.scale_factor() as f32,
            appearance.line_height,
        ));
        match renderer {
            Ok(renderer) => self.renderer = Some(renderer),
            Err(error) => {
                ls_log::error!(SUBSYSTEM, "renderer_failed", "{error}");
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);

        self.install_completion_waker();

        let paths = std::mem::take(&mut self.startup_paths);
        for path in paths {
            self.open_path(path);
        }
        if self.core.tabs().is_empty() {
            // Start with an empty buffer so the editor is usable immediately.
            self.core.new_document();
        }
        if let Some(id) = self.core.active() {
            // The editor is the input target from the first frame; nothing has
            // to be clicked to wake it up.
            self.last_active = Some(id);
            self.adopt_new_document(id);
        }
        self.request_redraw();
    }

    /// Chooses how the loop sleeps.
    ///
    /// The default is `Wait`: no timer, no wakeups, no frames. The only thing
    /// that needs a clock is the caret, and it asks for a single deadline rather
    /// than a continuous frame loop (ADR-0013). When the caret is not blinking
    /// -- no document, or a confirmation is up -- the loop goes fully idle.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // The caret's clock is a loop concern, not a frame concern. Ticking it
        // here means the deadline actually invalidates something: it used to be
        // ticked inside `redraw`, which only runs when a frame was already
        // requested, so the timer fired and nothing blinked.
        if self.tick_caret() {
            self.request_redraw();
        }
        self.poll_external_changes();
        match self.next_wakeup() {
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    /// Background work reaching the event-loop thread.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::TaskCompleted => self.pump_background_work(),
            UserEvent::TerminalOutput => self.drain_terminal_output(),
            UserEvent::LspDiagnostics => self.drain_lsp_diagnostics(),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.request_quit();
                if self.take_exit() {
                    event_loop.exit();
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.resize(size.width, size.height);
                }
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                self.request_redraw();
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                self.pending_input.get_or_insert_with(Instant::now);

                // A confirmation owns the keyboard until it is answered.
                if self.prompt.is_some() {
                    if let Some(answer) = keymap::resolve_prompt(&event.logical_key) {
                        self.resolve_prompt(answer);
                        if self.take_exit() {
                            event_loop.exit();
                        }
                    }
                    return;
                }

                // A navigable list (file tree / search results / git status)
                // owns the keyboard while it is open.
                // The command runner owns the keyboard while it is shown.
                if self.focus == InputFocus::Terminal {
                    match &event.logical_key {
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape)
                        | winit::keyboard::Key::Named(winit::keyboard::NamedKey::F11) => {
                            self.toggle_terminal();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Enter) => {
                            self.send_terminal_line();
                            self.request_redraw();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Backspace) => {
                            self.terminal_input.pop();
                            self.request_redraw();
                        }
                        _ => {
                            if let Some(text) = event.text.as_ref() {
                                let printable: String =
                                    text.chars().filter(|c| !c.is_control()).collect();
                                if !printable.is_empty() {
                                    self.terminal_input.push_str(&printable);
                                    self.request_redraw();
                                }
                            }
                        }
                    }
                    return;
                }

                if self.focus == InputFocus::List {
                    match &event.logical_key {
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowDown) => {
                            let len = self.list_rows().len();
                            if len > 0 {
                                self.list_selected = (self.list_selected + 1).min(len - 1);
                            }
                            self.request_redraw();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowUp) => {
                            self.list_selected = self.list_selected.saturating_sub(1);
                            self.request_redraw();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Enter) => {
                            self.activate_list_selection();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape) => {
                            self.active_list = None;
                            self.refresh_focus();
                            self.request_redraw();
                        }
                        _ => {}
                    }
                    return;
                }

                // Typing a workspace-search query.
                if self.focus == InputFocus::SearchQuery {
                    let shift = self.modifiers.contains(ModifiersState::SHIFT);
                    match keymap::resolve_find(&event.logical_key, shift) {
                        keymap::FindAction::Close => {
                            self.search_query_input = None;
                            self.refresh_focus();
                            self.request_redraw();
                        }
                        keymap::FindAction::Backspace => {
                            if let Some(query) = self.search_query_input.as_mut() {
                                query.pop();
                            }
                            self.request_redraw();
                        }
                        keymap::FindAction::Next | keymap::FindAction::Previous => {
                            if let Some(query) = self.search_query_input.take() {
                                if let Err(error) = self.core.request_workspace_search(query) {
                                    self.set_status(error.to_string(), Severity::Error);
                                } else {
                                    self.active_list = Some(ListKind::SearchResults);
                                    self.list_selected = 0;
                                }
                                self.refresh_focus();
                                self.request_redraw();
                            }
                        }
                        keymap::FindAction::None => {
                            if let Some(text) = event.text.as_ref() {
                                let printable: String =
                                    text.chars().filter(|c| !c.is_control()).collect();
                                if !printable.is_empty() {
                                    if let Some(query) = self.search_query_input.as_mut() {
                                        query.push_str(&printable);
                                    }
                                    self.request_redraw();
                                }
                            }
                        }
                    }
                    return;
                }

                // The find bar owns the keyboard while it is open: typing goes
                // into the query, not the document.
                if self.focus == InputFocus::Find {
                    let shift = self.modifiers.contains(ModifiersState::SHIFT);
                    match keymap::resolve_find(&event.logical_key, shift) {
                        keymap::FindAction::Close => {
                            self.core.close_find();
                            self.after_state_change();
                        }
                        keymap::FindAction::Backspace => {
                            let mut query =
                                self.core.find_state().map(|f| f.query()).unwrap_or("").to_string();
                            query.pop();
                            self.core.set_find_query(query);
                            self.after_state_change();
                        }
                        keymap::FindAction::Next => {
                            self.core.find_next();
                            self.after_state_change();
                        }
                        keymap::FindAction::Previous => {
                            self.core.find_previous();
                            self.after_state_change();
                        }
                        keymap::FindAction::None => {
                            if let Some(text) = event.text.as_ref() {
                                let printable: String =
                                    text.chars().filter(|c| !c.is_control()).collect();
                                if !printable.is_empty() {
                                    let mut query = self
                                        .core
                                        .find_state()
                                        .map(|f| f.query())
                                        .unwrap_or("")
                                        .to_string();
                                    query.push_str(&printable);
                                    self.core.set_find_query(query);
                                    self.after_state_change();
                                }
                            }
                        }
                    }
                    return;
                }
                self.wake_caret();

                let modifiers = winit::event::Modifiers::from(self.modifiers);
                match keymap::resolve(&event.logical_key, &modifiers) {
                    Binding::Command(id, args) => {
                        if id != "app.quit" {
                            self.quit_confirm_pending = false;
                        }

                        self.run_command(id, args);
                        if self.take_exit() {
                            event_loop.exit();
                        }
                    }
                    Binding::InsertText => {
                        if let Some(text) = event.text.as_ref() {
                            // Control characters arrive as text on some layouts;
                            // the editor inserts printable text only.
                            let printable: String =
                                text.chars().filter(|c| !c.is_control()).collect();
                            if !printable.is_empty() {
                                self.quit_confirm_pending = false;
                                self.run_command("edit.insert_text", CommandArgs::Text(printable));
                            }
                        }
                    }
                    Binding::None => {
                        self.pending_input = None;
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.pending_input.get_or_insert_with(Instant::now);
                let (x, y) = self.pointer;
                let target = match self.last_layout {
                    Some(layout) => wheel_target(&layout, self.focus, x, y),
                    None => WheelTarget::Blocked,
                };
                match target {
                    // The tab bar and the status bar do not scroll the
                    // document, and an open menu or confirmation owns input.
                    WheelTarget::Chrome | WheelTarget::Blocked => return,
                    WheelTarget::Editor => {}
                }
                match delta {
                    MouseScrollDelta::LineDelta(dx, dy) => {
                        if dx.abs() > 0.0 {
                            let step = self
                                .last_layout
                                .map(|layout| layout.metrics.digit_width * 3.0)
                                .unwrap_or(24.0);
                            self.scroll_by_pixels(dx * step, 0.0);
                        }
                        self.scroll_by_lines(dy * WHEEL_LINES);
                    }
                    MouseScrollDelta::PixelDelta(position) => {
                        self.scroll_by_pixels(position.x as f32, position.y as f32);
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = (position.x as f32, position.y as f32);
                if self.menu.is_open() {
                    // The highlight follows the pointer down an open dropdown.
                    if let Some(geometry) = &self.menu_geometry {
                        let (x, y) = self.pointer;
                        let hovered = menu::hovered_item(geometry, x, y);
                        if hovered != self.menu.hovered_item {
                            self.menu.hovered_item = hovered;
                            self.request_redraw();
                        }
                    }
                    return;
                }
                if self.dragging_selection {
                    let (x, y) = self.pointer;
                    self.place_cursor_at(x, y, true);
                } else if self.dragging_scrollbar {
                    if let Some(layout) = self.last_layout {
                        let y = self.pointer.1;
                        self.scroll_to_scrollbar_position(y, &layout);
                    }
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                let (x, y) = self.pointer;
                match state {
                    ElementState::Pressed => {
                        self.pending_input.get_or_insert_with(Instant::now);
                        self.on_mouse_press(x, y);
                    }
                    ElementState::Released => {
                        self.dragging_selection = false;
                        self.dragging_scrollbar = false;
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.redraw();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::FontMetrics;
    use ls_platform::MemoryClipboard;

    fn layout() -> Layout {
        Layout::with_chrome(
            1000.0,
            700.0,
            1.0,
            FontMetrics { font_size: 14.0, line_height: 20.0, digit_width: 8.0 },
            4,
            true,
            true,
            false,
        )
    }

    fn centre(rect: crate::layout::Rect) -> (f32, f32) {
        (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0)
    }

    #[test]
    fn the_wheel_scrolls_the_editor_without_a_prior_click() {
        // The defect this encodes: the editor used to become the wheel target
        // only after a click had moved the caret. Focus starts on the editor,
        // so a freshly opened document scrolls straight away.
        let layout = layout();
        let (x, y) = centre(layout.text);
        assert_eq!(
            wheel_target(&layout, InputFocus::default(), x, y),
            WheelTarget::Editor,
            "the default focus is the editor"
        );
    }

    #[test]
    fn the_gutter_and_the_scrollbar_scroll_the_editor_too() {
        let layout = layout();
        for rect in [layout.gutter, layout.scrollbar] {
            let (x, y) = centre(rect);
            assert_eq!(wheel_target(&layout, InputFocus::Editor, x, y), WheelTarget::Editor);
        }
    }

    #[test]
    fn the_wheel_over_chrome_does_not_scroll_the_document() {
        let layout = layout();
        for rect in [layout.tab_bar, layout.menu_bar, layout.status_bar] {
            let (x, y) = centre(rect);
            assert_eq!(
                wheel_target(&layout, InputFocus::Editor, x, y),
                WheelTarget::Chrome,
                "chrome must not scroll the editor under it"
            );
        }
    }

    #[test]
    fn an_open_menu_or_a_confirmation_swallows_the_wheel() {
        let layout = layout();
        let (x, y) = centre(layout.text);
        assert_eq!(wheel_target(&layout, InputFocus::Menu, x, y), WheelTarget::Blocked);
        assert_eq!(wheel_target(&layout, InputFocus::Prompt, x, y), WheelTarget::Blocked);
    }

    #[test]
    fn a_newer_diagnostics_version_is_applied() {
        assert!(should_apply_lsp_diagnostics(Some(5), Some(6)));
        assert!(should_apply_lsp_diagnostics(None, Some(1)), "the first version ever seen applies");
    }

    #[test]
    fn an_older_diagnostics_version_is_dropped() {
        // The exact bug this exists to prevent: a slow re-analysis for
        // revision 41 finishing after revision 45's result already landed
        // must not overwrite it.
        assert!(!should_apply_lsp_diagnostics(Some(45), Some(41)));
    }

    #[test]
    fn the_same_version_reapplying_is_allowed() {
        // A server may republish the same analysis (e.g. after an unrelated
        // file changed); it is not older, so it is not rejected as stale.
        assert!(should_apply_lsp_diagnostics(Some(5), Some(5)));
    }

    #[test]
    fn a_server_that_never_sends_a_version_is_always_applied() {
        assert!(should_apply_lsp_diagnostics(Some(100), None));
    }

    // --- the file tree: one geometry-free row-builder, tested directly ------------

    fn scratch_workspace(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lightspeed-app-tree-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn editor_at(root: &Path) -> EditorCore {
        let mut core = EditorCore::with_clipboard(
            EffectiveConfig::default(),
            Box::new(MemoryClipboard::new()),
        );
        core.open_workspace(root).expect("open the scratch workspace");
        core
    }

    #[test]
    fn collapsed_directories_show_only_their_own_name() {
        let root = scratch_workspace("collapsed");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("README.md"), "").unwrap();
        let core = editor_at(&root);

        let mut rows = Vec::new();
        append_tree_level(&core, &std::collections::HashSet::new(), &root, 0, &mut rows);

        assert_eq!(rows.len(), 2, "one directory row, one file row, nothing from inside src/");
        let dir_row = rows.iter().find(|r| r.label.contains("src")).unwrap();
        assert!(
            dir_row.label.starts_with('\u{25b8}'),
            "a collapsed directory shows the closed glyph"
        );
        assert!(matches!(dir_row.action, Some(ListAction::ToggleDirectory(_))));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn expanding_a_directory_reveals_its_children_indented_one_level_deeper() {
        let root = scratch_workspace("expanded");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        let core = editor_at(&root);

        let mut expanded = std::collections::HashSet::new();
        expanded.insert(root.join("src"));
        let mut rows = Vec::new();
        append_tree_level(&core, &expanded, &root, 0, &mut rows);

        assert_eq!(rows.len(), 2, "the directory row, plus main.rs inside it");
        let dir_row = &rows[0];
        assert!(
            dir_row.label.starts_with('\u{25be}'),
            "an expanded directory shows the open glyph"
        );
        let file_row = &rows[1];
        assert!(file_row.label.contains("main.rs"));
        assert!(
            file_row.label.starts_with("    "),
            "a child is indented two steps deeper than its parent's zero: {:?}",
            file_row.label
        );
        assert!(
            matches!(&file_row.action, Some(ListAction::OpenFile(p)) if p.ends_with("main.rs"))
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_separate_branches_can_be_expanded_at_once() {
        // The whole point of a real tree over a drill-down browser: opening
        // one branch does not require closing another.
        let root = scratch_workspace("two-branches");
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::create_dir_all(root.join("beta")).unwrap();
        std::fs::write(root.join("alpha/a.rs"), "").unwrap();
        std::fs::write(root.join("beta/b.rs"), "").unwrap();
        let core = editor_at(&root);

        let mut expanded = std::collections::HashSet::new();
        expanded.insert(root.join("alpha"));
        expanded.insert(root.join("beta"));
        let mut rows = Vec::new();
        append_tree_level(&core, &expanded, &root, 0, &mut rows);

        assert_eq!(rows.len(), 4, "alpha, a.rs, beta, b.rs");
        assert!(rows.iter().any(|r| r.label.contains("a.rs")));
        assert!(rows.iter().any(|r| r.label.contains("b.rs")));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_collapsed_directory_never_touches_its_children_on_disk() {
        // Bounded by what is expanded, not by workspace size: a directory
        // that is not open must not even be read.
        let root = scratch_workspace("bounded");
        std::fs::create_dir_all(root.join("huge")).unwrap();
        for index in 0..50 {
            std::fs::write(root.join("huge").join(format!("f{index}.txt")), "").unwrap();
        }
        let core = editor_at(&root);

        let mut rows = Vec::new();
        append_tree_level(&core, &std::collections::HashSet::new(), &root, 0, &mut rows);
        assert_eq!(rows.len(), 1, "only the collapsed huge/ row itself, none of its 50 files");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_caret_asks_for_two_invalidations_a_second_and_no_more() {
        // The blink is the only thing on a clock. Each tick flips the caret and
        // pushes the deadline out by half a cycle, so a second of idling costs
        // two frames rather than sixty.
        let start = Instant::now();
        let mut visible = true;
        let mut deadline = start + CARET_BLINK;
        let mut flips = 0;

        // Simulate a second of loop wakeups without sleeping.
        let mut now = start;
        while now <= start + Duration::from_secs(1) {
            if now >= deadline {
                visible = !visible;
                deadline = now + CARET_BLINK;
                flips += 1;
            }
            now += Duration::from_millis(10);
        }
        assert_eq!(flips, 2, "one second of blinking is two invalidations");
        assert!(visible, "two flips is one whole cycle, back to solid");
    }

    #[test]
    fn focus_is_derived_from_the_surfaces_that_are_up() {
        // Focus cannot be left pointing at a menu that has since closed,
        // because it is recomputed rather than remembered.
        let layout = layout();
        let (x, y) = centre(layout.text);
        let mut focus = InputFocus::Menu;
        assert_eq!(wheel_target(&layout, focus, x, y), WheelTarget::Blocked);
        focus = InputFocus::Editor;
        assert_eq!(wheel_target(&layout, focus, x, y), WheelTarget::Editor);
    }
}
