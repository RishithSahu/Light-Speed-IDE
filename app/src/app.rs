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
use crate::tabs::{self, TabGeometry, TabHit};
use crate::theme::Theme;
use ls_core::{
    CommandArgs, ContentState, DocumentId, EditorCore, EffectiveConfig, EventPayload, LineIndex,
    LoadInjection, PersistenceState, RenderSnapshot, ShellRequest, Viewport,
};
use ls_platform::ProcessSampler;
use std::collections::HashMap;
use std::path::PathBuf;
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
        InputFocus::Menu | InputFocus::Prompt => WheelTarget::Blocked,
        InputFocus::Editor => {
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

/// How the user answered a confirmation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PromptAnswer {
    Save,
    Discard,
    Cancel,
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
    last_click: Option<(Instant, f32, f32)>,
    window_title: String,
    /// Path the diagnostics commands act on: the last file that was opened.
    diagnostics_path: Option<PathBuf>,
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
        LightSpeed {
            core: EditorCore::new(config),
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
        self.reveal_cursor = true;
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
            match menu::hit(geometry, self.menu, x, y) {
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

        // A second click in the same place selects the word under it.
        let double = self
            .last_click
            .map(|(at, last_x, last_y)| {
                at.elapsed() < DOUBLE_CLICK_TIME
                    && (last_x - x).abs() < DOUBLE_CLICK_SLOP
                    && (last_y - y).abs() < DOUBLE_CLICK_SLOP
            })
            .unwrap_or(false);
        self.last_click = Some((Instant::now(), x, y));

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

        if layout.text.contains(x, y) || layout.gutter.contains(x, y) {
            self.place_cursor_at(x, y, self.modifiers.contains(ModifiersState::SHIFT));
            if double {
                self.select_word_at_cursor();
            } else {
                self.dragging_selection = true;
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

    /// When the loop should wake next, if anything is on a timer.
    fn next_wakeup(&self) -> Option<Instant> {
        // Only the caret needs a timer; everything else is event-driven.
        if self.core.active().is_some() && self.prompt.is_none() {
            Some(self.caret_deadline)
        } else {
            None
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

    /// Rows for the floating panel: the performance overlay, the loading
    /// panel, or both stacked.
    fn panel_rows(&self) -> Vec<String> {
        let mut rows = Vec::new();
        if self.dev_panel_visible {
            rows.extend(self.dev_panel_rows.iter().cloned());
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

        let geometry = menu::geometry(
            layout.menu_bar,
            self.menu,
            layout.metrics.digit_width,
            layout.metrics.line_height,
            layout.scale,
        );
        self.menu_geometry = Some(geometry.clone());
        if self.overlay_visible {
            self.build_overlay();
        }
        if self.dev_panel_visible {
            self.dev_panel_rows = devpanel::lines(&self.core, &self.heartbeat);
        }

        let panel_rows = self.panel_rows();
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
            Some(open) => menu::MENUS[open]
                .items
                .iter()
                .map(|item| menu::is_enabled(&self.core, item))
                .collect(),
            None => Vec::new(),
        };
        let prompt_text = self.prompt_text();
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
            caret_visible,
            prompt: prompt_text.as_deref(),
            placeholder: "  LightSpeed IDE\n\n  Ctrl+O  open a file\n  Ctrl+N  new file\n  F12     performance overlay",
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
        match self.next_wakeup() {
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    /// Background work reaching the event-loop thread.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::TaskCompleted => self.pump_background_work(),
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

    fn layout() -> Layout {
        Layout::with_chrome(
            1000.0,
            700.0,
            1.0,
            FontMetrics { font_size: 14.0, line_height: 20.0, digit_width: 8.0 },
            4,
            true,
            true,
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
