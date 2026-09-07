//! Command registry (specification section 12).
//!
//! Commands are the single routing mechanism for application actions. A key
//! binding, a menu item, a future command palette and a future automation
//! client all reach the editor the same way: by naming a command id. UI code
//! never implements an application action itself.
//!
//! The registry is a static table of function pointers, so dispatch costs a hash
//! lookup and an indirect call, and every command is enumerable - which is what
//! a command palette and a keybinding editor will need.

use crate::editor::EditorCore;
use crate::error::EditorError;
use crate::selection::Movement;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Arguments a command may need. Most commands take none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CommandArgs {
    #[default]
    None,
    /// Text to insert or paste.
    Text(String),
    /// Target of an open or save-as.
    Path(PathBuf),
    /// Target of a "go to line".
    Position { line: usize, column: usize },
    /// A 1-based position: a tab number (Ctrl+1..9) or a recent-files slot.
    Index(usize),
}

impl CommandArgs {
    pub fn text(&self) -> Option<&str> {
        match self {
            CommandArgs::Text(text) => Some(text),
            _ => None,
        }
    }

    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            CommandArgs::Path(path) => Some(path),
            _ => None,
        }
    }
}

pub type CommandFn = fn(&mut EditorCore, CommandArgs) -> Result<(), EditorError>;
pub type EnabledFn = fn(&EditorCore) -> bool;

/// One application action.
pub struct CommandDescriptor {
    pub id: &'static str,
    pub display_name: &'static str,
    pub enabled: EnabledFn,
    pub execute: CommandFn,
}

impl std::fmt::Debug for CommandDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandDescriptor").field("id", &self.id).finish()
    }
}

fn always(_core: &EditorCore) -> bool {
    true
}

fn has_document(core: &EditorCore) -> bool {
    core.active_document().is_some()
}

fn has_selection(core: &EditorCore) -> bool {
    core.active_document().is_some_and(|document| !document.selections().primary().is_caret())
}

fn can_undo(core: &EditorCore) -> bool {
    core.active_document().is_some_and(|document| document.can_undo())
}

fn can_redo(core: &EditorCore) -> bool {
    core.active_document().is_some_and(|document| document.can_redo())
}

fn is_dirty_or_untitled(core: &EditorCore) -> bool {
    core.active_document().is_some_and(|document| document.is_dirty() || document.path().is_none())
}

fn has_find_matches(core: &EditorCore) -> bool {
    core.find_state().is_some_and(|find| !find.matches().is_empty())
}

/// Defines the move/select pair for one cursor movement.
macro_rules! movement_pair {
    ($move_fn:ident, $select_fn:ident, $movement:expr) => {
        fn $move_fn(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
            core.move_cursor($movement, false)
        }
        fn $select_fn(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
            core.move_cursor($movement, true)
        }
    };
}

movement_pair!(cursor_left, select_left, Movement::CharLeft);
movement_pair!(cursor_right, select_right, Movement::CharRight);
movement_pair!(cursor_up, select_up, Movement::LineUp);
movement_pair!(cursor_down, select_down, Movement::LineDown);
movement_pair!(cursor_word_left, select_word_left, Movement::WordLeft);
movement_pair!(cursor_word_right, select_word_right, Movement::WordRight);
movement_pair!(cursor_line_start, select_line_start, Movement::LineStartSmart);
movement_pair!(cursor_line_end, select_line_end, Movement::LineEnd);
movement_pair!(cursor_document_start, select_document_start, Movement::DocumentStart);
movement_pair!(cursor_document_end, select_document_end, Movement::DocumentEnd);
movement_pair!(cursor_page_up, select_page_up, Movement::PageUp);
movement_pair!(cursor_page_down, select_page_down, Movement::PageDown);

fn file_new(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.new_document();
    Ok(())
}

fn file_open(core: &mut EditorCore, args: CommandArgs) -> Result<(), EditorError> {
    match args.path() {
        // The core cannot show a dialog; it asks the shell for a path.
        None => core.request_shell(ShellRequest::OpenFileDialog),
        Some(path) => {
            if let Err(error) = core.open_document(path) {
                core.report_open_failure(error);
            }
        }
    }
    Ok(())
}

fn file_save(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.save_active();
    Ok(())
}

fn file_save_as(core: &mut EditorCore, args: CommandArgs) -> Result<(), EditorError> {
    match args.path() {
        None => core.request_shell(ShellRequest::SaveAsDialog),
        Some(path) => core.save_active_as(path.clone()),
    }
    Ok(())
}

fn file_close_tab(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    if let Some(id) = core.active() {
        core.close_document(id)?;
    }
    Ok(())
}

fn file_close_all_clean_tabs(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.close_all_clean_tabs();
    Ok(())
}

fn edit_undo(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.undo_active();
    Ok(())
}

fn edit_redo(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.redo_active();
    Ok(())
}

fn edit_copy(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.copy()
}

fn edit_cut(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.cut()
}

fn edit_paste(core: &mut EditorCore, args: CommandArgs) -> Result<(), EditorError> {
    match args.text() {
        Some(text) => {
            core.paste_text(text);
            Ok(())
        }
        None => core.paste_from_clipboard(),
    }
}

fn edit_select_all(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.select_all();
    Ok(())
}

fn edit_insert_text(core: &mut EditorCore, args: CommandArgs) -> Result<(), EditorError> {
    if let Some(text) = args.text() {
        core.type_text(text);
    }
    Ok(())
}

fn edit_insert_newline(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.type_text("\n");
    Ok(())
}

fn edit_insert_tab(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.insert_tab();
    Ok(())
}

fn edit_dedent(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.dedent();
    Ok(())
}

fn edit_delete_backward(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.delete_backward();
    Ok(())
}

fn edit_delete_forward(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.delete_forward();
    Ok(())
}

fn edit_delete_word_backward(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.delete_word_backward();
    Ok(())
}

fn edit_delete_word_forward(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.delete_word_forward();
    Ok(())
}

fn go_to_position(core: &mut EditorCore, args: CommandArgs) -> Result<(), EditorError> {
    if let CommandArgs::Position { line, column } = args {
        core.go_to(line, column);
    }
    Ok(())
}

fn edit_find(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.open_find();
    Ok(())
}

fn edit_find_next(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.find_next();
    Ok(())
}

fn edit_find_previous(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.find_previous();
    Ok(())
}

fn view_next_tab(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.cycle_tab(1);
    Ok(())
}

fn view_previous_tab(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.cycle_tab(-1);
    Ok(())
}

fn view_go_to_tab(core: &mut EditorCore, args: CommandArgs) -> Result<(), EditorError> {
    if let CommandArgs::Index(number) = args {
        core.go_to_tab(number);
    }
    Ok(())
}

fn view_toggle_status_bar(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::ToggleStatusBar);
    Ok(())
}

fn view_toggle_dev_panel(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::ToggleDevPanel);
    Ok(())
}

fn view_toggle_resource_center(
    core: &mut EditorCore,
    _args: CommandArgs,
) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::ToggleResourceCenter);
    Ok(())
}

fn view_toggle_file_tree(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::ToggleFileTree);
    Ok(())
}

fn file_open_folder(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::OpenFolderDialog);
    Ok(())
}

fn view_workspace_search(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::WorkspaceSearch);
    Ok(())
}

fn view_toggle_git_status(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::ToggleGitStatus);
    Ok(())
}

fn view_open_settings(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::ToggleSettings);
    Ok(())
}

fn view_toggle_dependencies(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::ToggleDependencyView);
    Ok(())
}

fn view_refresh_dependencies(
    core: &mut EditorCore,
    _args: CommandArgs,
) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::RefreshDependencyView);
    Ok(())
}

fn view_toggle_terminal(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::ToggleTerminal);
    Ok(())
}

/// Cancels the load the active tab is waiting for.
fn document_cancel_load(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    let Some(active) = core.active() else { return Ok(()) };
    core.cancel_open(active);
    Ok(())
}

fn diagnostics_duplicate_storm(
    core: &mut EditorCore,
    _args: CommandArgs,
) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::DiagnosticsDuplicateStorm);
    Ok(())
}

fn diagnostics_slow_load(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::DiagnosticsSlowLoad);
    Ok(())
}

fn diagnostics_failing_load(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::DiagnosticsFailingLoad);
    Ok(())
}

/// A load is only cancellable while it is in flight.
fn is_loading(core: &EditorCore) -> bool {
    core.active().map(|active| core.is_loading(active)).unwrap_or(false)
}

fn view_toggle_performance_overlay(
    core: &mut EditorCore,
    _args: CommandArgs,
) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::TogglePerformanceOverlay);
    Ok(())
}

fn app_quit(core: &mut EditorCore, _args: CommandArgs) -> Result<(), EditorError> {
    core.request_shell(ShellRequest::Quit);
    Ok(())
}

/// Something only the shell can do, requested by a command.
///
/// The core has no window, no dialogs and no event loop, so commands that need
/// those hand a request back rather than reaching outside their layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellRequest {
    OpenFileDialog,
    SaveAsDialog,
    TogglePerformanceOverlay,
    /// Show or hide the asynchronous-loading development panel.
    ToggleDevPanel,
    /// Show or hide the Resource Center (admission, accounting, pressure).
    ToggleResourceCenter,
    /// Show or hide the file tree (item 6).
    ToggleFileTree,
    /// Open a folder as the workspace root (item 6).
    OpenFolderDialog,
    /// Open the workspace-search query bar (item 7).
    WorkspaceSearch,
    /// Show or hide the git status panel (item 11).
    ToggleGitStatus,
    /// Show or hide the settings screen.
    ToggleSettings,
    /// Show or hide the dependency view, scanning the workspace to build it.
    ToggleDependencyView,
    /// Scan the workspace again, replacing whatever was saved for it.
    RefreshDependencyView,
    /// Show or hide the command runner (item 10).
    ToggleTerminal,
    /// Show or hide the status bar.
    ToggleStatusBar,
    /// Diagnostics: issue several requests for one path at once, so the join
    /// path is observable in a running editor.
    DiagnosticsDuplicateStorm,
    /// Diagnostics: reopen the last path with an injected delay.
    DiagnosticsSlowLoad,
    /// Diagnostics: reopen the last path with an injected failure.
    DiagnosticsFailingLoad,
    Quit,
}

/// Every command in the application.
pub const COMMANDS: &[CommandDescriptor] = &[
    CommandDescriptor {
        id: "file.new",
        display_name: "New File",
        enabled: always,
        execute: file_new,
    },
    CommandDescriptor {
        id: "file.open",
        display_name: "Open File",
        enabled: always,
        execute: file_open,
    },
    CommandDescriptor {
        id: "file.save",
        display_name: "Save",
        enabled: is_dirty_or_untitled,
        execute: file_save,
    },
    CommandDescriptor {
        id: "file.save_as",
        display_name: "Save As",
        enabled: has_document,
        execute: file_save_as,
    },
    CommandDescriptor {
        id: "file.close_tab",
        display_name: "Close Tab",
        enabled: has_document,
        execute: file_close_tab,
    },
    CommandDescriptor {
        id: "file.close_all_clean_tabs",
        display_name: "Close All Clean Tabs",
        enabled: has_document,
        execute: file_close_all_clean_tabs,
    },
    CommandDescriptor {
        id: "edit.undo",
        display_name: "Undo",
        enabled: can_undo,
        execute: edit_undo,
    },
    CommandDescriptor {
        id: "edit.redo",
        display_name: "Redo",
        enabled: can_redo,
        execute: edit_redo,
    },
    CommandDescriptor {
        id: "edit.copy",
        display_name: "Copy",
        enabled: has_selection,
        execute: edit_copy,
    },
    CommandDescriptor {
        id: "edit.cut",
        display_name: "Cut",
        enabled: has_selection,
        execute: edit_cut,
    },
    CommandDescriptor {
        id: "edit.paste",
        display_name: "Paste",
        enabled: has_document,
        execute: edit_paste,
    },
    CommandDescriptor {
        id: "edit.select_all",
        display_name: "Select All",
        enabled: has_document,
        execute: edit_select_all,
    },
    CommandDescriptor {
        id: "edit.insert_text",
        display_name: "Insert Text",
        enabled: has_document,
        execute: edit_insert_text,
    },
    CommandDescriptor {
        id: "edit.insert_newline",
        display_name: "Insert Line Break",
        enabled: has_document,
        execute: edit_insert_newline,
    },
    CommandDescriptor {
        id: "edit.insert_tab",
        display_name: "Insert Tab",
        enabled: has_document,
        execute: edit_insert_tab,
    },
    CommandDescriptor {
        id: "edit.dedent",
        display_name: "Dedent",
        enabled: has_document,
        execute: edit_dedent,
    },
    CommandDescriptor {
        id: "edit.delete_backward",
        display_name: "Delete Backward",
        enabled: has_document,
        execute: edit_delete_backward,
    },
    CommandDescriptor {
        id: "edit.delete_forward",
        display_name: "Delete Forward",
        enabled: has_document,
        execute: edit_delete_forward,
    },
    CommandDescriptor {
        id: "edit.delete_word_backward",
        display_name: "Delete Word Backward",
        enabled: has_document,
        execute: edit_delete_word_backward,
    },
    CommandDescriptor {
        id: "edit.delete_word_forward",
        display_name: "Delete Word Forward",
        enabled: has_document,
        execute: edit_delete_word_forward,
    },
    CommandDescriptor {
        id: "cursor.left",
        display_name: "Cursor Left",
        enabled: has_document,
        execute: cursor_left,
    },
    CommandDescriptor {
        id: "cursor.left.select",
        display_name: "Select Left",
        enabled: has_document,
        execute: select_left,
    },
    CommandDescriptor {
        id: "cursor.right",
        display_name: "Cursor Right",
        enabled: has_document,
        execute: cursor_right,
    },
    CommandDescriptor {
        id: "cursor.right.select",
        display_name: "Select Right",
        enabled: has_document,
        execute: select_right,
    },
    CommandDescriptor {
        id: "cursor.up",
        display_name: "Cursor Up",
        enabled: has_document,
        execute: cursor_up,
    },
    CommandDescriptor {
        id: "cursor.up.select",
        display_name: "Select Up",
        enabled: has_document,
        execute: select_up,
    },
    CommandDescriptor {
        id: "cursor.down",
        display_name: "Cursor Down",
        enabled: has_document,
        execute: cursor_down,
    },
    CommandDescriptor {
        id: "cursor.down.select",
        display_name: "Select Down",
        enabled: has_document,
        execute: select_down,
    },
    CommandDescriptor {
        id: "cursor.word_left",
        display_name: "Cursor Word Left",
        enabled: has_document,
        execute: cursor_word_left,
    },
    CommandDescriptor {
        id: "cursor.word_left.select",
        display_name: "Select Word Left",
        enabled: has_document,
        execute: select_word_left,
    },
    CommandDescriptor {
        id: "cursor.word_right",
        display_name: "Cursor Word Right",
        enabled: has_document,
        execute: cursor_word_right,
    },
    CommandDescriptor {
        id: "cursor.word_right.select",
        display_name: "Select Word Right",
        enabled: has_document,
        execute: select_word_right,
    },
    CommandDescriptor {
        id: "cursor.line_start",
        display_name: "Cursor Line Start",
        enabled: has_document,
        execute: cursor_line_start,
    },
    CommandDescriptor {
        id: "cursor.line_start.select",
        display_name: "Select to Line Start",
        enabled: has_document,
        execute: select_line_start,
    },
    CommandDescriptor {
        id: "cursor.line_end",
        display_name: "Cursor Line End",
        enabled: has_document,
        execute: cursor_line_end,
    },
    CommandDescriptor {
        id: "cursor.line_end.select",
        display_name: "Select to Line End",
        enabled: has_document,
        execute: select_line_end,
    },
    CommandDescriptor {
        id: "cursor.document_start",
        display_name: "Cursor Document Start",
        enabled: has_document,
        execute: cursor_document_start,
    },
    CommandDescriptor {
        id: "cursor.document_start.select",
        display_name: "Select to Document Start",
        enabled: has_document,
        execute: select_document_start,
    },
    CommandDescriptor {
        id: "cursor.document_end",
        display_name: "Cursor Document End",
        enabled: has_document,
        execute: cursor_document_end,
    },
    CommandDescriptor {
        id: "cursor.document_end.select",
        display_name: "Select to Document End",
        enabled: has_document,
        execute: select_document_end,
    },
    CommandDescriptor {
        id: "cursor.page_up",
        display_name: "Cursor Page Up",
        enabled: has_document,
        execute: cursor_page_up,
    },
    CommandDescriptor {
        id: "cursor.page_up.select",
        display_name: "Select Page Up",
        enabled: has_document,
        execute: select_page_up,
    },
    CommandDescriptor {
        id: "cursor.page_down",
        display_name: "Cursor Page Down",
        enabled: has_document,
        execute: cursor_page_down,
    },
    CommandDescriptor {
        id: "cursor.page_down.select",
        display_name: "Select Page Down",
        enabled: has_document,
        execute: select_page_down,
    },
    CommandDescriptor {
        id: "cursor.go_to",
        display_name: "Go to Line",
        enabled: has_document,
        execute: go_to_position,
    },
    CommandDescriptor {
        id: "edit.find",
        display_name: "Find...",
        enabled: has_document,
        execute: edit_find,
    },
    CommandDescriptor {
        id: "edit.find_next",
        display_name: "Find Next",
        enabled: has_find_matches,
        execute: edit_find_next,
    },
    CommandDescriptor {
        id: "edit.find_previous",
        display_name: "Find Previous",
        enabled: has_find_matches,
        execute: edit_find_previous,
    },
    CommandDescriptor {
        id: "view.next_tab",
        display_name: "Next Tab",
        enabled: has_document,
        execute: view_next_tab,
    },
    CommandDescriptor {
        id: "view.previous_tab",
        display_name: "Previous Tab",
        enabled: has_document,
        execute: view_previous_tab,
    },
    CommandDescriptor {
        id: "view.go_to_tab",
        display_name: "Go to Tab",
        enabled: has_document,
        execute: view_go_to_tab,
    },
    CommandDescriptor {
        id: "view.toggle_performance_overlay",
        display_name: "Toggle Performance Overlay",
        enabled: always,
        execute: view_toggle_performance_overlay,
    },
    CommandDescriptor {
        id: "view.toggle_status_bar",
        display_name: "Toggle Status Bar",
        enabled: always,
        execute: view_toggle_status_bar,
    },
    CommandDescriptor {
        id: "view.toggle_dev_panel",
        display_name: "Toggle Loading Panel",
        enabled: always,
        execute: view_toggle_dev_panel,
    },
    CommandDescriptor {
        id: "view.toggle_resource_center",
        display_name: "Toggle Resource Center",
        enabled: always,
        execute: view_toggle_resource_center,
    },
    CommandDescriptor {
        id: "view.toggle_file_tree",
        display_name: "Toggle File Tree",
        enabled: always,
        execute: view_toggle_file_tree,
    },
    CommandDescriptor {
        id: "file.open_folder",
        display_name: "Open Folder",
        enabled: always,
        execute: file_open_folder,
    },
    CommandDescriptor {
        id: "view.workspace_search",
        display_name: "Search in Files...",
        enabled: always,
        execute: view_workspace_search,
    },
    CommandDescriptor {
        id: "view.toggle_git_status",
        display_name: "Toggle Git Status",
        enabled: always,
        execute: view_toggle_git_status,
    },
    CommandDescriptor {
        id: "view.open_settings",
        display_name: "Settings",
        enabled: always,
        execute: view_open_settings,
    },
    CommandDescriptor {
        id: "view.toggle_dependencies",
        display_name: "Toggle Dependency View",
        enabled: always,
        execute: view_toggle_dependencies,
    },
    CommandDescriptor {
        id: "view.refresh_dependencies",
        display_name: "Rescan Dependencies",
        enabled: always,
        execute: view_refresh_dependencies,
    },
    CommandDescriptor {
        id: "view.toggle_terminal",
        display_name: "Toggle Terminal",
        enabled: always,
        execute: view_toggle_terminal,
    },
    CommandDescriptor {
        id: "document.cancel_load",
        display_name: "Cancel Load",
        enabled: is_loading,
        execute: document_cancel_load,
    },
    CommandDescriptor {
        id: "diagnostics.duplicate_storm",
        display_name: "Diagnostics: Duplicate Load Storm",
        enabled: always,
        execute: diagnostics_duplicate_storm,
    },
    CommandDescriptor {
        id: "diagnostics.slow_load",
        display_name: "Diagnostics: Slow Load",
        enabled: always,
        execute: diagnostics_slow_load,
    },
    CommandDescriptor {
        id: "diagnostics.failing_load",
        display_name: "Diagnostics: Failing Load",
        enabled: always,
        execute: diagnostics_failing_load,
    },
    CommandDescriptor { id: "app.quit", display_name: "Quit", enabled: always, execute: app_quit },
];

fn index() -> &'static HashMap<&'static str, &'static CommandDescriptor> {
    static INDEX: OnceLock<HashMap<&'static str, &'static CommandDescriptor>> = OnceLock::new();
    INDEX.get_or_init(|| COMMANDS.iter().map(|command| (command.id, command)).collect())
}

/// Looks up a command by id.
pub fn find(id: &str) -> Option<&'static CommandDescriptor> {
    index().get(id).copied()
}

/// All commands, for menus, palettes and keybinding editors.
pub fn all() -> &'static [CommandDescriptor] {
    COMMANDS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn command_ids_are_unique() {
        let mut seen = HashSet::new();
        for command in COMMANDS {
            assert!(seen.insert(command.id), "duplicate command id {}", command.id);
        }
    }

    #[test]
    fn command_ids_are_namespaced_and_named() {
        for command in COMMANDS {
            assert!(command.id.contains('.'), "{} should be namespaced", command.id);
            assert!(!command.display_name.is_empty(), "{} needs a display name", command.id);
            assert_eq!(command.id, command.id.to_lowercase(), "{} should be lowercase", command.id);
        }
    }

    #[test]
    fn every_command_is_findable() {
        for command in COMMANDS {
            let found = find(command.id).expect("registered command is findable");
            assert_eq!(found.id, command.id);
        }
        assert!(find("does.not.exist").is_none());
    }

    #[test]
    fn every_movement_has_a_selecting_counterpart() {
        for command in COMMANDS.iter().filter(|c| c.id.starts_with("cursor.")) {
            if command.id.ends_with(".select") || command.id == "cursor.go_to" {
                continue;
            }
            let selecting = format!("{}.select", command.id);
            assert!(find(&selecting).is_some(), "{} has no selecting counterpart", command.id);
        }
    }

    #[test]
    fn args_accessors_are_typed() {
        assert_eq!(CommandArgs::Text("x".into()).text(), Some("x"));
        assert_eq!(CommandArgs::None.text(), None);
        assert_eq!(CommandArgs::Path(PathBuf::from("a")).path(), Some(&PathBuf::from("a")));
        assert_eq!(CommandArgs::default(), CommandArgs::None);
    }
}
