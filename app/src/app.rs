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

/// How long a load has to still be running before its tab is actually shown.
///
/// Without this, opening a file that fails almost instantly -- a binary file
/// being rejected, say, which typically fails within the first few KB --
/// makes a tab flash into existence and vanish again within milliseconds.
/// That reads as a glitch, not as "the file was checked and rejected," even
/// though the real error is reported correctly (see `status_left`). A load
/// that finishes well inside this window, success or failure, never shows a
/// loading tab at all; one that is still running past it shows the normal
/// "Loading..." tab exactly as before.
const LOADING_TAB_GRACE: Duration = Duration::from_millis(120);

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

/// How long typing has to pause before the workspace search actually runs.
///
/// Results follow the query as it is typed, which is only affordable because
/// of this: without it, "needle" is six full workspace walks, five of them
/// for prefixes nobody asked about. At a typical typing cadence this fires
/// once per word rather than once per letter, and the walk that *is* running
/// when the next keystroke lands is cancelled mid-flight
/// (`workspace_search::search_cancellable`) rather than left to finish.
///
/// 150ms is the usual sweet spot: below a fast typist's inter-key gap (so it
/// does not fire mid-word), and short enough that the results feel attached
/// to the typing rather than trailing it.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

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
    /// The command palette owns the keyboard until it runs a command or is
    /// dismissed.
    CommandPalette,
}

/// Where a wheel event should go.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WheelTarget {
    /// Scroll the active document.
    Editor,
    /// Scroll the sidebar's row list.
    Sidebar,
    /// Scroll back through the terminal's output.
    BottomPanel,
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
/// pointer over the sidebar's row list      -> the sidebar scrolls
/// pointer over tab bar or menu bar         -> nothing scrolls
/// a dropdown or a confirmation is up       -> nothing scrolls
/// ```
pub fn wheel_target(layout: &Layout, focus: InputFocus, x: f32, y: f32) -> WheelTarget {
    // Checked ahead of `focus`: the sidebar is non-modal chrome, not a
    // surface that claims the wheel only while it holds keyboard focus --
    // scrolling it while the caret is still in the editor (or the list has
    // just been clicked into, which is `InputFocus::List`) is the whole
    // point of it staying open non-modally. `layout.sidebar` is a
    // zero-width rect when the panel is hidden, so this never fires then.
    if layout.sidebar.contains(x, y) {
        return WheelTarget::Sidebar;
    }
    // Same reasoning for the terminal: scrolling back through output is not
    // something that should require clicking into the panel first, and it
    // must work while `InputFocus::Terminal` holds the keyboard rather than
    // being swallowed by the "a modal surface owns input" arm below.
    // `layout.bottom_panel` is empty when the panel is hidden.
    if layout.bottom_panel_visible && layout.bottom_panel.contains(x, y) {
        return WheelTarget::BottomPanel;
    }
    match focus {
        InputFocus::Menu
        | InputFocus::Prompt
        | InputFocus::SearchQuery
        | InputFocus::Terminal
        | InputFocus::CommandPalette => WheelTarget::Blocked,
        InputFocus::List => WheelTarget::Chrome,
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
    use crate::icons::Icon;
    use crate::theme::SidebarRowKind;
    match core.workspace().enumerate_children(dir) {
        Ok(entries) => {
            for entry in entries {
                match entry.kind {
                    ls_core::EntryKind::Directory => {
                        let is_expanded = expanded.contains(&entry.path);
                        rows.push(ListRow {
                            label: entry.name.clone(),
                            action: Some(ListAction::ToggleDirectory(entry.path.clone())),
                            depth,
                            chevron: Some(if is_expanded {
                                Icon::ChevronDown
                            } else {
                                Icon::ChevronRight
                            }),
                            icon: Some(
                                if is_expanded { Icon::FolderOpened } else { Icon::Folder }.into(),
                            ),
                            icon_color: None,
                            kind: SidebarRowKind::Directory,
                        });
                        if is_expanded {
                            append_tree_level(core, expanded, &entry.path, depth + 1, rows);
                        }
                    }
                    _ => {
                        let file_icon = crate::icons::icon_for_file(&entry.name);
                        rows.push(ListRow {
                            label: entry.name.clone(),
                            action: Some(ListAction::OpenFile(entry.path.clone())),
                            depth,
                            chevron: None,
                            icon: Some(file_icon.into()),
                            icon_color: Some(file_icon.color()),
                            kind: SidebarRowKind::File,
                        });
                    }
                }
            }
        }
        Err(error) => {
            let mut row = ListRow::message(error.to_string());
            row.depth = depth;
            rows.push(row);
        }
    }
}

/// Whether a still-loading tab has been running long enough to actually show,
/// rather than flash and vanish for a load that fails (or succeeds) almost
/// instantly. A pure function for the same reason `wheel_target` is: the
/// debounce rule is worth asserting on directly rather than only observing it
/// through a flickering window.
pub fn should_show_loading_tab(started: Instant, now: Instant) -> bool {
    now.saturating_duration_since(started) >= LOADING_TAB_GRACE
}

/// Derives the next input focus from what is currently focused and which
/// surfaces are up. A pure function for the same reason `wheel_target` is:
/// this is the exact rule behind a real bug (opening a file from the
/// explorer was typeable for exactly one keystroke -- see `refresh_focus`'s
/// doc comment for the full story), so it deserves a name and a direct test
/// rather than being something only observed through a flickering window.
///
/// Prompt, Menu, SearchQuery and Find are exclusive: any one of them open
/// wins outright. Editor, List and Terminal are "resting" focuses that can
/// coexist with their panel being visible or not -- `current` is only kept
/// at List or Terminal if it was *already* there and that panel is still up;
/// entering one of them for the first time is always an explicit choice a
/// caller makes (`focus_list`/`focus_terminal`), never something derived
/// here from visibility alone.
#[allow(clippy::too_many_arguments)]
pub fn derive_focus(
    current: InputFocus,
    prompt_open: bool,
    menu_open: bool,
    search_query_open: bool,
    find_open: bool,
    list_open: bool,
    terminal_visible: bool,
    command_palette_open: bool,
) -> InputFocus {
    if prompt_open {
        InputFocus::Prompt
    } else if command_palette_open {
        InputFocus::CommandPalette
    } else if menu_open {
        InputFocus::Menu
    } else if find_open {
        InputFocus::Find
    // A typed workspace-search query is a resting focus, not an exclusive
    // one, the same as List/Terminal below and for the same bug this fixes:
    // `search_query_open` alone used to force `SearchQuery` back on every
    // refresh (a diagnostics update, a heartbeat tick, anything routed
    // through `after_state_change`), so a click into the editor -- which
    // sets `focus` directly rather than through here -- got silently
    // reverted the moment anything else refreshed. It reads as the editor
    // being stuck until Escape, because functionally it was. Entering
    // `SearchQuery` is still an explicit act (`open_search_query`); this
    // only stops it from being *re*-entered against the user's own click.
    } else if current == InputFocus::SearchQuery && search_query_open {
        InputFocus::SearchQuery
    } else if current == InputFocus::List && list_open {
        InputFocus::List
    } else if current == InputFocus::Terminal && terminal_visible {
        InputFocus::Terminal
    } else {
        InputFocus::Editor
    }
}

/// How many lines of terminal output fit in the bottom panel, after the
/// header and the input line have taken their rows.
///
/// A pure function of the layout for the same reason `wheel_target` is: the
/// scroll clamp and the row builder have to agree on this number exactly, or
/// scrolling either stops short of the end or runs past it.
pub fn terminal_visible_lines(layout: &Layout) -> usize {
    const CHROME_ROWS: usize = 2; // the header, and the input line
    let line_height = layout.metrics.line_height.max(1.0);
    let rows = (layout.bottom_panel.height / line_height).floor() as usize;
    rows.saturating_sub(CHROME_ROWS).max(1)
}

/// The byte offset in `line` for character index `cursor`, clamped to the
/// line's own length in characters.
///
/// Every terminal-input edit needs this: `String` operations (`insert_str`,
/// `replace_range`) are byte-indexed, but a cursor is inherently a character
/// position -- someone arrowing past an accented letter or an emoji has moved
/// one character, not however many bytes it happens to encode as. Splicing
/// at a raw byte offset derived any other way risks landing mid-character,
/// which is not a rare-input edge case here so much as the ordinary case for
/// text that is not pure ASCII.
fn terminal_char_boundary(line: &str, cursor: usize) -> usize {
    line.char_indices().nth(cursor).map(|(byte, _)| byte).unwrap_or(line.len())
}

/// Builds the terminal's live input row with a visible cursor spliced in at
/// character index `cursor`.
///
/// A pure function of the same shape as `terminal_visible_lines`, and worth
/// pulling out for the same reason: this is the fix for "the cursor is not
/// visible in the terminal" landing somewhere it can be asserted on
/// directly rather than only eyeballed in a screenshot. The block glyph is
/// spliced into the plain-text row itself rather than drawn as a separate,
/// distinctly colored overlay, because `Region::BottomPanel` is rendered
/// with `set_text` -- one color for the whole region, no per-character
/// spans the way `RichText` gives the sidebar and the command palette their
/// colored icons and highlights (see `text::TextEngine::set_rich_text`).
pub fn terminal_input_row(input: &str, cursor: usize) -> String {
    let byte = terminal_char_boundary(input, cursor);
    let mut row = String::with_capacity(input.len() + "> ".len() + '\u{2588}'.len_utf8());
    row.push_str("> ");
    row.push_str(&input[..byte]);
    row.push('\u{2588}');
    row.push_str(&input[byte..]);
    row
}

/// Which way the terminal's command history is being walked.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HistoryDirection {
    /// Up: toward commands run further in the past.
    Older,
    /// Down: back toward the live input line.
    Newer,
}

/// Resolves one Up/Down press against the command history, returning the
/// index it lands on (`None` means back at the live input line, not
/// recalling anything).
///
/// A pure function for the same reason `wheel_target` is: recall has to
/// agree exactly with what `terminal_command_history` actually holds, or
/// pressing Up either skips the newest command or walks one entry past the
/// oldest into a panic. `current` is `None` while the live line is showing
/// (a fresh prompt, or Down walked past the newest entry back to it).
///
/// Older never runs off the front (it just stops at index `0`, the oldest
/// entry, the way a real shell's history does); Newer past the newest entry
/// returns to `None` rather than wrapping.
pub fn navigate_terminal_history(
    history: &[String],
    current: Option<usize>,
    direction: HistoryDirection,
) -> Option<usize> {
    if history.is_empty() {
        return None;
    }
    match (direction, current) {
        (HistoryDirection::Older, None) => Some(history.len() - 1),
        (HistoryDirection::Older, Some(index)) => Some(index.saturating_sub(1)),
        (HistoryDirection::Newer, Some(index)) if index + 1 < history.len() => Some(index + 1),
        (HistoryDirection::Newer, _) => None,
    }
}

/// What a click on a sidebar row acts on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SidebarClick {
    /// The Search panel's own header, which is a text field rather than a
    /// caption: the click belongs to the field, not to the list.
    SearchField,
    /// List entry `0`-based `index`, below the header.
    Row(usize),
    /// An ordinary panel's title line. Nothing to act on.
    Header,
}

/// Resolves a sidebar click by row, given how many header lines the panel
/// has and whether it is the Search panel.
///
/// A pure function for the same reason `wheel_target` is, and for a bug worth
/// pinning down: the Search panel's header is *interactive*, which no other
/// panel's is. Treating "the click is in the header" as "the click does
/// nothing to the list, so claim list focus and move on" is what made
/// clicking the search box silently take the keyboard away from it.
pub fn sidebar_click(row: usize, header_lines: usize, is_search_panel: bool) -> SidebarClick {
    if row >= header_lines {
        return SidebarClick::Row(row - header_lines);
    }
    if is_search_panel {
        SidebarClick::SearchField
    } else {
        SidebarClick::Header
    }
}

/// Whether a workspace-search result that has landed is for the query in the
/// search field right now.
///
/// A pure function for the same reason `wheel_target` is: this is the rule
/// that keeps a panel updating as you type from showing the wrong thing.
/// Searches run on a worker against a query captured when they were
/// dispatched, so the result in hand is routinely for a prefix of what has
/// since been typed -- "nee" landing under a field that already reads
/// "needle". `typed` is `None` once the field is dismissed but the results
/// are still on screen, where whatever last completed is what to show.
pub fn search_result_is_current(result_query: &str, typed: Option<&str>) -> bool {
    match typed {
        Some(query) => result_query == query,
        None => true,
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

/// Whether `(x, y)` is on the sidebar's resize grip: a strip straddling its
/// right edge, wide enough to grab without pixel-perfect aim but narrow
/// enough not to steal clicks meant for the last column of list rows.
pub fn sidebar_grip_hit(layout: &Layout, x: f32, y: f32) -> bool {
    if !layout.sidebar_visible {
        return false;
    }
    let half = (crate::layout::SIDEBAR_GRIP_WIDTH * layout.scale) / 2.0;
    let border = layout.sidebar.right();
    x >= border - half && x <= border + half && y >= layout.sidebar.y && y < layout.sidebar.bottom()
}

/// Whether `(x, y)` is on the bottom panel's resize grip: a strip straddling
/// its top edge. Same shape as [`sidebar_grip_hit`], on the vertical axis.
pub fn bottom_panel_grip_hit(layout: &Layout, x: f32, y: f32) -> bool {
    if !layout.bottom_panel_visible {
        return false;
    }
    let half = (crate::layout::BOTTOM_PANEL_GRIP_HEIGHT * layout.scale) / 2.0;
    let border = layout.bottom_panel.y;
    y >= border - half
        && y <= border + half
        && x >= layout.activity_bar.right()
        && x < layout.window.right()
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
    /// Switches to an already-open document, from the Open Editors section.
    ActivateTab(DocumentId),
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
    /// Nesting depth, in tree levels. Indentation is applied when the row is
    /// composed for drawing, not baked into `label`, so hit testing and the
    /// text stay independent of each other.
    depth: usize,
    /// The expand/collapse chevron, for rows that have children.
    chevron: Option<crate::icons::Icon>,
    /// The row's own icon: a folder (chrome glyph) or a file (Material
    /// Design Icons glyph, by extension) -- either converts to a `Glyph`, so
    /// this field does not need to know which.
    icon: Option<crate::icons::Glyph>,
    /// Overrides the row-kind-based default icon color set in
    /// `sidebar_rows` -- used for file icons, which each carry their own
    /// characteristic color the way `material-icon-theme` colors them,
    /// unlike the folder glyph or header chevron, which follow the row's
    /// kind instead.
    icon_color: Option<crate::theme::Color>,
    kind: crate::theme::SidebarRowKind,
}

impl ListRow {
    /// A row with no icons and no action: a message, or a section header.
    fn message(label: impl Into<String>) -> Self {
        ListRow {
            label: label.into(),
            action: None,
            depth: 0,
            chevron: None,
            icon: None,
            icon_color: None,
            kind: crate::theme::SidebarRowKind::Info,
        }
    }

    fn with_kind(mut self, kind: crate::theme::SidebarRowKind) -> Self {
        self.kind = kind;
        self
    }

    fn with_icon(mut self, icon: impl Into<crate::icons::Glyph>) -> Self {
        self.icon = Some(icon.into());
        self
    }
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

/// The persistent activity bar's icons, top to bottom -- Explorer, Search,
/// Source Control, Extensions, Debug, matching Lapce's own default order,
/// then Dependencies, which is ours.
const ACTIVITY_ICONS: [crate::icons::Icon; 6] = [
    crate::icons::Icon::Files,
    crate::icons::Icon::Search,
    crate::icons::Icon::SourceControl,
    crate::icons::Icon::Extensions,
    crate::icons::Icon::Debug,
    crate::icons::Icon::TypeHierarchy,
];

/// Which activity-bar row is wired to a real action. `Extensions` and
/// `Debug` have no subsystem behind them -- they render dimmed and inert,
/// present for layout fidelity rather than faking a feature that does not
/// exist.
const ACTIVITY_EXPLORER: usize = 0;
const ACTIVITY_SEARCH: usize = 1;
const ACTIVITY_SOURCE_CONTROL: usize = 2;
/// The dependency view. Its scan is expensive enough to be worth not doing
/// on startup, so it runs on this click and nowhere else.
const ACTIVITY_DEPENDENCIES: usize = 5;

/// Which settings file the screen is editing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SettingsScope {
    /// This person's own settings, which follow them between projects.
    User,
    /// This project's settings, committed with its code.
    Workspace,
}

/// A press being held over the dependency graph.
#[derive(Copy, Clone, Debug)]
struct DependencyPress {
    /// Where the button went down. Never moves, so the release can tell a
    /// click from a drag.
    origin: (f32, f32),
    /// Where the pointer has reached, so each motion pans by its own step.
    at: (f32, f32),
    /// The node the press started on, if any.
    node: Option<usize>,
}

/// How much one notch of the wheel zooms the dependency graph. Chosen so a
/// notch is a noticeable step and about eight of them cross the whole range.
const ZOOM_PER_NOTCH: f32 = 1.25;

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
/// Not `Copy`/`Eq` any more: `FolderPicked` carries an owned answer rather
/// than being a bare wake-up signal like the rest.
#[derive(Debug)]
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
    /// The folder picker closed. Carries the answer directly, since unlike
    /// the events above there is no shared buffer to drain -- one dialog
    /// produces one path (or `None` for cancelled).
    FolderPicked(Box<Result<Option<PathBuf>, ls_platform::PlatformError>>),
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
    /// Whether the settings screen has taken over the editor area.
    settings_open: bool,
    /// Settings this person chose, wherever they are.
    user_settings: ls_core::settings::Settings,
    /// Settings this project chose, committed alongside its code.
    workspace_settings: ls_core::settings::Settings,
    /// The two merged, which is what the rest of the shell reads.
    settings: ls_core::settings::Settings,
    /// Which file the screen is editing.
    settings_scope: SettingsScope,
    /// What has been typed into the search box.
    settings_query: String,
    /// The section picked on the left, or `None` for all of them.
    settings_section: Option<usize>,
    /// How far the list is scrolled, in rows.
    settings_scroll: usize,
    /// The field being typed into, and what has been typed so far. Held as a
    /// draft rather than written through on every keystroke, so a half-typed
    /// number never reaches the rest of the application.
    settings_editing: Option<(&'static str, String)>,
    /// Whether the search box has the keyboard.
    settings_search_focused: bool,
    /// The screen as last measured, so a click resolves against exactly what
    /// was drawn.
    settings_screen: crate::settings_ui::Screen,
    /// Whether the dependency view has taken over the editor area.
    dependency_view: bool,
    /// The settled simulation: who imports whom and where each file rests.
    /// Expensive to produce and independent of the window, so this is what
    /// is cached on disk between sessions.
    dependency_settled: Option<crate::depgraph::Settled>,
    /// The settled graph fitted to the current pane. Cheap, and rebuilt
    /// whenever the window resizes.
    dependency_scene: Option<crate::depgraph::Scene>,
    /// Where the reader has panned and zoomed to.
    dependency_view_at: crate::depgraph::View,
    /// The node under the pointer, traced along its edges.
    dependency_hovered: Option<usize>,
    /// A press being held over the graph: where it started, where the
    /// pointer has reached, and which node it began on. The origin is kept
    /// apart from the running position because a drag that ends somewhere
    /// else is a pan, and only a press that stayed put opens a file.
    dependency_press: Option<DependencyPress>,
    /// Whether the graph on screen came off the disk rather than a scan.
    dependency_from_cache: bool,
    dev_panel_visible: bool,
    dev_panel_rows: Vec<String>,
    resource_center_visible: bool,
    resource_center_rows: Vec<String>,
    heartbeat: Heartbeat,
    menu: MenuState,
    menu_geometry: Option<menu::MenuGeometry>,
    /// Whether the command palette is open, and what it is doing while it
    /// is: `command_palette_query` is what has been typed so far,
    /// `command_palette_selected` is the highlighted row (by index into the
    /// filtered list, recomputed fresh each frame rather than stored), and
    /// `command_palette_hovered` mirrors `sidebar_hovered_row`'s role for
    /// the sidebar.
    /// When the debounced workspace search should actually run, if a query
    /// has been typed since the last one was dispatched. `None` means there
    /// is nothing waiting -- the event loop only wakes for this when it is
    /// `Some` (see `next_wakeup`), so an idle search panel costs nothing.
    search_debounce_deadline: Option<Instant>,
    /// Whether a folder picker is already on screen. It runs on its own
    /// thread now, so unlike a modal dialog nothing else stops a second
    /// request from arriving while the first is still open.
    folder_picker_open: bool,
    command_palette_open: bool,
    command_palette_query: String,
    command_palette_selected: usize,
    command_palette_hovered: Option<usize>,
    /// The palette's own floating panel rectangle for the frame on screen,
    /// the same reason `tab_geometry` is stored rather than recomputed at
    /// click time: drawing and hit testing must agree on one rectangle.
    palette_geometry: Option<crate::layout::Rect>,
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
    /// When each in-flight load was requested, so its tab can stay hidden
    /// until `LOADING_TAB_GRACE` has passed (see `should_show_loading_tab`).
    /// Pruned once a document is no longer loading.
    loading_tab_started: HashMap<DocumentId, Instant>,
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
    /// The sidebar's width in logical pixels, dragged by the user via the
    /// grip on its right edge. Persists across toggling the sidebar off and
    /// on, the way VS Code remembers explorer width.
    sidebar_width: f32,
    /// Set while the user is dragging the sidebar's resize grip.
    dragging_sidebar: bool,
    /// Whether the resize cursor is currently shown for the sidebar's grip,
    /// so it is only changed (a real OS call) on an actual transition.
    sidebar_grip_hovered: bool,
    /// Which sidebar row (by the same numbering as `list_selected` plus the
    /// header, see `sidebar_selected_row`) the pointer is currently over, for
    /// a hover highlight distinct from the selection.
    sidebar_hovered_row: Option<usize>,
    /// How far the sidebar's row list is scrolled, in logical pixels. Its own
    /// state rather than a derived value, the same reason the editor's
    /// `scroll_y` lives on `EditorView` instead of being recomputed from the
    /// caret every frame -- a list taller than the panel needs somewhere to
    /// remember where the user left it.
    sidebar_scroll_y: f32,
    /// The bottom panel's height in logical pixels, dragged via the grip on
    /// its top edge. Same shape as `sidebar_width`, on the vertical axis.
    bottom_panel_height: f32,
    dragging_bottom_panel: bool,
    bottom_panel_grip_hovered: bool,
    /// Which activity-bar row the pointer is over, if any.
    activity_hovered: Option<usize>,
    /// The workspace-search query as it is being typed, before Enter submits
    /// it. `None` when the search bar is not open.
    search_query_input: Option<String>,
    /// A line to jump to once the file a list row just requested finishes
    /// opening (it may still be loading asynchronously).
    pending_jump: Option<(PathBuf, usize)>,
    terminal: Option<crate::terminal::Terminal>,
    terminal_visible: bool,
    terminal_scrollback: String,
    /// How far the terminal view is scrolled back, in lines up from the
    /// newest output. `0` means pinned to the bottom, which is where it sits
    /// unless the user deliberately scrolls away -- so output written while
    /// they are reading history does not yank the view out from under them.
    terminal_scroll_lines: usize,
    terminal_input: String,
    /// Where the caret sits in `terminal_input`, as a character index (never
    /// a byte offset -- see `terminal_char_boundary`). `0` is before the
    /// first character; `terminal_input.chars().count()` is past the last
    /// one, which is where a fresh or freshly-recalled line leaves it.
    terminal_cursor: usize,
    /// Commands run this session, oldest first, for the Up/Down recall
    /// `terminal_history_index` walks. Separate from the *permanent*
    /// transcript (`ls_platform::terminal_log`, written from
    /// `Terminal::send_line`/`drain_output`): that one is an unbounded,
    /// append-only record meant for reading later outside the editor; this
    /// one is a bounded, in-memory list meant for recall inside it, the same
    /// distinction a real shell draws between its history file and what
    /// pressing Up walks through in the current session.
    terminal_command_history: Vec<String>,
    /// Which entry of `terminal_command_history` Up/Down has navigated to,
    /// counting from the oldest. `None` means the input line is live text
    /// the user is typing, not a recalled command -- the state a fresh
    /// prompt and a Down-arrowed-past-the-newest-entry prompt share.
    terminal_history_index: Option<usize>,
    /// What was being typed before the first Up press, restored if Down
    /// walks back past the newest history entry to a live line again.
    terminal_history_draft: String,
    /// The running language servers, one per language, started on demand.
    /// Previously a single shared client, which meant the first recognized
    /// document's server received every later document too, whatever
    /// language it was.
    lsp: crate::lsp::LspManager,
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
            settings_open: false,
            user_settings: ls_core::settings::Settings::new(),
            workspace_settings: ls_core::settings::Settings::new(),
            settings: ls_core::settings::Settings::new(),
            settings_scope: SettingsScope::User,
            settings_query: String::new(),
            settings_section: None,
            settings_scroll: 0,
            settings_editing: None,
            settings_search_focused: false,
            settings_screen: crate::settings_ui::Screen::default(),
            dependency_view: false,
            dependency_settled: None,
            dependency_scene: None,
            dependency_view_at: crate::depgraph::View::default(),
            dependency_hovered: None,
            dependency_press: None,
            dependency_from_cache: false,
            dev_panel_visible: false,
            dev_panel_rows: Vec::new(),
            resource_center_visible: false,
            resource_center_rows: Vec::new(),
            heartbeat: Heartbeat::new(),
            menu: MenuState::default(),
            menu_geometry: None,
            search_debounce_deadline: None,
            folder_picker_open: false,
            command_palette_open: false,
            command_palette_query: String::new(),
            command_palette_selected: 0,
            command_palette_hovered: None,
            palette_geometry: None,
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
            loading_tab_started: HashMap::new(),
            active_list: None,
            file_tree_root: None,
            expanded_dirs: std::collections::HashSet::new(),
            list_selected: 0,
            sidebar_width: crate::layout::SIDEBAR_WIDTH,
            dragging_sidebar: false,
            sidebar_grip_hovered: false,
            sidebar_hovered_row: None,
            sidebar_scroll_y: 0.0,
            bottom_panel_height: crate::layout::BOTTOM_PANEL_HEIGHT,
            dragging_bottom_panel: false,
            bottom_panel_grip_hovered: false,
            activity_hovered: None,
            search_query_input: None,
            pending_jump: None,
            terminal: None,
            terminal_visible: false,
            terminal_scrollback: String::new(),
            terminal_scroll_lines: 0,
            terminal_input: String::new(),
            terminal_cursor: 0,
            terminal_command_history: Vec::new(),
            terminal_history_index: None,
            terminal_history_draft: String::new(),
            lsp: crate::lsp::LspManager::default(),
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

        // A finished scan is settled and saved once, here, rather than on
        // every frame that notices it.
        if self.dependency_view
            && self.dependency_settled.is_none()
            && !self.core.is_dependency_graph_pending()
            && self.core.dependency_graph().is_some()
        {
            self.adopt_dependency_scan();
        }

        // Once a load is no longer in flight (loaded, failed, or cancelled)
        // its grace-period clock has done its job.
        let core = &self.core;
        self.loading_tab_started.retain(|id, _| core.is_loading(*id));

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
                ShellRequest::ToggleSettings => self.toggle_settings(),
                ShellRequest::RefreshDependencyView => self.refresh_dependency_view(),
                ShellRequest::ToggleDependencyView => {
                    let active = self.activity_active_index() == Some(ACTIVITY_DEPENDENCIES);
                    self.toggle_dependency_view(active);
                }
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
        // `lightspeed .` is an ordinary thing to type, and a directory is not
        // a document: opening it as the workspace is what was meant. Without
        // this it reported "... is a directory" and started on an empty
        // buffer with no workspace at all.
        if path.is_dir() {
            self.open_workspace_at(path);
            return;
        }
        match self.core.request_open_document_with(&path, injection) {
            Ok(request) => {
                // A joined request reuses a document already loading, whose
                // start time is already tracked; only a genuinely new load
                // starts the grace-period clock.
                self.loading_tab_started.entry(request.document).or_insert_with(Instant::now);
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

    /// Scrolls the sidebar's row list rather than the document -- the same
    /// shape as `scroll_by_pixels`, clamped against the sidebar's own row
    /// count instead of the document's line count.
    fn scroll_sidebar_by_pixels(&mut self, dy: f32) {
        let Some(layout) = self.last_layout else { return };
        let sidebar_rich = self.sidebar_rows(&layout);
        let total_height =
            (sidebar_rich.text.matches('\n').count() + 1) as f32 * layout.metrics.line_height;
        let max_scroll = (total_height - layout.sidebar.height).max(0.0);
        self.sidebar_scroll_y = (self.sidebar_scroll_y - dy).clamp(0.0, max_scroll);
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

        // The palette is drawn over everything else too, and gets the click
        // before anything underneath it does.
        if self.command_palette_open {
            let hit = self
                .palette_geometry
                .and_then(|panel| crate::palette::row_hit(panel, layout.metrics.line_height, x, y));
            match hit {
                Some(index) => {
                    self.command_palette_selected = index;
                    self.run_palette_selection();
                }
                None => self.close_command_palette(),
            }
            return;
        }

        if self.prompt.is_some() {
            // The confirmation owns input until it is answered.
            return;
        }

        // Clicking the title bar's own command field opens the palette --
        // Lapce's primary entry point for it, the search-box look it already
        // had before this was wired to anything.
        if layout.title_search.contains(x, y) {
            self.open_command_palette();
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

        if sidebar_grip_hit(&layout, x, y) {
            self.dragging_sidebar = true;
            return;
        }

        if bottom_panel_grip_hit(&layout, x, y) {
            self.dragging_bottom_panel = true;
            return;
        }

        if let Some(index) =
            crate::layout::icon_rail_hit(layout.activity_bar, ACTIVITY_ICONS.len(), x, y)
        {
            self.activate_activity_item(index);
            return;
        }

        // The settings screen and the graph both stand in for the document,
        // so they take presses over the editor area before the editor's own
        // handling can place a caret in a document nobody can see.
        if layout.text.contains(x, y) && self.press_settings(x, y) {
            return;
        }
        if self.press_dependency_view(x, y, &layout) {
            return;
        }

        if layout.bottom_panel_visible && layout.bottom_panel_rail.contains(x, y) {
            self.toggle_terminal();
            return;
        }

        if layout.bottom_panel_visible && layout.bottom_panel.contains(x, y) {
            self.focus_terminal();
            return;
        }

        if layout.title_actions.contains(x, y) {
            // The right-hand cluster is Run and then the gear. Only the gear
            // does anything today, and it is the second of the two.
            if x >= layout.title_actions.x + layout.title_actions.width / 2.0 {
                self.toggle_settings();
                return;
            }
        }

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
            // Rows before `sidebar_header_lines()` are the panel's own
            // header -- a single title line normally, or the Search panel's
            // title/field/replace block -- drawn but not a list entry.
            let header_lines = self.sidebar_header_lines();
            let row = ((y - layout.sidebar.y + self.sidebar_scroll_y) / layout.metrics.line_height)
                as usize;

            match sidebar_click(row, header_lines, self.showing_search_panel()) {
                // The Search panel's header is a text field, not a caption:
                // clicking it has to put the keyboard *into* it. Claiming
                // list focus here instead is exactly what made clicking the
                // search box appear to do nothing -- the panel was open, the
                // caret was drawn, and every keystroke went to the results.
                SidebarClick::SearchField => {
                    self.focus_search_field();
                }
                SidebarClick::Header => {
                    self.focus_list();
                }
                SidebarClick::Row(index) => {
                    // Clicking a row claims the panel's focus outright, the
                    // same as clicking a tab claims the editor's -- otherwise
                    // a click on a row while the editor happened to have
                    // focus would select the row but leave the keyboard
                    // pointed at the document.
                    self.focus_list();
                    if index < self.list_rows().len() {
                        self.list_selected = index;
                        self.activate_list_selection();
                    }
                }
            }
            return;
        }

        if layout.text.contains(x, y) || layout.gutter.contains(x, y) {
            // Claiming the editor's focus here, rather than leaving it to be
            // derived later, is what lets a click reclaim the keyboard from
            // the sidebar or terminal while either stays visibly open.
            self.focus = InputFocus::Editor;
            self.place_cursor_at(x, y, self.modifiers.contains(ModifiersState::SHIFT));
            match click_count {
                2 => self.select_word_at_cursor(),
                3 => self.select_line_at_cursor(),
                _ => self.dragging_selection = true,
            }
            self.wake_caret();
        }
    }

    /// Tracks the two things the sidebar cares about on plain pointer
    /// movement (no button down): the resize cursor over its grip, and which
    /// row -- if any -- is under the pointer, for a hover highlight.
    fn update_pointer_interaction(&mut self, layout: &Layout) {
        let sidebar_grip_hovered = sidebar_grip_hit(layout, self.pointer.0, self.pointer.1);
        let bottom_panel_grip_hovered =
            bottom_panel_grip_hit(layout, self.pointer.0, self.pointer.1);

        if sidebar_grip_hovered != self.sidebar_grip_hovered
            || bottom_panel_grip_hovered != self.bottom_panel_grip_hovered
        {
            self.sidebar_grip_hovered = sidebar_grip_hovered;
            self.bottom_panel_grip_hovered = bottom_panel_grip_hovered;
            if let Some(window) = self.window.as_ref() {
                let icon = if sidebar_grip_hovered {
                    winit::window::CursorIcon::ColResize
                } else if bottom_panel_grip_hovered {
                    winit::window::CursorIcon::RowResize
                } else {
                    winit::window::CursorIcon::Default
                };
                window.set_cursor(icon);
            }
        }

        let (x, y) = self.pointer;
        let any_grip_hovered = sidebar_grip_hovered || bottom_panel_grip_hovered;
        let hovered_row = if layout.sidebar_visible
            && layout.sidebar.contains(x, y)
            && !any_grip_hovered
            && self.active_list.is_some()
        {
            let header_lines = self.sidebar_header_lines();
            let row = ((y - layout.sidebar.y + self.sidebar_scroll_y) / layout.metrics.line_height)
                as usize;
            (row >= header_lines && row - header_lines < self.list_rows().len()).then_some(row)
        } else {
            None
        };
        if hovered_row != self.sidebar_hovered_row {
            self.sidebar_hovered_row = hovered_row;
            self.request_redraw();
        }

        let palette_hovered = self
            .palette_geometry
            .and_then(|panel| crate::palette::row_hit(panel, layout.metrics.line_height, x, y));
        if palette_hovered != self.command_palette_hovered {
            self.command_palette_hovered = palette_hovered;
            self.request_redraw();
        }

        let activity_hovered = if any_grip_hovered {
            None
        } else {
            crate::layout::icon_rail_hit(layout.activity_bar, ACTIVITY_ICONS.len(), x, y)
        };
        if activity_hovered != self.activity_hovered {
            self.activity_hovered = activity_hovered;
            self.request_redraw();
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
    /// Left half of the status bar, in Lapce's arrangement: the branch first,
    /// then the diagnostic counts, then whatever transient message the shell
    /// most recently had to say.
    fn status_left(&self) -> crate::text::RichText {
        use crate::icons::Icon;
        let mut rich = crate::text::RichText::new();

        if let Some(status) = self.core.git_status() {
            if let Some(branch) = &status.branch {
                rich.icon(Icon::GitBranch, self.theme.status_text);
                rich.plain(" ");
                // Lapce marks a dirty working tree with a trailing asterisk
                // rather than a second glyph.
                let dirty = if status.is_clean() { "" } else { "*" };
                rich.colored(&format!("{branch}{dirty}"), self.theme.status_text);
                rich.plain("   ");
            }
        }

        let (errors, warnings) = self.diagnostic_counts();
        rich.icon(Icon::Error, self.theme.status_text);
        rich.colored(&format!(" {errors}  "), self.theme.status_text);
        rich.icon(Icon::Warning, self.theme.status_text);
        rich.colored(&format!(" {warnings}"), self.theme.status_text);

        // A transient message (a save failure, a load rejection) outranks the
        // steady-state readout for as long as it is live.
        if let Some((message, at, _)) = &self.status_message {
            if at.elapsed() < STATUS_MESSAGE_TIME {
                rich.plain("   ");
                rich.colored(message, self.status_color());
                return rich;
            }
        }
        if let Some(summary) = devpanel::status_summary(&self.core) {
            rich.plain("   ");
            rich.colored(&summary, self.theme.status_text);
        } else {
            let state = self.document_state_label();
            if !state.is_empty() {
                rich.plain("   ");
                rich.colored(state, self.theme.dim_text);
            }
        }
        rich
    }

    /// Error and warning counts across every open document, the number Lapce
    /// puts next to its two status-bar glyphs.
    fn diagnostic_counts(&self) -> (usize, usize) {
        let mut errors = 0;
        let mut warnings = 0;
        for tab in self.core.tabs() {
            let Some(document) = self.core.document(*tab) else { continue };
            for diagnostic in document.diagnostics() {
                match diagnostic.severity {
                    ls_core::DiagnosticSeverity::Error => errors += 1,
                    ls_core::DiagnosticSeverity::Warning => warnings += 1,
                    _ => {}
                }
            }
        }
        (errors, warnings)
    }

    /// Right half of the status bar.
    /// Right half of the status bar: cursor position, encoding, line ending
    /// and language -- Lapce's order, and the panel toggles it ends with.
    ///
    /// The process readouts (RAM, CPU, frame time) that used to live here are
    /// still available in the performance overlay (F12); the status bar is
    /// for the document, not for the profiler.
    fn status_right(&self) -> crate::text::RichText {
        use crate::icons::Icon;
        let mut rich = crate::text::RichText::new();
        rich.icon(Icon::LayoutSidebarLeft, self.theme.dim_text);
        rich.plain("  ");
        rich.icon(Icon::LayoutPanel, self.theme.dim_text);
        rich.plain("   ");

        if let Some(document) = self.core.active_document() {
            let (line, column) = self.cursor_line_column().unwrap_or((0, 0));
            let mut line_ending = document.line_ending().label().to_string();
            if document.has_mixed_line_endings() {
                line_ending.push_str(" (mixed)");
            }
            rich.colored(
                &format!(
                    "Ln {}, Col {}   {}   {}   {}",
                    line + 1,
                    column + 1,
                    document.encoding().label(),
                    line_ending,
                    document.language().name(),
                ),
                self.theme.status_text,
            );
        } else {
            rich.colored(keymap::HINTS, self.theme.dim_text);
        }
        rich
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

    /// Gives the sidebar list (file tree / search results / git status)
    /// input focus. An explicit action, the same as `focus_editor`, and for
    /// the same reason: since the panel now stays open after the user opens
    /// a file from it (a docked panel, not a modal picker), `refresh_focus`
    /// can no longer treat "the panel is visible" as "the panel is focused"
    /// -- entering List focus has to be something a caller asks for.
    fn focus_list(&mut self) {
        self.menu.close();
        self.focus = InputFocus::List;
        self.request_redraw();
    }

    /// Gives the terminal panel input focus. Same reasoning as `focus_list`:
    /// the terminal stays visible after it loses focus (clicking back into
    /// the editor does not close it), so entering its focus is explicit too.
    fn focus_terminal(&mut self) {
        self.menu.close();
        self.focus = InputFocus::Terminal;
        self.request_redraw();
    }

    /// Recomputes the input target from the surfaces that are up.
    ///
    /// Prompt, the command palette, Menu and Find are genuinely exclusive:
    /// while any of them is up, nothing else can reasonably hold the
    /// keyboard, so they are derived fresh every time. Editor, List,
    /// Terminal and SearchQuery are not -- the file tree, the terminal and a
    /// typed-but-not-yet-submitted search query all stay around after the
    /// user's attention moves elsewhere (see `focus_list`/`focus_terminal`/
    /// `open_search_query`), so visibility or "is there a query" alone can
    /// no longer decide between them the way it used to. This is the fix for
    /// two real bugs of the same shape: opening a file from the explorer
    /// left it typeable for exactly one keystroke, because the very next
    /// `refresh_focus` call -- triggered by the load simply finishing --
    /// saw the still-open file tree and handed focus straight back to it;
    /// clicking into the editor while a workspace-search query was typed
    /// but not yet submitted looked identical, but for search (nothing ever
    /// clears `search_query_input` on a stray click, only Escape/Enter do).
    /// A resting focus (Editor/List/Terminal/SearchQuery) is now left alone
    /// unless its own surface has closed out from under it, in which case it
    /// falls back to the editor; entering List, Terminal or SearchQuery in
    /// the first place is always an explicit call to
    /// `focus_list`/`focus_terminal`/`open_search_query`, never something
    /// this function decides on its own.
    fn refresh_focus(&mut self) {
        self.focus = derive_focus(
            self.focus,
            self.prompt.is_some(),
            self.menu.is_open(),
            self.search_query_input.is_some(),
            self.core.is_find_open(),
            self.active_list.is_some(),
            self.terminal_visible,
            self.command_palette_open,
        );
    }

    /// Opens the command palette, the way clicking the title bar's own
    /// command field or Ctrl+Shift+P does in Lapce -- an explicit action,
    /// not something `refresh_focus` derives, the same discipline
    /// `focus_list`/`focus_terminal` already follow.
    fn open_command_palette(&mut self) {
        self.menu.close();
        self.command_palette_open = true;
        self.command_palette_query.clear();
        self.command_palette_selected = 0;
        self.command_palette_hovered = None;
        self.refresh_focus();
        self.request_redraw();
    }

    fn close_command_palette(&mut self) {
        self.command_palette_open = false;
        self.refresh_focus();
        self.request_redraw();
    }

    /// The commands the palette currently offers: the full registry, filtered
    /// by the typed query and by what the active document actually allows
    /// right now -- the same enablement check the dropdown menu uses, so the
    /// palette never lists something it would then refuse to run.
    fn command_palette_rows(&self) -> Vec<crate::palette::PaletteRow> {
        crate::palette::filter(ls_core::commands::all(), &self.command_palette_query, |id| {
            self.core.is_command_enabled(id)
        })
    }

    /// Runs the highlighted row, if any, and closes the palette either way --
    /// selecting something that turns out to be disabled by the time Enter
    /// is pressed is vanishingly unlikely (the list it was chosen from is
    /// filtered by the same check moments earlier), but `run_command` itself
    /// already handles a command that refuses to apply.
    fn run_palette_selection(&mut self) {
        let rows = self.command_palette_rows();
        if let Some(row) = rows.get(self.command_palette_selected) {
            let id = row.id;
            self.close_command_palette();
            self.run_command(id, CommandArgs::None);
        } else {
            self.close_command_palette();
        }
    }

    /// The palette's own content: a query row (magnifier, typed text, a
    /// caret), then one line per still-matching command. Selection/hover
    /// highlighting is drawn separately, from the same row rectangles
    /// `palette::row_hit` uses, the same split `sidebar_rows` and its
    /// highlight quads keep.
    fn palette_rich(&self, rows: &[crate::palette::PaletteRow]) -> crate::text::RichText {
        let mut rich = crate::text::RichText::new();
        rich.icon(crate::icons::Icon::Search, self.theme.dim_text);
        rich.plain(" ");
        if self.command_palette_query.is_empty() {
            rich.colored("Type a command...", self.theme.dim_text);
        } else {
            rich.colored(&self.command_palette_query, self.theme.text);
        }
        rich.colored("\u{2588}", self.theme.cursor);
        // Capped the same way `palette::geometry` caps the panel's height:
        // there is no scrolling here, so a row this buffer shaped but the
        // panel had no room to show would just be an invisible, still-
        // selectable command -- confusing rather than merely absent.
        for row in rows.iter().take(crate::palette::MAX_VISIBLE_ROWS) {
            rich.newline();
            rich.colored(row.display_name, self.theme.text);
        }
        rich
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
    ///
    /// A typed workspace-search query used to get a banner here too, a
    /// second, modal-looking box floating over the editor duplicating the
    /// same query already live in the sidebar's own Search panel (see
    /// `push_search_header`) -- worse, one with no click-away out, only
    /// Escape, which read as the editor being stuck. The sidebar's field is
    /// the only place that query needs to show.
    fn banner_text(&self) -> Option<String> {
        self.prompt_text().or_else(|| self.find_bar_text())
    }

    /// Builds the rows for whichever list is currently shown, fresh every
    /// call. All three lists are cheap to (re)build: git status and search
    /// results just format state `EditorCore` already has, and the file tree
    /// only reads directories the user actually expanded (never a recursive
    /// walk), so nothing here is worth caching and nothing can go stale.
    fn list_rows(&self) -> Vec<ListRow> {
        use crate::icons::Icon;
        use crate::theme::SidebarRowKind;
        let root = self.core.workspace().root().map(|c| c.as_path().to_path_buf());
        match self.active_list {
            None => Vec::new(),
            // Lapce's explorer is two stacked, collapsible sections: the
            // documents that are open, then the workspace tree.
            Some(ListKind::FileTree) => {
                let mut rows = Vec::new();
                rows.push(
                    ListRow::message("OPEN EDITORS")
                        .with_kind(SidebarRowKind::Header)
                        .with_icon(Icon::ChevronDown),
                );
                for tab in self.core.tab_presentations() {
                    let file_icon = crate::icons::icon_for_file(&tab.title);
                    rows.push(
                        ListRow {
                            label: tab.title.clone(),
                            action: Some(ListAction::ActivateTab(tab.id)),
                            depth: 1,
                            chevron: None,
                            icon: Some(file_icon.into()),
                            icon_color: Some(file_icon.color()),
                            kind: SidebarRowKind::File,
                        }
                        .with_kind(if tab.active {
                            SidebarRowKind::Directory
                        } else {
                            SidebarRowKind::File
                        }),
                    );
                }
                rows.push(
                    ListRow::message("FILE EXPLORER")
                        .with_kind(SidebarRowKind::Header)
                        .with_icon(Icon::ChevronDown),
                );
                if let Some(tree_root) = &self.file_tree_root {
                    let before = rows.len();
                    self.append_tree_level(tree_root, 0, &mut rows);
                    if rows.len() == before {
                        rows.push(ListRow::message("(empty)"));
                    }
                } else {
                    rows.push(ListRow::message("No folder opened"));
                }
                rows
            }
            Some(ListKind::GitStatus) => match self.core.git_status() {
                Some(status) if status.is_clean() => {
                    vec![ListRow::message("Working tree clean")]
                }
                Some(status) => status
                    .files
                    .iter()
                    .map(|file| {
                        let full = root.as_ref().map(|r| r.join(&file.path));
                        let file_icon =
                            crate::icons::icon_for_file(&file.path.display().to_string());
                        let mut row = ListRow {
                            label: format!(
                                "{}{}",
                                file.path.display(),
                                if file.staged { "  (staged)" } else { "" }
                            ),
                            action: None,
                            depth: 0,
                            chevron: None,
                            icon: Some(file_icon.into()),
                            icon_color: Some(file_icon.color()),
                            kind: SidebarRowKind::File,
                        };
                        row.action = full.map(ListAction::OpenFile);
                        row
                    })
                    .collect(),
                None if self.core.is_git_status_pending() => {
                    vec![ListRow::message("Checking git status...")]
                }
                None => {
                    vec![ListRow::message("Not a git repository, or git is not installed")]
                }
            },
            // Only ever the result for the query in the field right now:
            // `matching_search_result` drops one for an older prefix, so a
            // panel that updates as you type never shows hits for a query
            // you have already typed past.
            Some(ListKind::SearchResults) => match self.matching_search_result() {
                _ if self
                    .search_query_input
                    .as_deref()
                    .is_some_and(|query| query.trim().is_empty()) =>
                {
                    Vec::new()
                }
                Some(result) if result.hits.is_empty() => {
                    vec![ListRow::message(format!("No matches for \"{}\"", result.query))]
                }
                Some(result) => {
                    let mut rows: Vec<ListRow> = result
                        .hits
                        .iter()
                        .map(|hit| {
                            let name = hit
                                .path
                                .file_name()
                                .map(|name| name.to_string_lossy().to_string())
                                .unwrap_or_else(|| hit.path.display().to_string());
                            let file_icon = crate::icons::icon_for_file(&name);
                            ListRow {
                                label: format!(
                                    "{name}:{}  {}",
                                    hit.line_number,
                                    hit.preview.trim()
                                ),
                                action: Some(ListAction::OpenFileAt(
                                    hit.path.clone(),
                                    hit.line_number,
                                )),
                                depth: 0,
                                chevron: None,
                                icon: Some(file_icon.into()),
                                icon_color: Some(file_icon.color()),
                                kind: SidebarRowKind::File,
                            }
                        })
                        .collect();
                    if result.truncated {
                        rows.push(ListRow::message("... more results exist; narrow the query"));
                    }
                    rows
                }
                // A query is typed but its own results have not landed yet:
                // either a walk is running, or the debounce is still
                // counting down before one starts.
                None if self.core.is_workspace_search_pending()
                    || self.search_debounce_deadline.is_some() =>
                {
                    vec![ListRow::message("Searching...")]
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
        self.focus_list();
    }

    /// Opens the native folder picker and, if the user chooses one, opens it
    /// as the workspace root and shows the explorer. This never depends on a
    /// document already being open -- the picker itself is where the folder
    /// comes from.
    fn show_open_folder_dialog(&mut self) {
        if self.folder_picker_open {
            // A second request while one is already up would stack two
            // dialogs the user never asked for.
            return;
        }
        let initial_dir =
            self.core.workspace().root().map(|c| c.as_path().to_path_buf()).or_else(|| {
                self.core
                    .active_document()
                    .and_then(|d| d.path())
                    .and_then(|p| p.as_path().parent().map(|parent| parent.to_path_buf()))
            });

        // The picker runs elsewhere and reports back through the event loop.
        // It used to run here, which meant the editor stopped painting for as
        // long as the Windows shell took to put its dialog on screen -- and
        // measurably that is seconds, none of it this program's work (see
        // `ls_platform::dialog::open_folder_async`).
        self.folder_picker_open = true;
        self.set_status("Opening the folder picker...", Severity::Success);
        let proxy = self.proxy.clone();
        ls_platform::dialog::open_folder_async(
            "Open Folder",
            initial_dir.as_deref(),
            move |result| {
                let _ = proxy.send_event(UserEvent::FolderPicked(Box::new(result)));
            },
        );
    }

    /// Applies whatever the folder picker came back with.
    /// Opens `dir` as the workspace root and shows it in the explorer --
    /// shared by the folder picker and by a directory named on the command
    /// line, so both land in exactly the same state.
    fn open_workspace_at(&mut self, dir: PathBuf) {
        if let Err(error) = self.core.open_workspace(&dir) {
            self.set_status(error.to_string(), Severity::Error);
            return;
        }
        self.file_tree_root = Some(dir);
        self.list_selected = 0;
        self.active_list = Some(ListKind::FileTree);
        // The project half of the settings moves with the folder.
        self.reload_settings();
        self.apply_settings();
        self.focus_list();
    }

    fn folder_picked(&mut self, result: Result<Option<PathBuf>, ls_platform::PlatformError>) {
        self.folder_picker_open = false;
        // Drop the "opening..." note: whatever happens next either replaces
        // it or means there is nothing left to say.
        self.status_message = None;
        match result {
            Ok(Some(dir)) => self.open_workspace_at(dir),
            Ok(None) => {}
            Err(error) => {
                ls_log::diag::log_error(&error);
                self.set_status(error.to_string(), Severity::Error);
            }
        }
        self.request_redraw();
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
        self.focus_list();
    }

    fn open_search_query(&mut self) {
        // The results list stays mounted under the query field from the
        // moment the panel opens: `list_rows` shows the state of the search
        // for the *current* query (nothing / searching / hits), so there is
        // no separate "results mode" to switch into once one completes.
        self.active_list = Some(ListKind::SearchResults);
        self.search_query_input = Some(String::new());
        self.list_selected = 0;
        self.search_debounce_deadline = None;
        // Explicit, the same as `focus_list`/`focus_terminal`: entering
        // `SearchQuery` focus is no longer something `derive_focus` grants
        // just because a query happens to be set (see its own comment).
        self.focus = InputFocus::SearchQuery;
        self.request_redraw();
    }

    /// Restarts the debounce window after the query changed. The search
    /// itself runs from `about_to_wait` once the window elapses without
    /// another keystroke resetting it.
    fn schedule_search_debounce(&mut self) {
        self.search_debounce_deadline = Some(Instant::now() + SEARCH_DEBOUNCE);
        self.list_selected = 0;
        self.request_redraw();
    }

    /// Dispatches the typed query now, cancelling whatever walk is already in
    /// flight (`EditorCore::request_workspace_search` cancels the previous
    /// task, and the walk polls that flag per file).
    fn run_pending_search(&mut self) {
        self.search_debounce_deadline = None;
        let Some(query) = self.search_query_input.clone() else { return };
        if query.is_empty() {
            return;
        }
        if let Err(error) = self.core.request_workspace_search(query) {
            self.set_status(error.to_string(), Severity::Error);
        }
        self.request_redraw();
    }

    /// Fires the debounced search if its window has elapsed. Returns whether
    /// anything was dispatched, so the caller knows to redraw.
    fn tick_search_debounce(&mut self) -> bool {
        let Some(deadline) = self.search_debounce_deadline else { return false };
        if Instant::now() < deadline {
            return false;
        }
        self.run_pending_search();
        true
    }

    /// How many result rows the search panel is currently showing, for
    /// bounding arrow-key movement through them.
    fn search_result_rows(&self) -> usize {
        if self.active_list != Some(ListKind::SearchResults) {
            return 0;
        }
        match self.matching_search_result() {
            Some(result) => result.hits.len(),
            None => 0,
        }
    }

    /// The completed search result, but only if it is for the query that is
    /// in the field right now.
    ///
    /// Results arrive from a worker, so the one in hand is routinely for a
    /// prefix of what has since been typed. Showing it anyway is how a
    /// live-updating panel ends up flashing hits for "nee" under a field
    /// that reads "needle" -- so a result whose query does not match is
    /// treated as not being there at all.
    fn matching_search_result(&self) -> Option<&ls_core::workspace_search::WorkspaceSearchResult> {
        let result = self.core.workspace_search_result()?;
        search_result_is_current(&result.query, self.search_query_input.as_deref())
            .then_some(result)
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
        self.focus_terminal();
    }

    fn drain_terminal_output(&mut self) {
        let Some(terminal) = self.terminal.as_mut() else { return };
        let text = terminal.drain_output();
        if text.is_empty() {
            return;
        }
        // Held so the view can stay on the same lines if the user is reading
        // history while output keeps arriving underneath.
        let before = self.terminal_scrollback.lines().count();

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

        if self.terminal_scroll_lines > 0 {
            // Scrolled back: the offset is measured from the bottom, so new
            // lines at the bottom would slide the visible window forward.
            // Growing the offset by however many lines arrived keeps the
            // text the user is actually reading still. Trimming the
            // scrollback can shrink the total, so this is clamped rather
            // than simply added to.
            let after = self.terminal_scrollback.lines().count();
            let added = after.saturating_sub(before);
            let max_scroll = self
                .last_layout
                .map(|layout| after.saturating_sub(terminal_visible_lines(&layout)))
                .unwrap_or(after);
            self.terminal_scroll_lines = (self.terminal_scroll_lines + added).min(max_scroll);
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

        // A server that crashed or exited is dropped, and any document it
        // had been told about is forgotten, so the next document of that
        // language starts a fresh one and re-announces itself rather than
        // talking to a dead handle forever.
        for retired in self.lsp.retire_dead() {
            self.lsp_opened.retain(|document| {
                self.core
                    .document(*document)
                    .map(|document| document.language() != retired)
                    .unwrap_or(false)
            });
        }

        let proxy = self.proxy.clone();
        let root = self
            .core
            .workspace()
            .root()
            .map(|c| c.as_path().to_path_buf())
            .or_else(|| path.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        if !self.lsp_opened.contains(&id) {
            // Routed by the document's *own* language, so a Python file
            // reaches a Python server rather than whichever server happened
            // to start first.
            if let Some(client) = self.lsp.client_for(language, &root, move || {
                let _ = proxy.send_event(UserEvent::LspDiagnostics);
            }) {
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
            if let Some(client) = self.lsp.client_for(language, &root, move || {
                let _ = proxy.send_event(UserEvent::LspDiagnostics);
            }) {
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
        if self.lsp.is_empty() {
            return;
        }
        let updates = self.lsp.drain_diagnostics();
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
        self.terminal_cursor = 0;
        self.record_terminal_history(&line);
        if let Some(terminal) = self.terminal.as_mut() {
            if !terminal.is_alive() {
                self.set_status("The shell has exited", Severity::Warning);
                self.terminal = None;
                return;
            }
            self.terminal_scrollback.push_str("> ");
            self.terminal_scrollback.push_str(&line);
            self.terminal_scrollback.push('\n');
            // Also writes "> {line}" to the permanent transcript
            // (`ls_platform::terminal_log`) -- one call, so a line can never
            // reach the on-screen scrollback without also reaching the
            // record meant to outlive it.
            terminal.send_line(&line);
            // Running something is a statement about wanting to see what it
            // does, so this snaps back to the newest output however far back
            // the user had scrolled.
            self.terminal_scroll_lines = 0;
        }
    }

    /// Adds a run command to the recall list Up/Down walks, and ends
    /// whatever recall was in progress -- running something is a decision
    /// about what to do next, not a browse through what was done before.
    fn record_terminal_history(&mut self, line: &str) {
        self.terminal_history_index = None;
        self.terminal_history_draft.clear();
        if line.trim().is_empty() {
            return;
        }
        // Immediately repeating the last command is common (rerunning a
        // build, say) and not worth a second, identical entry to arrow past.
        if self.terminal_command_history.last().map(String::as_str) != Some(line) {
            self.terminal_command_history.push(line.to_string());
        }
        // Bounded the same way the permanent transcript is not: this list
        // exists for in-session recall, and a session that ran ten thousand
        // commands does not need all of them one Up-press away -- the file
        // `ls_platform::terminal_log` writes to is where the full record
        // lives.
        const MAX_RECALL_HISTORY: usize = 1000;
        if self.terminal_command_history.len() > MAX_RECALL_HISTORY {
            let overflow = self.terminal_command_history.len() - MAX_RECALL_HISTORY;
            self.terminal_command_history.drain(..overflow);
        }
    }

    /// Recalls an older command (Up). The first press off a live line saves
    /// what was being typed so Down can restore it later.
    fn terminal_history_up(&mut self) {
        let next = navigate_terminal_history(
            &self.terminal_command_history,
            self.terminal_history_index,
            HistoryDirection::Older,
        );
        let Some(index) = next else { return };
        if self.terminal_history_index.is_none() {
            self.terminal_history_draft = std::mem::take(&mut self.terminal_input);
        }
        self.terminal_history_index = Some(index);
        self.terminal_input = self.terminal_command_history[index].clone();
        // A recalled command lands with the caret at the end, ready to run
        // as-is or be edited -- the same place a real shell's history
        // recall leaves it.
        self.terminal_cursor = self.terminal_input.chars().count();
    }

    /// Walks back toward the live input line (Down). A no-op while not
    /// already recalling something -- otherwise every Down press on an
    /// ordinary line would clobber whatever was being typed with a stale,
    /// empty draft.
    fn terminal_history_down(&mut self) {
        if self.terminal_history_index.is_none() {
            return;
        }
        let next = navigate_terminal_history(
            &self.terminal_command_history,
            self.terminal_history_index,
            HistoryDirection::Newer,
        );
        self.terminal_history_index = next;
        self.terminal_input = match next {
            Some(index) => self.terminal_command_history[index].clone(),
            None => std::mem::take(&mut self.terminal_history_draft),
        };
        self.terminal_cursor = self.terminal_input.chars().count();
    }

    /// Inserts `text` at the caret and advances it past what was inserted.
    fn terminal_insert(&mut self, text: &str) {
        let byte = terminal_char_boundary(&self.terminal_input, self.terminal_cursor);
        self.terminal_input.insert_str(byte, text);
        self.terminal_cursor += text.chars().count();
    }

    /// Deletes the character behind the caret (Backspace) and moves the
    /// caret back onto where it used to be.
    fn terminal_backspace(&mut self) {
        let Some(previous) = self.terminal_cursor.checked_sub(1) else { return };
        let start = terminal_char_boundary(&self.terminal_input, previous);
        let end = terminal_char_boundary(&self.terminal_input, self.terminal_cursor);
        self.terminal_input.replace_range(start..end, "");
        self.terminal_cursor = previous;
    }

    /// Deletes the character under/ahead of the caret (Delete). The caret
    /// itself does not move -- there is nothing left of it to step back
    /// over, unlike Backspace.
    fn terminal_delete_forward(&mut self) {
        if self.terminal_cursor >= self.terminal_input.chars().count() {
            return;
        }
        let start = terminal_char_boundary(&self.terminal_input, self.terminal_cursor);
        let end = terminal_char_boundary(&self.terminal_input, self.terminal_cursor + 1);
        self.terminal_input.replace_range(start..end, "");
    }

    fn terminal_move_left(&mut self) {
        self.terminal_cursor = self.terminal_cursor.saturating_sub(1);
    }

    fn terminal_move_right(&mut self) {
        let end = self.terminal_input.chars().count();
        self.terminal_cursor = (self.terminal_cursor + 1).min(end);
    }

    /// Rows for the docked bottom panel's terminal content: the visible
    /// window of scrollback plus the line being typed, in the same
    /// `Vec<String>` shape every other panel uses.
    fn bottom_panel_rows(&self, layout: &Layout) -> Vec<String> {
        let mut rows = Vec::new();
        rows.push("Terminal  (Enter to run, \u{2191}\u{2193} for history, F11 to hide)".to_string());

        // Sized to the panel the user actually dragged out, not a fixed 12:
        // a hardcoded count clipped output on a tall panel and left a short
        // one half empty. Two rows are spent on the header and the input
        // line, so they come off the budget.
        let visible_lines = terminal_visible_lines(layout);
        let all: Vec<&str> = self.terminal_scrollback.lines().collect();
        // `terminal_scroll_lines` counts lines *up from the bottom*, so 0 is
        // pinned to the newest output -- the state a terminal is in almost
        // all the time.
        let from_bottom = self.terminal_scroll_lines.min(all.len().saturating_sub(visible_lines));
        let end = all.len().saturating_sub(from_bottom);
        let start = end.saturating_sub(visible_lines);
        rows.extend(all[start..end].iter().map(|line| line.to_string()));

        // Only the live prompt when looking at the newest output: showing an
        // input line under scrolled-back history would be an invitation to
        // type into something that is not where the cursor is.
        if from_bottom == 0 {
            rows.push(terminal_input_row(&self.terminal_input, self.terminal_cursor));
        } else {
            rows.push(format!("-- scrolled back {from_bottom} lines --"));
        }
        rows
    }

    /// Scrolls the terminal's output by `lines` (positive scrolls back into
    /// history), clamped so it can never run past either end.
    fn scroll_terminal_by_lines(&mut self, lines: f32) {
        let Some(layout) = self.last_layout else { return };
        let total = self.terminal_scrollback.lines().count();
        let max_scroll = total.saturating_sub(terminal_visible_lines(&layout));
        let delta = lines.round() as i64;
        let next = self.terminal_scroll_lines as i64 + delta;
        self.terminal_scroll_lines = next.clamp(0, max_scroll as i64) as usize;
        self.request_redraw();
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
                // `adopt_new_document`); `refresh_focus` now leaves an
                // Editor-resting focus alone regardless of the panel still
                // being open (see its doc comment), so there is nothing left
                // to do here.
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
            Some(ListAction::ActivateTab(id)) => {
                // Open Editors rows switch to a document that is already
                // loaded; `activate_tab` moves focus to the editor itself.
                self.activate_tab(id);
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
        // The caret, the disk-change poll and a pending search debounce are
        // the only timers; everything else is event-driven. Whichever fires
        // first is when the loop wakes -- and with no query waiting, the
        // search contributes no wakeup at all rather than a ticking one.
        let caret =
            (self.core.active().is_some() && self.prompt.is_none()).then_some(self.caret_deadline);
        let watch = (!self.core.tabs().is_empty()).then_some(self.next_watch_check);
        [caret, watch, self.search_debounce_deadline].into_iter().flatten().min()
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

    /// Whether the docked bottom panel should be shown this frame. Terminal
    /// is the only tenant it has today (Lapce also docks Search/Problem
    /// there; we have neither as a real panel yet), so this is exactly
    /// `terminal_visible` for now, named separately because the layout and
    /// the panel-switching UI treat it as its own dock, not as "the
    /// terminal's own state" -- the same way `sidebar_visible` is its own
    /// concept even though today it is driven by `active_list`.
    fn bottom_panel_visible(&self) -> bool {
        self.terminal_visible
    }

    /// Which activity-bar row corresponds to the sidebar's current state, if
    /// any -- so its cell reads as "cut into" the editor. Typing a search
    /// query counts as Search being active even before any request has
    /// gone out, matching how `sidebar_visible` already treats it.
    fn activity_active_index(&self) -> Option<usize> {
        if self.dependency_view {
            return Some(ACTIVITY_DEPENDENCIES);
        }
        if self.search_query_input.is_some() {
            return Some(ACTIVITY_SEARCH);
        }
        match self.active_list {
            Some(ListKind::FileTree) => Some(ACTIVITY_EXPLORER),
            Some(ListKind::SearchResults) => Some(ACTIVITY_SEARCH),
            Some(ListKind::GitStatus) => Some(ACTIVITY_SOURCE_CONTROL),
            None => None,
        }
    }

    /// Handles a click on activity-bar row `index`. Clicking the item that
    /// is already active closes its panel, the same toggle behavior the
    /// keyboard shortcuts for these panels already have. Extensions and
    /// Debug (indices 3 and 4) are inert: there is no subsystem behind them.
    fn activate_activity_item(&mut self, index: usize) {
        let already_active = self.activity_active_index() == Some(index);
        // A query left typed-but-unsubmitted is what keeps the Search panel
        // showing (see `activity_active_index`, `sidebar_header_lines`): it
        // outranks `active_list` on its own, so switching to Explorer or
        // Source Control without clearing it left the sidebar showing
        // Search regardless of which icon was actually clicked -- Explorer
        // looked unclickable because activating it worked, the *display*
        // just never noticed.
        if index != ACTIVITY_SEARCH {
            self.search_query_input = None;
        }
        // Only one activity is shown at a time, so picking any other one
        // puts the document back in the editor area.
        if index != ACTIVITY_DEPENDENCIES {
            self.dependency_view = false;
        }
        match index {
            ACTIVITY_EXPLORER => self.toggle_file_tree(),
            ACTIVITY_SEARCH => {
                if already_active {
                    self.active_list = None;
                    self.search_query_input = None;
                    self.refresh_focus();
                    self.request_redraw();
                } else {
                    self.open_search_query();
                }
            }
            ACTIVITY_SOURCE_CONTROL => self.toggle_git_status(),
            ACTIVITY_DEPENDENCIES => self.toggle_dependency_view(already_active),
            _ => {}
        }
    }

    // --- settings -----------------------------------------------------

    /// Loads both settings files and merges them.
    ///
    /// Called at startup and whenever the workspace changes, because the
    /// project half of the answer moves with the folder.
    fn reload_settings(&mut self) {
        self.user_settings = ls_platform::settings_file::user_path()
            .and_then(|path| ls_platform::settings_file::load(&path))
            .map(|text| ls_core::settings::Settings::decode(&text))
            .unwrap_or_default();
        self.workspace_settings = self
            .core
            .workspace()
            .root()
            .map(|root| ls_platform::settings_file::workspace_path(root.as_path()))
            .and_then(|path| ls_platform::settings_file::load(&path))
            .map(|text| ls_core::settings::Settings::decode(&text))
            .unwrap_or_default();
        self.remerge_settings();
    }

    /// Recomputes what the rest of the shell reads: the person's settings,
    /// with the project's laid over whatever it actually states.
    fn remerge_settings(&mut self) {
        let mut merged = self.user_settings.clone();
        merged.overlay(&self.workspace_settings);
        self.settings = merged;
    }

    /// Writes the file the screen is currently editing.
    fn save_settings(&mut self) {
        let (path, settings) = match self.settings_scope {
            SettingsScope::User => {
                (ls_platform::settings_file::user_path(), &self.user_settings)
            }
            SettingsScope::Workspace => (
                self.core
                    .workspace()
                    .root()
                    .map(|root| ls_platform::settings_file::workspace_path(root.as_path())),
                &self.workspace_settings,
            ),
        };
        let Some(path) = path else {
            self.set_status("Open a folder to keep settings with the project", Severity::Warning);
            return;
        };
        if !ls_platform::settings_file::save(&path, &settings.encode()) {
            self.set_status(format!("Could not write {}", path.display()), Severity::Error);
        }
    }

    /// Changes one setting in the file being edited, then applies and saves.
    fn write_setting(&mut self, key: &str, value: &str) {
        let changed = match self.settings_scope {
            SettingsScope::User => self.user_settings.set(key, value),
            SettingsScope::Workspace => self.workspace_settings.set(key, value),
        };
        if !changed {
            return;
        }
        self.remerge_settings();
        self.apply_settings();
        self.save_settings();
        self.request_redraw();
    }

    /// Puts one setting back to its default.
    fn reset_setting(&mut self, key: &str) {
        let changed = match self.settings_scope {
            SettingsScope::User => self.user_settings.reset(key),
            SettingsScope::Workspace => self.workspace_settings.reset(key),
        };
        if !changed {
            return;
        }
        self.remerge_settings();
        self.apply_settings();
        self.save_settings();
        self.request_redraw();
    }

    /// Pushes the merged settings into the things they configure.
    ///
    /// Only the ones that can change under a running window are here; the
    /// rest are read where they are used, and the screen says so on their
    /// own row rather than pretending otherwise.
    fn apply_settings(&mut self) {
        let family = self.settings.text("editor.fontFamily");
        let size = self.settings.integer("editor.fontSize") as f32;
        let ratio = self.settings.float("editor.lineHeight") as f32;
        let scale = self.window.as_ref().map(|window| window.scale_factor()).unwrap_or(1.0) as f32;
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.set_font(&family, size * scale, ratio);
        }

        self.core.set_document_settings(ls_core::document::DocumentSettings {
            tab_width: self.settings.integer("editor.tabWidth").max(1) as usize,
            insert_spaces: self.settings.bool("editor.insertSpaces"),
            coalesce_window: self.core.config().editor.coalesce_window,
        });

        self.sidebar_width = self.settings.integer("workbench.sidebarWidth") as f32;
        self.overlay_visible = self.settings.bool("performance.showOverlay");
        if let Some(terminal) = self.terminal.as_ref() {
            terminal.set_scrollback(self.settings.integer("terminal.scrollbackBytes") as usize);
        }
        // A caret that is not blinking must also stop asking for frames.
        if !self.settings.bool("editor.caretBlink") {
            self.caret_visible = true;
        }
        self.request_redraw();
    }

    /// Handles a key while the settings screen is up, reporting whether it
    /// was consumed.
    ///
    /// Escape is the one key that always means something: it takes the
    /// keyboard back from a field, and closes the screen when nothing has
    /// it. Everything else only applies while the search box or a field is
    /// focused, so the shortcuts that open other views still work with the
    /// settings on screen.
    fn settings_key(&mut self, key: &winit::keyboard::Key) -> bool {
        use winit::keyboard::{Key, NamedKey};
        match key {
            Key::Named(NamedKey::Escape) => {
                if self.settings_editing.is_some() {
                    // Abandons the draft rather than writing it: Escape has
                    // meant "forget what I typed" everywhere else here.
                    self.settings_editing = None;
                } else if self.settings_search_focused && !self.settings_query.is_empty() {
                    self.settings_query.clear();
                    self.settings_scroll = 0;
                } else {
                    self.settings_open = false;
                    self.refresh_focus();
                }
                self.request_redraw();
                true
            }
            Key::Named(NamedKey::Enter) => {
                if self.settings_editing.is_some() {
                    self.commit_settings_field();
                    self.request_redraw();
                    return true;
                }
                false
            }
            Key::Named(NamedKey::Backspace) => {
                if self.settings_editing.is_some() || self.settings_search_focused {
                    self.settings_backspace();
                    return true;
                }
                false
            }
            Key::Named(NamedKey::Tab) => {
                // Moves between the two settings files, which is the only
                // other thing the screen has to switch between.
                if self.settings_editing.is_none() {
                    self.settings_scope = match self.settings_scope {
                        SettingsScope::User => SettingsScope::Workspace,
                        SettingsScope::Workspace => SettingsScope::User,
                    };
                    self.request_redraw();
                    return true;
                }
                false
            }
            Key::Character(text) => {
                if self.modifiers.control_key() || self.modifiers.alt_key() {
                    return false;
                }
                if self.settings_editing.is_some() || self.settings_search_focused {
                    self.settings_type(text);
                    return true;
                }
                false
            }
            Key::Named(NamedKey::Space) => {
                if self.settings_editing.is_some() || self.settings_search_focused {
                    self.settings_type(" ");
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    /// Opens or closes the settings screen.
    fn toggle_settings(&mut self) {
        self.settings_open = !self.settings_open;
        if self.settings_open {
            self.dependency_view = false;
            self.settings_editing = None;
            self.settings_search_focused = true;
        }
        self.refresh_focus();
        self.request_redraw();
    }

    /// The settings the screen is currently showing: the section picked on
    /// the left, narrowed by whatever is in the search box.
    fn visible_settings(&self) -> Vec<&'static ls_core::settings::SettingDescriptor> {
        let section = self
            .settings_section
            .and_then(|at| ls_core::settings::SECTIONS.get(at))
            .copied();
        ls_core::settings::Settings::search(&self.settings_query)
            .into_iter()
            .filter(|setting| section.is_none_or(|section| setting.section == section))
            .collect()
    }

    /// Handles a click on the settings screen, reporting whether it landed.
    fn press_settings(&mut self, x: f32, y: f32) -> bool {
        use crate::settings_ui::Hit;
        if !self.settings_open {
            return false;
        }
        let visible = self.visible_settings();
        let hit = crate::settings_ui::hit(&self.settings_screen, &visible, x, y);
        // Anything but going on typing in the same field commits the draft,
        // so a value is never left half-entered behind a click elsewhere.
        if !matches!(hit, Hit::Control(key) if Some(key) == self.settings_editing.as_ref().map(|(key, _)| *key))
        {
            self.commit_settings_field();
        }
        match hit {
            Hit::Search => {
                self.settings_search_focused = true;
                self.settings_editing = None;
            }
            Hit::Category(at) => {
                // Clicking the section already showing widens back to all of
                // them, which is the only way back without a separate row.
                self.settings_section = if self.settings_section == Some(at) { None } else { Some(at) };
                self.settings_scroll = 0;
                self.settings_search_focused = false;
            }
            Hit::Control(key) => {
                self.settings_search_focused = false;
                let Some(descriptor) = ls_core::settings::descriptor(key) else { return true };
                match descriptor.kind {
                    ls_core::settings::SettingKind::Bool => {
                        let now = !self.settings.bool(key);
                        self.write_setting(key, if now { "true" } else { "false" });
                    }
                    _ => {
                        self.settings_editing = Some((key, self.settings.text(key)));
                    }
                }
            }
            Hit::Option(key, option) => {
                self.settings_search_focused = false;
                self.write_setting(key, option);
            }
            Hit::Reset(key) => {
                self.settings_search_focused = false;
                self.reset_setting(key);
            }
            Hit::Nothing => {
                self.settings_search_focused = false;
            }
        }
        self.request_redraw();
        true
    }

    /// Writes whatever is in the field being edited, if any.
    fn commit_settings_field(&mut self) {
        let Some((key, draft)) = self.settings_editing.take() else { return };
        self.write_setting(key, &draft);
    }

    /// Types into whichever of the search box or a field has the keyboard.
    fn settings_type(&mut self, text: &str) {
        if let Some((_, draft)) = self.settings_editing.as_mut() {
            draft.push_str(text);
        } else if self.settings_search_focused {
            self.settings_query.push_str(text);
            self.settings_scroll = 0;
        } else {
            return;
        }
        self.request_redraw();
    }

    /// Backspace in whichever of the two has the keyboard.
    fn settings_backspace(&mut self) {
        if let Some((_, draft)) = self.settings_editing.as_mut() {
            draft.pop();
        } else if self.settings_search_focused {
            self.settings_query.pop();
            self.settings_scroll = 0;
        } else {
            return;
        }
        self.request_redraw();
    }

    /// Scrolls the settings list, clamped so it always shows something.
    fn scroll_settings(&mut self, rows: f32) {
        let furthest = crate::settings_ui::max_scroll(
            &self.settings_screen,
            self.last_layout.map(|layout| layout.metrics.line_height).unwrap_or(20.0),
        );
        let moved = (self.settings_scroll as f32 - rows).clamp(0.0, furthest as f32) as usize;
        if moved != self.settings_scroll {
            self.settings_scroll = moved;
            self.request_redraw();
        }
    }

    /// Opens the dependency view.
    ///
    /// The workspace is only scanned when there is nothing already known
    /// about it: a settled graph is written to disk the first time and read
    /// back on every visit after, so opening the view is normally instant.
    /// Scanning and settling are real work -- walking every file, then
    /// running the simulation -- and neither happens on startup or in the
    /// background, only when the reader asks to look. Ctrl+Shift+R rescans.
    fn toggle_dependency_view(&mut self, already_active: bool) {
        if already_active {
            self.dependency_view = false;
            self.dependency_press = None;
            self.refresh_focus();
            self.request_redraw();
            return;
        }
        self.dependency_view = true;
        self.active_list = None;
        self.dependency_press = None;
        self.dependency_hovered = None;
        self.refresh_focus();

        if self.dependency_settled.is_some() {
            // Already in hand from earlier this session.
            self.request_redraw();
            return;
        }
        if self.load_cached_dependencies() {
            self.request_redraw();
            return;
        }
        self.rescan_dependencies();
    }

    /// Throws away what is known and scans the workspace again.
    fn rescan_dependencies(&mut self) {
        if let Err(error) = self.core.request_dependency_graph() {
            self.set_status(format!("Dependency view: {error}"), Severity::Warning);
            return;
        }
        self.dependency_settled = None;
        self.dependency_scene = None;
        self.dependency_hovered = None;
        self.dependency_from_cache = false;
        self.dependency_view_at = crate::depgraph::View::default();
        self.request_redraw();
    }

    /// The command behind Ctrl+Shift+R: rescan, and open the view if it is
    /// not already up, so the shortcut works from anywhere.
    fn refresh_dependency_view(&mut self) {
        if self.core.workspace().root().is_none() {
            self.set_status("Dependency view: open a folder first", Severity::Warning);
            return;
        }
        self.dependency_view = true;
        self.active_list = None;
        self.refresh_focus();
        self.rescan_dependencies();
        self.set_status("Rescanning the workspace for dependencies...", Severity::Success);
    }

    /// Reads back the graph saved for this workspace, reporting whether
    /// there was one to read.
    fn load_cached_dependencies(&mut self) -> bool {
        let Some(root) = self.core.workspace().root().map(|root| root.as_path().to_path_buf())
        else {
            return false;
        };
        let Some(text) = ls_platform::depgraph_cache::load(&root) else { return false };
        let Some(settled) = crate::depgraph::decode(&root, &text) else { return false };

        let files = settled.files.len();
        let edges = settled.edges.len();
        self.dependency_settled = Some(settled);
        self.dependency_scene = None;
        self.dependency_view_at = crate::depgraph::View::default();
        self.dependency_from_cache = true;
        self.set_status(
            format!("Dependencies: {files} files, {edges} links (saved; Ctrl+Shift+R to rescan)"),
            Severity::Success,
        );
        true
    }

    /// Settles a completed scan, saves it, and reports what was found.
    ///
    /// Settling happens here rather than on the worker because this is where
    /// a completed scan first becomes visible to the shell. It costs a few
    /// hundred milliseconds once per workspace, and never again while the
    /// saved copy stands.
    fn adopt_dependency_scan(&mut self) {
        let Some(graph) = self.core.dependency_graph() else { return };
        let files = graph.files.len();
        let edges = graph.edges.len();
        let truncated = graph.truncated;
        let settled = crate::depgraph::settle_graph(graph);

        if let Some(root) = self.core.workspace().root().map(|root| root.as_path().to_path_buf()) {
            let text = crate::depgraph::encode(&root, &settled);
            // Best-effort: a cache that will not write costs a rescan next
            // time, which is not worth interrupting anyone over.
            ls_platform::depgraph_cache::save(&root, &text);
        }

        self.dependency_settled = Some(settled);
        self.dependency_scene = None;
        self.dependency_from_cache = false;
        self.dependency_view_at = crate::depgraph::View::default();
        if truncated {
            self.set_status(
                format!("Dependencies: {files} files, {edges} links (workspace too large; the scan stopped early)"),
                Severity::Warning,
            );
        } else {
            self.set_status(
                format!("Dependencies: {files} files, {edges} links"),
                Severity::Success,
            );
        }
    }

    /// Fits the settled graph to the pane, at the first frame after it lands
    /// and again whenever the pane changes size.
    ///
    /// Done here rather than where the scan completes because it needs the
    /// pane's size, which belongs to the renderer. Re-fitting on every frame
    /// of a window drag would be wasteful, so it only re-runs once the pane
    /// has actually changed by a visible amount. The reader's pan and zoom
    /// survive it: a resize should not throw away where they were looking.
    fn ensure_dependency_scene(&mut self, layout: &Layout) {
        const RESIZE_SLOP: f32 = 8.0;
        let viewport = (layout.text.width, layout.text.height);
        let fitted = self.dependency_scene.as_ref().is_some_and(|scene| {
            (scene.width - viewport.0).abs() < RESIZE_SLOP
                && (scene.height - viewport.1).abs() < RESIZE_SLOP
        });
        if fitted {
            return;
        }
        let Some(settled) = self.dependency_settled.as_ref() else { return };
        self.dependency_scene = Some(crate::depgraph::build_scene(settled, viewport));
    }

    /// What the dependency pane shows when there is no graph to draw yet.
    fn dependency_placeholder(&self) -> &'static str {
        if self.core.workspace().root().is_none() {
            "  No folder is open.\n\n  File > Open Folder, then pick the dependency view again."
        } else if self.core.is_dependency_graph_pending() {
            "  Scanning the workspace for dependencies..."
        } else if self.dependency_settled.is_some() {
            "  Laying the graph out..."
        } else {
            "  No files in a language this understands were found here."
        }
    }

    /// The rectangle the graph is drawn in.
    fn dependency_pane(layout: &Layout) -> crate::layout::Rect {
        layout.text
    }

    /// Zooms about the pointer, so what is under it stays under it.
    fn zoom_dependency_view(&mut self, factor: f32, layout: &Layout) {
        let Some(scene) = self.dependency_scene.as_ref() else { return };
        let pane = Self::dependency_pane(layout);
        let zoomed = self.dependency_view_at.zoomed_at(factor, self.pointer, pane);
        let zoomed = zoomed.clamped(scene, pane);
        if zoomed != self.dependency_view_at {
            self.dependency_view_at = zoomed;
            self.request_redraw();
        }
    }

    /// Puts the whole graph back on screen, at the framing it opened with.
    fn reset_dependency_view(&mut self) {
        if self.dependency_view_at != crate::depgraph::View::default() {
            self.dependency_view_at = crate::depgraph::View::default();
            self.set_status("Dependency view: fitted to the window", Severity::Success);
            self.request_redraw();
        }
    }

    /// Handles a press inside the graph, reporting whether it was consumed.
    fn press_dependency_view(&mut self, x: f32, y: f32, layout: &Layout) -> bool {
        if !self.dependency_view {
            return false;
        }
        let pane = Self::dependency_pane(layout);
        if !pane.contains(x, y) {
            return false;
        }
        let at = self.dependency_view_at;
        let hit = self
            .dependency_scene
            .as_ref()
            .and_then(|scene| crate::depgraph::hit_test(scene, pane, at, (x, y)));

        // Double-clicking the empty canvas puts the whole graph back on
        // screen -- the same gesture every other graph canvas uses to fit,
        // and one that costs no keybinding to remember.
        let repeats = self.last_click.map(|(_, _, _, count)| count).unwrap_or(1);
        if hit.is_none() && repeats >= 2 {
            self.reset_dependency_view();
            self.dependency_press = None;
            return true;
        }

        self.dependency_press =
            Some(DependencyPress { origin: (x, y), at: (x, y), node: hit });
        true
    }

    /// Handles the release that ends a press in the graph.
    ///
    /// A press that stayed put on a node opens that file; one that moved was
    /// a drag, and the graph has already followed the pointer. Deciding on
    /// release rather than on press is what lets one button both pan and
    /// open, with no modifier to remember.
    fn release_dependency_view(&mut self, x: f32, y: f32) {
        let Some(press) = self.dependency_press.take() else { return };
        if !crate::depgraph::is_click(press.origin, (x, y)) {
            return;
        }
        let Some(node) = press.node else { return };
        let Some(path) = self
            .dependency_scene
            .as_ref()
            .and_then(|scene| scene.nodes.get(node))
            .map(|node| node.path.clone())
        else {
            return;
        };
        let Some(root) = self.core.workspace().root().map(|root| root.as_path().to_path_buf())
        else {
            return;
        };
        // Step out of the way: the graph covers the editor, so opening a file
        // without closing it leaves the reader looking at the picture they
        // just clicked instead of the file they asked for. The graph and
        // where they had panned to are both kept, so Ctrl+Shift+D puts them
        // straight back where they were.
        self.dependency_view = false;
        self.dependency_hovered = None;
        self.refresh_focus();
        self.open_path(root.join(path));
    }

    /// Drags the graph with the pointer while a press is held.
    fn drag_dependency_view(&mut self, x: f32, y: f32, layout: &Layout) {
        let Some(press) = self.dependency_press else { return };
        let Some(scene) = self.dependency_scene.as_ref() else { return };
        let pane = Self::dependency_pane(layout);
        // Moved by the step since the last position, while `origin` stays
        // put so the release can still tell a drag from a click.
        let step = (x - press.at.0, y - press.at.1);
        self.dependency_view_at = self.dependency_view_at.panned(step).clamped(scene, pane);
        self.dependency_press = Some(DependencyPress { at: (x, y), ..press });
        self.request_redraw();
    }

    /// Follows the pointer over the graph, tracing whichever file it is on.
    fn hover_dependency_view(&mut self, x: f32, y: f32, layout: &Layout) {
        let pane = Self::dependency_pane(layout);
        let at = self.dependency_view_at;
        let hovered = self
            .dependency_scene
            .as_ref()
            .and_then(|scene| crate::depgraph::hit_test(scene, pane, at, (x, y)));
        if hovered == self.dependency_hovered {
            return;
        }
        self.dependency_hovered = hovered;
        // The status bar has room for the whole path and the counts, which
        // no label inside a circle ever will.
        if let Some((scene, node)) = self.dependency_scene.as_ref().zip(hovered) {
            if let Some(file) = scene.nodes.get(node) {
                let (imports, imported_by) = scene.connections(node);
                self.status_message = Some((
                    format!(
                        "{}  -  imports {imports}, imported by {imported_by}  (click to open)",
                        file.path.display()
                    ),
                    Instant::now(),
                    Severity::Success,
                ));
            }
        }
        self.request_redraw();
    }

    /// Rows for the docked sidebar, each tagged with what kind of row it is
    /// (`theme::SidebarRowKind`) so the renderer can color folders, files,
    /// the header, and plain messages differently -- the nearest this
    /// text-only renderer gets to icons. Row 0 is the header, drawn but
    /// never itself a list entry; while the user is typing a workspace-search
    /// query, row 1 is the live, editable query line instead of a list row.
    ///
    /// Labels are truncated with an ellipsis to `layout.sidebar`'s actual
    /// width, using its own font metrics -- so a name that fits a wide,
    /// user-dragged panel is never clipped mid-character the way a
    /// fixed-width assumption would.
    fn sidebar_rows(&self, layout: &Layout) -> crate::text::RichText {
        use crate::text::RichText;
        use crate::theme::SidebarRowKind;

        let mut rich = RichText::new();
        // How many monospace characters fit after the widest icon prefix, so
        // a long name ends in an ellipsis rather than running under the
        // editor.
        let prefix = 16.0 * layout.scale + layout.metrics.icon_width * 3.0;
        let max_chars = (((layout.sidebar.width - prefix) / layout.metrics.digit_width.max(1.0))
            as usize)
            .max(4);
        let truncate = |label: &str| -> String {
            if label.chars().count() <= max_chars {
                return label.to_string();
            }
            let mut truncated: String = label.chars().take(max_chars.saturating_sub(1)).collect();
            truncated.push('\u{2026}');
            truncated
        };

        // The Search panel gets its own header -- title plus action icons, a
        // real search field, a (decorative, for now) replace field -- drawn
        // the same whether a query is still being typed or its results are
        // being browsed, the way VS Code's own Search view never collapses
        // those fields away once you start reading results.
        if self.showing_search_panel() {
            // The results list follows the header even while the query is
            // still being typed -- that is the whole point of the debounced
            // re-search: the panel updates under the field rather than
            // waiting for the field to be committed.
            self.push_search_header(&mut rich, layout, &truncate);
        } else if let Some(kind) = self.active_list {
            let title = match kind {
                ListKind::FileTree => "EXPLORER",
                ListKind::GitStatus => "SOURCE CONTROL",
                ListKind::SearchResults => unreachable!("handled by `is_search` above"),
            };
            rich.colored(title, self.theme.dim_text);
        } else {
            return rich;
        }

        for row in self.list_rows() {
            rich.newline();
            // Indentation is monospace spaces; the icon columns that follow
            // are icon-width, so every row at the same depth lines up.
            rich.plain(&"  ".repeat(row.depth));
            match row.chevron {
                Some(chevron) => rich.icon(chevron, self.theme.dim_text),
                None if row.kind == SidebarRowKind::Header => &mut rich,
                None => rich.icon_space(),
            };
            if let Some(icon) = row.icon {
                // A file's icon carries its own characteristic color
                // (`icon_color`, set alongside it by `icon_for_file`) --
                // only the folder glyph and header chevron fall back to a
                // kind-based default.
                let color = row.icon_color.unwrap_or(match row.kind {
                    SidebarRowKind::Header => self.theme.dim_text,
                    SidebarRowKind::Directory => self.theme.sidebar_folder,
                    _ => self.theme.dim_text,
                });
                rich.icon(icon, color);
                rich.plain(" ");
            }
            rich.colored(&truncate(&row.label), self.theme.sidebar_row_color(row.kind));
        }
        rich
    }

    /// The Search panel's own three-line header, in place of the plain title
    /// line every other panel gets: a title row with the same refresh /
    /// clear / new-search-editor / collapse-all actions VS Code's Search
    /// view header has, a real search field, and a replace field. Matches
    /// `sidebar_header_lines()`'s count of `3`.
    ///
    /// The case-sensitive / whole-word / regex toggles and the replace field
    /// are drawn but not wired to anything yet -- the workspace search
    /// behind them is a plain substring match with no such flags to toggle.
    /// Right-alignment of the trailing icons is padded with monospace
    /// spaces, the same approximation `menu::item_text` uses to line up
    /// shortcuts: this renderer has no per-run alignment within a line, only
    /// per-region (see `TextEngine::set_icon_cluster`), so a precise right
    /// edge is not available here the way it is for an icon-only cluster.
    fn push_search_header(
        &self,
        rich: &mut crate::text::RichText,
        layout: &Layout,
        truncate: &impl Fn(&str) -> String,
    ) {
        use crate::icons::Icon;
        let dim = self.theme.dim_text;
        let digit = layout.metrics.digit_width.max(1.0);
        let margin = 16.0 * layout.scale;

        // Row 0: "SEARCH", right-aligned action icons.
        rich.colored("SEARCH", dim);
        let header_icons = [Icon::Refresh, Icon::ClearAll, Icon::NewFile, Icon::CollapseAll];
        let used = margin
            + "SEARCH".len() as f32 * digit
            + header_icons.len() as f32 * layout.metrics.icon_width;
        rich.plain(&" ".repeat((((layout.sidebar.width - used) / digit).max(0.0)) as usize));
        for icon in header_icons {
            rich.icon(icon, dim);
        }
        rich.newline();

        // Row 1: the search field -- a magnifier, the query (typed so far,
        // or the last submitted one while browsing results, or a dim
        // placeholder if there is neither), a caret only while actually
        // typing, then the case/word/regex toggles.
        let typing = self.search_query_input.is_some();
        let submitted = self.core.workspace_search_result().map(|result| result.query.as_str());
        let query = self.search_query_input.as_deref().or(submitted).unwrap_or("");
        let query_shown = truncate(query);
        rich.icon(Icon::Search, self.theme.activity_icon_active);
        rich.plain(" ");
        if query.is_empty() && !typing {
            rich.colored("Search", dim);
        } else {
            rich.colored(&query_shown, self.theme.text);
        }
        if typing {
            rich.colored("\u{2588}", self.theme.cursor);
        }
        let toggle_icons = [Icon::CaseSensitive, Icon::WholeWord, Icon::Regex];
        let used = margin
            + layout.metrics.icon_width
            + digit
            + query_shown.chars().count() as f32 * digit
            + toggle_icons.len() as f32 * layout.metrics.icon_width;
        rich.plain(&" ".repeat((((layout.sidebar.width - used) / digit).max(0.0)) as usize));
        for icon in toggle_icons {
            rich.icon(icon, dim);
        }
        rich.newline();

        // Row 2: the replace field -- not wired to anything (there is no
        // find-and-replace behind it yet), shown collapsed and dim the way
        // VS Code's own replace field reads before it has ever been typed
        // into.
        rich.icon(Icon::Replace, dim);
        rich.plain(" ");
        rich.colored("Replace", dim);
        let used = margin + layout.metrics.icon_width + digit + "Replace".len() as f32 * digit
            + layout.metrics.icon_width;
        rich.plain(&" ".repeat((((layout.sidebar.width - used) / digit).max(0.0)) as usize));
        rich.icon(Icon::Ellipsis, dim);
    }

    /// The activity bar's icon column: one glyph per cell, the active one
    /// bright and the rest dim, padded so each lands in the middle of the
    /// square cell its highlight quad occupies.
    /// One icon glyph per line, one line per activity item -- `set_icon_cluster`
    /// gives each line exactly one cell's worth of height, so this no longer
    /// needs the blank-line padding a fixed UI-text line height used to
    /// require to make one icon land in each square cell.
    fn activity_rich(&self) -> crate::text::RichText {
        let mut rich = crate::text::RichText::new();
        for (index, icon) in ACTIVITY_ICONS.iter().enumerate() {
            if index > 0 {
                rich.newline();
            }
            let color = if self.activity_active_index() == Some(index) {
                self.theme.activity_icon_active
            } else {
                self.theme.activity_icon_inactive
            };
            rich.icon(*icon, color);
        }
        rich
    }

    /// The bottom panel's icon rail. Only the terminal lives there today.
    fn bottom_panel_rail_rich(&self) -> crate::text::RichText {
        let mut rich = crate::text::RichText::new();
        if self.bottom_panel_visible() {
            rich.icon(crate::icons::Icon::Terminal, self.theme.activity_icon_active);
        }
        rich
    }

    /// The header's left cluster: the menu button that replaced the old
    /// File/Edit/View bar.
    fn title_left_rich(&self) -> crate::text::RichText {
        let mut rich = crate::text::RichText::new();
        rich.icon(crate::icons::Icon::Menu, self.theme.activity_icon_active);
        rich
    }

    /// The header's centered command field: a magnifier and the workspace
    /// name, the way Lapce shows the palette's entry point.
    fn title_center_rich(&self) -> crate::text::RichText {
        let mut rich = crate::text::RichText::new();
        let name = self
            .core
            .workspace()
            .root()
            .and_then(|root| {
                root.as_path().file_name().map(|name| name.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "Open Folder".to_string());
        rich.icon(crate::icons::Icon::Search, self.theme.dim_text);
        rich.plain(" ");
        rich.colored(&name, self.theme.text);
        rich
    }

    /// The tab row's leading navigation cluster.
    fn tab_nav_rich(&self) -> crate::text::RichText {
        let mut rich = crate::text::RichText::new();
        rich.icon(crate::icons::Icon::ArrowLeft, self.theme.activity_icon_inactive);
        rich.plain(" ");
        rich.icon(crate::icons::Icon::ArrowRight, self.theme.activity_icon_inactive);
        rich
    }

    /// The tab row's trailing split/close cluster.
    fn tab_actions_rich(&self) -> crate::text::RichText {
        let mut rich = crate::text::RichText::new();
        rich.icon(crate::icons::Icon::SplitHorizontal, self.theme.activity_icon_inactive);
        rich.plain(" ");
        rich.icon(crate::icons::Icon::Close, self.theme.activity_icon_inactive);
        rich
    }

    /// The header's right cluster: run and settings.
    fn title_right_rich(&self) -> crate::text::RichText {
        let mut rich = crate::text::RichText::new();
        rich.icon(crate::icons::Icon::Play, self.theme.activity_icon_active);
        rich.plain(" ");
        rich.icon(crate::icons::Icon::SettingsGear, self.theme.activity_icon_active);
        rich
    }

    /// The breadcrumb trail under the tab bar: the active document's path,
    /// one segment at a time, exactly as Lapce shows it.
    fn breadcrumb_rich(&self) -> crate::text::RichText {
        let mut rich = crate::text::RichText::new();
        let Some(document) = self.core.active_document() else { return rich };
        let Some(path) = document.path() else {
            rich.colored(document.display_name(), self.theme.dim_text);
            return rich;
        };
        // Relative to the workspace root when there is one, so the trail
        // reads as project structure rather than as a disk path.
        let full = path.as_path();
        let relative = self
            .core
            .workspace()
            .root()
            .and_then(|root| full.strip_prefix(root.as_path()).ok())
            .unwrap_or(full);
        let mut first = true;
        for segment in relative.components() {
            let text = segment.as_os_str().to_string_lossy();
            if !first {
                rich.plain(" ");
                rich.icon(crate::icons::Icon::ChevronRight, self.theme.dim_text);
                rich.plain(" ");
            }
            first = false;
            rich.colored(&text, self.theme.dim_text);
        }
        rich
    }

    /// Which sidebar row is selected, in the row numbering `sidebar_rows`
    /// uses (the header is row 0). `None` while typing a query: the input
    /// line is not a "selection" in the list sense.
    fn sidebar_selected_row(&self) -> Option<usize> {
        // A typed query no longer suppresses the highlight: the arrow keys
        // walk the live results underneath the field, so the row they are on
        // has to be visible as the selection.
        self.active_list.map(|_| self.list_selected + self.sidebar_header_lines())
    }

    /// How many lines at the top of the sidebar's row buffer are its own
    /// header rather than a list entry. A plain title is one line; the
    /// Search panel's own header (title + action icons, the search field,
    /// the replace field -- see `push_search_header`) is three, in either of
    /// its two states (typing a query, or browsing results), since the
    /// header block is drawn the same either way.
    fn sidebar_header_lines(&self) -> usize {
        if self.showing_search_panel() {
            3
        } else {
            1
        }
    }

    /// Whether the sidebar is showing the Search panel -- either because a
    /// query is being typed, or because its results are on screen.
    fn showing_search_panel(&self) -> bool {
        self.search_query_input.is_some() || self.active_list == Some(ListKind::SearchResults)
    }

    /// Puts the keyboard in the search field, restoring the query that
    /// produced the results on screen so it can be refined rather than
    /// retyped.
    fn focus_search_field(&mut self) {
        if self.search_query_input.is_none() {
            let previous = self
                .core
                .workspace_search_result()
                .map(|result| result.query.clone())
                .unwrap_or_default();
            self.search_query_input = Some(previous);
        }
        self.active_list = Some(ListKind::SearchResults);
        self.focus = InputFocus::SearchQuery;
        self.request_redraw();
    }

    /// Rows for the floating performance/dev-tool panel, top-right. The
    /// explorer/search/git list lives in the docked sidebar instead (see
    /// `sidebar_rows`), and the terminal lives in the docked bottom panel
    /// (see `bottom_panel_rows`) -- neither belongs here any more; this
    /// panel is diagnostics, not the UI the user works in day to day.
    fn panel_rows(&self) -> Vec<String> {
        let mut rows = Vec::new();
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
            self.sidebar_width,
            self.bottom_panel_visible(),
            self.bottom_panel_height,
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
            layout.title_menu_button,
            self.menu,
            layout.metrics.digit_width,
            layout.metrics.line_height,
            layout.scale,
            &recent_rows,
        );
        self.menu_geometry = Some(geometry.clone());
        let palette_rows = self.command_palette_rows();
        self.palette_geometry = self.command_palette_open.then(|| {
            crate::palette::geometry(
                layout.window,
                palette_rows.len(),
                layout.metrics.line_height,
                layout.scale,
            )
        });
        let palette_rich = self.command_palette_open.then(|| self.palette_rich(&palette_rows));
        // The selected row is only meaningful up to the same visible cap the
        // panel itself and `palette_rich` both already clamp to.
        let palette_selected = self
            .command_palette_open
            .then_some(self.command_palette_selected)
            .filter(|_| !palette_rows.is_empty());
        if self.overlay_visible {
            self.build_overlay();
        }
        if self.dev_panel_visible {
            self.dev_panel_rows = devpanel::lines(&self.core, &self.heartbeat);
        }
        if self.resource_center_visible {
            self.resource_center_rows =
                resources::lines(&self.core, &self.process_stats, &self.lsp.running());
        }

        let panel_rows = self.panel_rows();
        let sidebar_rich = self.sidebar_rows(&layout);
        // Clamped here rather than only where the wheel mutates it: the row
        // list itself can shrink between frames (a directory collapses, a
        // search returns fewer hits) and leave a stale scroll position
        // pointing past the end of a now-shorter list.
        let sidebar_total_height =
            (sidebar_rich.text.matches('\n').count() + 1) as f32 * layout.metrics.line_height;
        let sidebar_max_scroll = (sidebar_total_height - layout.sidebar.height).max(0.0);
        self.sidebar_scroll_y = self.sidebar_scroll_y.clamp(0.0, sidebar_max_scroll);
        let sidebar_selected_row = self.sidebar_selected_row();
        let status_left = self.status_left();
        let status_right = self.status_right();
        let status_color = self.status_color();
        // One tab computation per frame, shared by drawing and hit testing.
        // Storing it here is what keeps a click resolving against the same
        // rectangles the user is looking at.
        let now = Instant::now();
        let mut presentations = self.core.tab_presentations();
        presentations.retain(|tab| {
            !tab.loading
                || self
                    .loading_tab_started
                    .get(&tab.id)
                    .is_none_or(|started| should_show_loading_tab(*started, now))
        });
        self.tab_geometry = tabs::geometry(
            layout.tab_bar,
            &presentations,
            layout.metrics.digit_width,
            layout.metrics.material_icon_width,
            layout.scale,
        );
        let tabs = self.tab_geometry.clone();
        let view =
            self.core.active().and_then(|id| self.views.get(&id).copied()).unwrap_or_default();

        let menu_enabled: Vec<bool> = if self.menu.open.is_some() {
            let mut enabled: Vec<bool> =
                menu::all_items().iter().map(|item| menu::is_enabled(&self.core, item)).collect();
            // Opening a recent file is always allowed; only the registry
            // command items above are ever refused.
            enabled.extend(std::iter::repeat_n(true, recent_rows.len()));
            enabled
        } else {
            Vec::new()
        };
        let prompt_text = self.banner_text();
        if self.dependency_view {
            self.ensure_dependency_scene(&layout);
        }
        let bottom_panel_rows =
            if self.bottom_panel_visible() { self.bottom_panel_rows(&layout) } else { Vec::new() };
        let activity_active = self.activity_active_index();
        let activity = self.activity_rich();
        let bottom_panel_rail = self.bottom_panel_rail_rich();
        let title_left = self.title_left_rich();
        let title_center = self.title_center_rich();
        let title_right = self.title_right_rich();
        let breadcrumb = self.breadcrumb_rich();
        let tab_nav = self.tab_nav_rich();
        let tab_actions = self.tab_actions_rich();
        let menu_state = self.menu;
        let empty_geometry =
            menu::MenuGeometry { titles: Vec::new(), dropdown: None, items: Vec::new() };
        let menu_geometry = self.menu_geometry.as_ref().unwrap_or(&empty_geometry);
        let caret_visible = self.caret_visible;

        // The settings screen is measured here, once, so the click handler
        // and the renderer both read the same geometry.
        let settings_visible = self.settings_open.then(|| self.visible_settings());
        let settings_rows = settings_visible.as_ref().map(|visible| {
            crate::settings_ui::rows(
                visible,
                &self.settings,
                self.settings_editing.as_ref().map(|(key, draft)| (*key, draft.as_str())),
            )
        });
        if let Some(visible) = settings_visible.as_ref() {
            self.settings_screen = crate::settings_ui::layout(
                layout.text,
                layout.metrics.line_height,
                layout.metrics.digit_width,
                visible,
                &self.settings,
                self.settings_scroll,
            );
        }
        let settings_frame = match (settings_visible.as_ref(), settings_rows.as_ref()) {
            (Some(visible), Some(rows)) => Some(crate::renderer::SettingsFrame {
                screen: &self.settings_screen,
                rows,
                visible,
                values: &self.settings,
                query: &self.settings_query,
                query_focused: self.settings_search_focused,
                section: self.settings_section,
                editing: self
                    .settings_editing
                    .as_ref()
                    .map(|(key, draft)| (*key, draft.as_str())),
                scroll_rows: self.settings_scroll,
                workspace_scope: self.settings_scope == SettingsScope::Workspace,
            }),
            _ => None,
        };

        let dependency_placeholder = self.dependency_placeholder();
        let dependency = self.dependency_view.then_some(crate::renderer::DependencyFrame {
            scene: self.dependency_scene.as_ref(),
            view: self.dependency_view_at,
            traced: self.dependency_hovered,
            placeholder: dependency_placeholder,
        });

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
            sidebar: (!sidebar_rich.is_empty()).then_some(&sidebar_rich),
            sidebar_selected_row,
            sidebar_hovered_row: self.sidebar_hovered_row,
            sidebar_scroll_y: self.sidebar_scroll_y,
            palette: palette_rich.as_ref(),
            palette_panel: self.palette_geometry,
            palette_selected,
            palette_hovered: self.command_palette_hovered,
            activity: &activity,
            activity_active,
            activity_hovered: self.activity_hovered,
            bottom_panel: (!bottom_panel_rows.is_empty()).then_some(bottom_panel_rows.as_slice()),
            bottom_panel_rail: &bottom_panel_rail,
            title_left: &title_left,
            title_center: &title_center,
            title_right: &title_right,
            breadcrumb: &breadcrumb,
            tab_nav: &tab_nav,
            tab_actions: &tab_actions,
            dependency,
            settings: settings_frame,
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

        // Settings are read once the renderer exists, because applying them
        // reaches the font, which the renderer owns.
        self.reload_settings();
        self.apply_settings();

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
        self.tick_search_debounce();
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
            UserEvent::FolderPicked(result) => self.folder_picked(*result),
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

                // Ctrl+, opens the settings from anywhere, the same standing
                // the command palette's shortcut has below. Routing it
                // through the editor keymap meant it did nothing whenever
                // the explorer or the terminal had the keyboard, which is
                // most of the time somebody wants it.
                if !self.settings_open
                    && self.modifiers.control_key()
                    && !self.modifiers.shift_key()
                    && matches!(&event.logical_key, winit::keyboard::Key::Character(text) if text.as_str() == ",")
                {
                    self.toggle_settings();
                    return;
                }

                // The settings screen owns the keyboard while it is up:
                // typing goes to the search box or to the field being
                // edited, and neither should reach the document behind it.
                // Checked after the prompt and before everything else, the
                // same standing the palette has below.
                if self.settings_open && self.settings_key(&event.logical_key) {
                    return;
                }

                // Ctrl+Shift+P opens the palette from anywhere -- checked
                // ahead of every focus-specific handler below, the same
                // priority a confirmation prompt gets, since a command
                // palette that only opened from some focuses would not be
                // the "run anything from anywhere" tool it is meant to be.
                let modifiers_for_palette = winit::event::Modifiers::from(self.modifiers);
                if keymap::is_command_palette_shortcut(&event.logical_key, &modifiers_for_palette)
                {
                    self.open_command_palette();
                    return;
                }

                // The palette owns the keyboard while it is open: typing
                // filters it, arrows move the highlight, Enter runs the
                // highlighted command.
                if self.focus == InputFocus::CommandPalette {
                    match &event.logical_key {
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape) => {
                            self.close_command_palette();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Enter) => {
                            self.run_palette_selection();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowDown) => {
                            // Capped to what the palette actually shows --
                            // there is no scrolling, so selection must never
                            // move past the last visible row.
                            let count = self
                                .command_palette_rows()
                                .len()
                                .min(crate::palette::MAX_VISIBLE_ROWS);
                            if count > 0 {
                                self.command_palette_selected =
                                    (self.command_palette_selected + 1).min(count - 1);
                            }
                            self.request_redraw();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowUp) => {
                            self.command_palette_selected =
                                self.command_palette_selected.saturating_sub(1);
                            self.request_redraw();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Backspace) => {
                            self.command_palette_query.pop();
                            self.command_palette_selected = 0;
                            self.request_redraw();
                        }
                        _ => {
                            if let Some(text) = event.text.as_ref() {
                                let printable: String =
                                    text.chars().filter(|c| !c.is_control()).collect();
                                if !printable.is_empty() {
                                    self.command_palette_query.push_str(&printable);
                                    self.command_palette_selected = 0;
                                    self.request_redraw();
                                }
                            }
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
                            self.terminal_backspace();
                            self.request_redraw();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Delete) => {
                            self.terminal_delete_forward();
                            self.request_redraw();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowUp) => {
                            self.terminal_history_up();
                            self.request_redraw();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowDown) => {
                            self.terminal_history_down();
                            self.request_redraw();
                        }
                        // Recall (Up/Down) walks entire commands; within one
                        // line, Left/Right move the caret over it rather than
                        // being left to fall into the default arm below and
                        // do nothing, which is what made the caret look
                        // stuck in place.
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowLeft) => {
                            self.terminal_move_left();
                            self.request_redraw();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowRight) => {
                            self.terminal_move_right();
                            self.request_redraw();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Home) => {
                            self.terminal_cursor = 0;
                            self.request_redraw();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::End) => {
                            self.terminal_cursor = self.terminal_input.chars().count();
                            self.request_redraw();
                        }
                        _ => {
                            if let Some(text) = event.text.as_ref() {
                                let printable: String =
                                    text.chars().filter(|c| !c.is_control()).collect();
                                if !printable.is_empty() {
                                    self.terminal_insert(&printable);
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

                // Typing a workspace-search query. Results follow the query
                // as it is typed (see `schedule_search_debounce`), so the
                // arrow keys walk the results underneath the field and Enter
                // opens the highlighted one -- there is nothing left for
                // Enter to "submit".
                if self.focus == InputFocus::SearchQuery {
                    match &event.logical_key {
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape) => {
                            self.search_query_input = None;
                            self.search_debounce_deadline = None;
                            self.refresh_focus();
                            self.request_redraw();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Backspace) => {
                            if let Some(query) = self.search_query_input.as_mut() {
                                query.pop();
                            }
                            self.schedule_search_debounce();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Enter) => {
                            // Enter on a highlighted result opens it; with no
                            // results yet it forces the pending search to run
                            // now rather than waiting out the debounce.
                            if self.search_result_rows() == 0 {
                                self.run_pending_search();
                            } else {
                                self.activate_list_selection();
                            }
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowDown) => {
                            let rows = self.search_result_rows();
                            if rows > 0 {
                                self.list_selected = (self.list_selected + 1).min(rows - 1);
                            }
                            self.request_redraw();
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowUp) => {
                            self.list_selected = self.list_selected.saturating_sub(1);
                            self.request_redraw();
                        }
                        _ => {
                            if let Some(text) = event.text.as_ref() {
                                let printable: String =
                                    text.chars().filter(|c| !c.is_control()).collect();
                                if !printable.is_empty() {
                                    if let Some(query) = self.search_query_input.as_mut() {
                                        query.push_str(&printable);
                                    }
                                    self.schedule_search_debounce();
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
                    WheelTarget::Editor | WheelTarget::Sidebar | WheelTarget::BottomPanel => {}
                }
                if target == WheelTarget::BottomPanel {
                    // Positive `dy` is a scroll *up*, which in a terminal
                    // means back into history.
                    match delta {
                        MouseScrollDelta::LineDelta(_, dy) => {
                            self.scroll_terminal_by_lines(dy * WHEEL_LINES);
                        }
                        MouseScrollDelta::PixelDelta(position) => {
                            let line_height = self
                                .last_layout
                                .map(|layout| layout.metrics.line_height)
                                .unwrap_or(20.0);
                            self.scroll_terminal_by_lines(position.y as f32 / line_height.max(1.0));
                        }
                    }
                    return;
                }
                if target == WheelTarget::Sidebar {
                    match delta {
                        MouseScrollDelta::LineDelta(_, dy) => {
                            let step = self
                                .last_layout
                                .map(|layout| layout.metrics.line_height)
                                .unwrap_or(20.0);
                            self.scroll_sidebar_by_pixels(dy * WHEEL_LINES * step);
                        }
                        MouseScrollDelta::PixelDelta(position) => {
                            self.scroll_sidebar_by_pixels(position.y as f32);
                        }
                    }
                    return;
                }
                if self.settings_open {
                    let rows = match delta {
                        MouseScrollDelta::LineDelta(_, dy) => dy * 3.0,
                        MouseScrollDelta::PixelDelta(position) => {
                            position.y as f32
                                / self
                                    .last_layout
                                    .map(|layout| layout.metrics.line_height)
                                    .unwrap_or(20.0)
                        }
                    };
                    self.scroll_settings(rows);
                    return;
                }
                if self.dependency_view {
                    // A graph canvas zooms under the wheel rather than
                    // scrolling: the picture is already fitted to the pane,
                    // so there is nothing above or below it to scroll to,
                    // and getting closer is the thing a reader wants.
                    if let Some(layout) = self.last_layout {
                        let notches = match delta {
                            MouseScrollDelta::LineDelta(_, dy) => dy,
                            MouseScrollDelta::PixelDelta(position) => position.y as f32 / 60.0,
                        };
                        if notches != 0.0 {
                            self.zoom_dependency_view(ZOOM_PER_NOTCH.powf(notches), &layout);
                        }
                    }
                    return;
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
                if self.dependency_view {
                    let (x, y) = self.pointer;
                    if let Some(layout) = self.last_layout {
                        if self.dependency_press.is_some() {
                            self.drag_dependency_view(x, y, &layout);
                        } else {
                            self.hover_dependency_view(x, y, &layout);
                        }
                    }
                    return;
                }
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
                } else if self.dragging_sidebar {
                    if let Some(layout) = self.last_layout {
                        let requested = (self.pointer.0 - layout.sidebar.x) / layout.scale;
                        self.sidebar_width = crate::layout::clamp_sidebar_width(requested);
                        self.request_redraw();
                    }
                } else if self.dragging_bottom_panel {
                    if let Some(layout) = self.last_layout {
                        let requested =
                            (layout.bottom_panel.bottom() - self.pointer.1) / layout.scale;
                        self.bottom_panel_height =
                            crate::layout::clamp_bottom_panel_height(requested);
                        self.request_redraw();
                    }
                } else if let Some(layout) = self.last_layout {
                    self.update_pointer_interaction(&layout);
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
                        self.release_dependency_view(x, y);
                        self.dragging_selection = false;
                        self.dragging_scrollbar = false;
                        self.dragging_sidebar = false;
                        self.dragging_bottom_panel = false;
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
            FontMetrics {
                font_size: 14.0,
                line_height: 20.0,
                digit_width: 8.0,
                icon_width: 14.0,
                material_icon_width: 14.0,
            },
            4,
            true,
            true,
            false,
            crate::layout::SIDEBAR_WIDTH,
            false,
            crate::layout::BOTTOM_PANEL_HEIGHT,
        )
    }

    fn centre(rect: crate::layout::Rect) -> (f32, f32) {
        (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0)
    }

    #[test]
    fn opening_a_file_from_a_still_open_explorer_keeps_editor_focus() {
        // Regression test for "I could only type 1 character and then it
        // stopped": opening a file focuses the editor, but the explorer
        // stays open (a docked panel now, not a modal picker). The very next
        // `refresh_focus` call -- triggered by nothing more than the load
        // finishing -- used to see the still-open panel and hand focus
        // straight back to it, so every keystroke after the first went to
        // the list's Up/Down/Enter handler instead of the document.
        let focus =
            derive_focus(InputFocus::Editor, false, false, false, false, true, false, false);
        assert_eq!(
            focus,
            InputFocus::Editor,
            "an open panel must not steal a resting Editor focus"
        );
    }

    #[test]
    fn a_resting_focus_survives_an_unrelated_refresh() {
        // Diagnostics arriving, external-change polling, terminal output --
        // none of these should be able to bounce focus away from wherever
        // the user actually is, as long as that surface is still open.
        assert_eq!(
            derive_focus(InputFocus::List, false, false, false, false, true, false, false),
            InputFocus::List
        );
        assert_eq!(
            derive_focus(InputFocus::Terminal, false, false, false, false, false, true, false),
            InputFocus::Terminal
        );
    }

    #[test]
    fn a_resting_focus_falls_back_to_the_editor_once_its_surface_closes() {
        assert_eq!(
            derive_focus(InputFocus::List, false, false, false, false, false, false, false),
            InputFocus::Editor
        );
        assert_eq!(
            derive_focus(InputFocus::Terminal, false, false, false, false, false, false, false),
            InputFocus::Editor
        );
    }

    #[test]
    fn entering_list_or_terminal_focus_is_never_derived_only_explicit() {
        // The panel being open is not, by itself, enough to grant it focus --
        // that has to come from `focus_list`/`focus_terminal`. Otherwise
        // simply opening the explorer while the user is mid-edit would yank
        // the keyboard away without them asking for it.
        assert_eq!(
            derive_focus(InputFocus::Editor, false, false, false, false, true, false, false),
            InputFocus::Editor
        );
        assert_eq!(
            derive_focus(InputFocus::Editor, false, false, false, false, false, true, false),
            InputFocus::Editor
        );
    }

    #[test]
    fn the_exclusive_surfaces_always_win_over_a_resting_focus() {
        // SearchQuery is deliberately not here: unlike Prompt/Menu/Find/the
        // palette, it is a resting focus too now (see the next test) --
        // typing a query must not be able to steal focus back from
        // somewhere the user has since clicked, only the click itself grants
        // it, the same as List and Terminal.
        for (prompt, menu, find, palette) in [
            (true, false, false, false),
            (false, true, false, false),
            (false, false, true, false),
            (false, false, false, true),
        ] {
            let focus =
                derive_focus(InputFocus::List, prompt, menu, false, find, true, false, palette);
            assert_ne!(focus, InputFocus::List, "an exclusive surface must win over a resting one");
        }
    }

    #[test]
    fn a_typed_search_query_cannot_steal_focus_back_from_a_click() {
        // Regression test: `search_query_open` alone used to force
        // `SearchQuery` focus on every `refresh_focus` call, so clicking into
        // the editor while a query was typed but not yet submitted got
        // silently reverted the instant anything else refreshed (a
        // diagnostics update, a heartbeat tick) -- the editor read as
        // permanently stuck until Escape.
        assert_eq!(
            derive_focus(InputFocus::Editor, false, false, true, false, false, false, false),
            InputFocus::Editor,
            "a query being open must not be able to grab focus the click already left"
        );
        assert_eq!(
            derive_focus(InputFocus::SearchQuery, false, false, true, false, false, false, false),
            InputFocus::SearchQuery,
            "but it is still a resting focus: unrelated refreshes must not evict it either"
        );
        assert_eq!(
            derive_focus(InputFocus::SearchQuery, false, false, false, false, false, false, false),
            InputFocus::Editor,
            "falls back to the editor once the query itself closes"
        );
    }

    #[test]
    fn a_load_that_finishes_inside_the_grace_period_never_shows_a_tab() {
        // Regression test for the "the file just opens and then closes" bug:
        // rejecting a binary file happens fast enough that the loading tab
        // used to flash into existence and vanish within milliseconds. A
        // load still running once the grace period has elapsed should show
        // normally -- this only suppresses the flash, not the tab itself.
        let started = Instant::now();
        assert!(
            !should_show_loading_tab(started, started),
            "a load that hasn't been running at all must not show a tab yet"
        );
        assert!(
            !should_show_loading_tab(started, started + Duration::from_millis(50)),
            "well inside the grace period"
        );
        assert!(
            should_show_loading_tab(started, started + LOADING_TAB_GRACE),
            "exactly at the grace period boundary, the tab should show"
        );
        assert!(
            should_show_loading_tab(started, started + Duration::from_secs(2)),
            "a genuinely slow load must still show its tab"
        );
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
    fn the_wheel_scrolls_the_sidebar_regardless_of_focus() {
        // Regression test: the sidebar had no wheel target at all, so a
        // file tree taller than the panel could never be scrolled. The
        // sidebar is non-modal chrome (see `derive_focus`), so this must work
        // whether the editor still has focus or a click has already claimed
        // `InputFocus::List`.
        let layout = Layout::with_chrome(
            1000.0,
            700.0,
            1.0,
            FontMetrics {
                font_size: 14.0,
                line_height: 20.0,
                digit_width: 8.0,
                icon_width: 14.0,
                material_icon_width: 14.0,
            },
            4,
            true,
            true,
            true,
            crate::layout::SIDEBAR_WIDTH,
            false,
            crate::layout::BOTTOM_PANEL_HEIGHT,
        );
        let (x, y) = centre(layout.sidebar);
        assert_eq!(wheel_target(&layout, InputFocus::Editor, x, y), WheelTarget::Sidebar);
        assert_eq!(wheel_target(&layout, InputFocus::List, x, y), WheelTarget::Sidebar);
    }

    #[test]
    fn the_cursor_is_visible_at_the_end_of_an_empty_or_full_line() {
        // Regression test for "cursor is not visible in the terminal": the
        // input row used to be plain `"> {input}"` with nothing marking
        // where typing would land.
        assert_eq!(terminal_input_row("", 0), "> \u{2588}");
        assert_eq!(terminal_input_row("cd Light", 8), "> cd Light\u{2588}");
    }

    #[test]
    fn the_cursor_splices_in_wherever_it_actually_is_not_just_the_end() {
        // Regression test for "I cannot move cursor back using arrow keys":
        // this is the render-side half of that fix -- the cursor must show
        // up mid-line, not only ever at the end.
        assert_eq!(terminal_input_row("cd Light", 2), "> cd\u{2588} Light");
        assert_eq!(terminal_input_row("cd Light", 0), "> \u{2588}cd Light");
    }

    #[test]
    fn the_cursor_never_splits_a_multibyte_character() {
        // `terminal_char_boundary` counts characters, not bytes -- a cursor
        // landing on a raw byte offset instead could splice a block glyph
        // into the middle of an accented letter's or an emoji's encoding.
        let line = "caf\u{e9} \u{1f389}party"; // "café 🎉party"
        assert_eq!(terminal_input_row(line, 4), "> caf\u{e9}\u{2588} \u{1f389}party");
        assert_eq!(terminal_input_row(line, 5), "> caf\u{e9} \u{2588}\u{1f389}party");
        assert_eq!(terminal_input_row(line, 6), "> caf\u{e9} \u{1f389}\u{2588}party");
    }

    #[test]
    fn a_cursor_index_past_the_end_of_the_line_clamps_to_the_end() {
        assert_eq!(terminal_input_row("hi", 50), "> hi\u{2588}");
    }

    #[test]
    fn the_terminal_shows_as_many_lines_as_its_panel_actually_has_room_for() {
        // The bug this replaces: a hardcoded 12 lines regardless of the
        // panel's size, so dragging it taller showed the same twelve with
        // empty space under them, and a short panel drew rows that were
        // clipped off the bottom.
        let mut layout = layout();
        layout.metrics.line_height = 20.0;

        layout.bottom_panel.height = 20.0 * 10.0;
        assert_eq!(
            terminal_visible_lines(&layout),
            8,
            "ten rows of space, less the header and the input line"
        );

        layout.bottom_panel.height = 20.0 * 30.0;
        assert_eq!(terminal_visible_lines(&layout), 28, "a taller panel shows more");
    }

    #[test]
    fn a_terminal_panel_too_small_for_its_own_chrome_still_shows_a_line() {
        // Dragged shut to nothing, the arithmetic must not underflow into a
        // huge count or report zero visible lines.
        let mut layout = layout();
        layout.metrics.line_height = 20.0;
        layout.bottom_panel.height = 0.0;
        assert_eq!(terminal_visible_lines(&layout), 1);
        layout.bottom_panel.height = 20.0;
        assert_eq!(terminal_visible_lines(&layout), 1);
    }

    #[test]
    fn the_terminal_takes_the_wheel_even_while_it_holds_the_keyboard() {
        // Scrolling back through output must not require clicking out of the
        // terminal first -- and `InputFocus::Terminal` otherwise falls into
        // the "a modal surface owns input" arm, which swallows the wheel.
        let mut layout = layout();
        layout.bottom_panel_visible = true;
        layout.bottom_panel = crate::layout::Rect::new(300.0, 400.0, 500.0, 200.0);
        let (x, y) = centre(layout.bottom_panel);
        assert_eq!(
            wheel_target(&layout, InputFocus::Terminal, x, y),
            WheelTarget::BottomPanel
        );
        assert_eq!(
            wheel_target(&layout, InputFocus::Editor, x, y),
            WheelTarget::BottomPanel,
            "and without having to focus it at all"
        );
    }

    #[test]
    fn pressing_up_on_an_empty_history_does_nothing() {
        assert_eq!(navigate_terminal_history(&[], None, HistoryDirection::Older), None);
    }

    #[test]
    fn up_from_a_live_line_recalls_the_newest_command_first() {
        // The shell convention this must match: the first Up press shows the
        // *last* thing run, not the first thing ever run.
        let history = vec!["git status".to_string(), "cargo build".to_string(), "ls".to_string()];
        assert_eq!(navigate_terminal_history(&history, None, HistoryDirection::Older), Some(2));
    }

    #[test]
    fn repeated_up_walks_toward_the_oldest_entry_and_stops_there() {
        let history = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        assert_eq!(navigate_terminal_history(&history, Some(2), HistoryDirection::Older), Some(1));
        assert_eq!(navigate_terminal_history(&history, Some(1), HistoryDirection::Older), Some(0));
        assert_eq!(
            navigate_terminal_history(&history, Some(0), HistoryDirection::Older),
            Some(0),
            "the oldest entry is the floor, not a place to run off the front of the list"
        );
    }

    #[test]
    fn down_walks_toward_the_newest_entry_then_back_to_the_live_line() {
        let history = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        assert_eq!(navigate_terminal_history(&history, Some(0), HistoryDirection::Newer), Some(1));
        assert_eq!(navigate_terminal_history(&history, Some(1), HistoryDirection::Newer), Some(2));
        assert_eq!(
            navigate_terminal_history(&history, Some(2), HistoryDirection::Newer),
            None,
            "past the newest entry is the live line, not a wrap back to the oldest"
        );
    }

    #[test]
    fn down_on_a_live_line_that_was_never_recalled_stays_on_it() {
        // `current: None` with nothing to walk back to must not manufacture
        // an index -- this is the case `App::terminal_history_down` guards
        // against actually applying, since here it would otherwise look
        // identical to "walked past the newest entry".
        let history = vec!["one".to_string()];
        assert_eq!(navigate_terminal_history(&history, None, HistoryDirection::Newer), None);
    }

    #[test]
    fn clicking_the_search_field_focuses_it_instead_of_the_results_list() {
        // Regression test for "nothing happens when I click on the search
        // button". Making the results list live while the query is still
        // being typed put `active_list` in `Some(SearchResults)`, which
        // brought the sidebar's click branch to life across the whole panel
        // -- including the search field itself. Clicking the field claimed
        // list focus, so every keystroke after that went to the results
        // instead of the box the caret was blinking in.
        assert_eq!(sidebar_click(1, 3, true), SidebarClick::SearchField, "the query field");
        assert_eq!(sidebar_click(2, 3, true), SidebarClick::SearchField, "the replace field");
        assert_eq!(sidebar_click(0, 3, true), SidebarClick::SearchField, "the title row");
    }

    #[test]
    fn clicking_past_the_search_header_still_picks_a_result() {
        // The fix must not swallow clicks on the results themselves.
        assert_eq!(sidebar_click(3, 3, true), SidebarClick::Row(0));
        assert_eq!(sidebar_click(7, 3, true), SidebarClick::Row(4));
    }

    #[test]
    fn an_ordinary_panels_title_row_is_not_a_text_field() {
        // The explorer and git panels have a caption, not an input: clicking
        // it must not try to focus something that does not exist.
        assert_eq!(sidebar_click(0, 1, false), SidebarClick::Header);
        assert_eq!(sidebar_click(1, 1, false), SidebarClick::Row(0));
        assert_eq!(sidebar_click(4, 1, false), SidebarClick::Row(3));
    }

    #[test]
    fn results_for_a_query_already_typed_past_are_not_shown() {
        // The failure this prevents: type "needle", and the walk dispatched
        // for "nee" three keystrokes ago lands first and paints its hits
        // under a field that says "needle". Both are real results; only one
        // of them is for what the user is looking at.
        assert!(search_result_is_current("needle", Some("needle")));
        assert!(!search_result_is_current("nee", Some("needle")), "a stale prefix must not show");
        assert!(
            !search_result_is_current("needles", Some("needle")),
            "nor a query the user has since backspaced past"
        );
    }

    #[test]
    fn a_dismissed_search_field_still_shows_whatever_last_completed() {
        // The field is gone but the results panel is still up: there is no
        // "current query" to disagree with, so the last result stands.
        assert!(search_result_is_current("needle", None));
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
        assert_eq!(
            dir_row.chevron,
            Some(crate::icons::Icon::ChevronRight),
            "a collapsed directory points its chevron sideways"
        );
        assert_eq!(dir_row.icon, Some(crate::icons::Icon::Folder.into()));
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
        assert_eq!(
            dir_row.chevron,
            Some(crate::icons::Icon::ChevronDown),
            "an expanded directory points its chevron down"
        );
        assert_eq!(dir_row.icon, Some(crate::icons::Icon::FolderOpened.into()));
        assert_eq!(dir_row.depth, 0);
        let file_row = &rows[1];
        assert!(file_row.label.contains("main.rs"));
        assert_eq!(file_row.depth, 1, "a child sits one level deeper than its parent");
        assert_eq!(file_row.chevron, None, "a file has nothing to expand");
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
