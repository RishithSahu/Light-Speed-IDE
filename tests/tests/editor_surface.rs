//! The minimum editor surface (specification section 21).
//!
//! Every requirement in that section is a statement about *core* behaviour that
//! the shell merely routes to: the menu runs commands, the tab bar reads
//! `tab_presentations`, the status bar reads document state, and nothing in the
//! window keeps a second copy of any of it.
//!
//! Testing it here rather than through a window is deliberate. The shell is a
//! `winit` binary and cannot be driven headlessly, but the shell also contains
//! no editor logic to test -- if it did, that would itself be the defect. So
//! this file proves the behaviour through the same API the shell calls, and
//! `architecture.rs` proves the shell calls nothing else.

use ls_core::{
    CommandArgs, ContentState, DocumentId, EditorCore, Movement, PersistenceState, RenderSnapshot,
    Viewport,
};
use ls_tests::{headless_editor, TempDir};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The snapshot the renderer would be handed for the active document.
fn frame(editor: &mut EditorCore) -> Arc<RenderSnapshot> {
    let id = editor.active().expect("a document is active");
    editor.render_snapshot(id, Viewport::default()).expect("the document exists")
}

/// The active document's text, as a string.
fn text(editor: &EditorCore) -> String {
    editor.active_document().expect("a document is active").text().to_string()
}

fn pump_until(editor: &mut EditorCore, mut settled: impl FnMut(&EditorCore) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !settled(editor) {
        editor.pump_completions();
        assert!(Instant::now() < deadline, "timed out waiting for background work");
        std::thread::yield_now();
    }
}

/// What the shell does at startup when it was given no file arguments.
fn launched_empty() -> EditorCore {
    let mut editor = headless_editor();
    if editor.tabs().is_empty() {
        editor.new_document();
    }
    editor
}

// --- starting up -------------------------------------------------------------

#[test]
fn launching_with_no_arguments_gives_an_editable_untitled_document() {
    // Section 6: the window is never empty and never shows a dead placeholder;
    // the user can type immediately.
    let mut editor = launched_empty();
    assert_eq!(editor.tabs().len(), 1);

    let id = editor.active().expect("a document is active");
    let document = editor.document(id).expect("the document exists");
    assert_eq!(document.display_name(), "Untitled-1");
    assert!(document.path().is_none(), "an untitled document has no path yet");
    assert!(!document.is_dirty(), "a fresh document starts clean");

    editor.type_text("hello");
    assert_eq!(editor.document(id).unwrap().text().to_string(), "hello");
    assert!(editor.document(id).unwrap().is_dirty());
}

#[test]
fn each_new_document_gets_the_next_untitled_number() {
    // Section 15: names are sequential, not all "Untitled".
    let mut editor = launched_empty();
    let second = editor.new_document();
    let third = editor.new_document();
    assert_eq!(editor.document(second).unwrap().display_name(), "Untitled-2");
    assert_eq!(editor.document(third).unwrap().display_name(), "Untitled-3");
}

// --- typing, the caret and selection ------------------------------------------

#[test]
fn typing_moves_the_caret_and_the_snapshot_follows() {
    // Section 10: the caret position the renderer draws comes from the
    // snapshot, which comes from the buffer -- not from anything the shell
    // counts for itself.
    let mut editor = launched_empty();
    editor.type_text("abc");

    let snapshot = frame(&mut editor);
    let cursor = snapshot.cursors.iter().find(|c| c.primary).expect("a primary caret");
    assert_eq!(cursor.line.get(), 0);
    assert_eq!(cursor.column_chars, 3, "the caret sits after the last character typed");

    editor.type_text("\ndef");
    let snapshot = frame(&mut editor);
    let cursor = snapshot.cursors.iter().find(|c| c.primary).expect("a primary caret");
    assert_eq!(cursor.line.get(), 1);
    assert_eq!(cursor.column_chars, 3);
    assert_eq!(snapshot.lines.len(), 2);
}

#[test]
fn shift_movement_extends_a_selection_that_reaches_the_snapshot() {
    // Section 11: selection is real editor state and is drawn from the
    // snapshot, so a selected range is visible to the renderer.
    let mut editor = launched_empty();
    editor.type_text("hello world");
    editor.move_cursor(Movement::LineStart, false).unwrap();
    editor.move_cursor(Movement::WordRight, true).unwrap();

    let snapshot = frame(&mut editor);
    assert!(
        !snapshot.selections.is_empty(),
        "an extended movement leaves a selection for the renderer to draw"
    );

    // Typing replaces it, which is the behaviour a selection has to have.
    editor.type_text("X");
    assert!(text(&editor).starts_with('X'));
}

#[test]
fn double_click_word_selection_uses_the_movement_commands() {
    // Section 12: the shell selects a word by running the same two commands the
    // keyboard uses, so there is one definition of a word boundary.
    let mut editor = launched_empty();
    editor.type_text("alpha beta gamma");
    editor.go_to(0, 8); // inside "beta"

    editor.execute("cursor.word_left", CommandArgs::None).unwrap();
    editor.execute("cursor.word_right.select", CommandArgs::None).unwrap();

    editor.copy().expect("a word is selected");
    let snapshot = frame(&mut editor);
    assert!(!snapshot.selections.is_empty(), "the word is selected");
}

// --- the menu is only the registry -------------------------------------------

#[test]
fn every_edit_menu_action_runs_through_the_command_registry() {
    // Section 12: the menu bar routes through the registry. Running the command
    // ids by name -- exactly what the menu does -- must produce the edits.
    let mut editor = launched_empty();
    editor.type_text("hello");

    editor.execute("edit.select_all", CommandArgs::None).unwrap();
    editor.execute("edit.copy", CommandArgs::None).unwrap();
    editor.execute("edit.delete_forward", CommandArgs::None).unwrap();
    assert_eq!(text(&editor), "");

    editor.execute("edit.paste", CommandArgs::None).unwrap();
    assert_eq!(text(&editor), "hello", "the clipboard round-trips");

    editor.execute("edit.undo", CommandArgs::None).unwrap();
    assert_ne!(text(&editor), "hello");
    editor.execute("edit.redo", CommandArgs::None).unwrap();
    assert_eq!(text(&editor), "hello");
}

#[test]
fn an_action_that_cannot_apply_reports_itself_disabled() {
    // The menu draws an item dim when the registry refuses it, so enablement
    // has to be answerable before anything is run.
    let mut editor = launched_empty();
    assert!(!editor.is_command_enabled("edit.undo"), "nothing to undo yet");
    assert!(!editor.is_command_enabled("edit.copy"), "nothing selected yet");

    editor.type_text("text");
    assert!(editor.is_command_enabled("edit.undo"));

    editor.execute("edit.select_all", CommandArgs::None).unwrap();
    assert!(editor.is_command_enabled("edit.copy"));
}

// --- opening and saving from the UI ------------------------------------------

#[test]
fn opening_from_the_menu_is_asynchronous_and_shows_a_loading_tab() {
    // Sections 7 and 8: the shell never blocks on a read. Between the request
    // and the pump there is a tab, and it says it is loading.
    let directory = TempDir::new("surface-open");
    let path = directory.write("file.txt", "on disk\n");

    let mut editor = launched_empty();
    let request = editor.request_open_document(&path).expect("the open was admitted");
    let id = request.document;

    assert!(editor.is_loading(id), "the document is loading, not loaded");
    let tab = editor
        .tab_presentations()
        .into_iter()
        .find(|tab| tab.id == id)
        .expect("a tab appears immediately");
    assert!(tab.loading, "the tab bar shows the load in progress");
    assert!(!tab.dirty, "a loading document is not dirty");

    pump_until(&mut editor, |editor| !editor.is_loading(id));
    assert_eq!(editor.document(id).unwrap().text().to_string(), "on disk\n");
    let tab = editor.tab_presentations().into_iter().find(|tab| tab.id == id).unwrap();
    assert!(!tab.loading);
}

#[test]
fn saving_from_the_menu_is_asynchronous_and_only_completes_on_the_pump() {
    // Sections 8 and 9: `Ctrl+S` requests a save and returns. The document does
    // not become clean until the event loop pumps the completion.
    let directory = TempDir::new("surface-save");
    let path = directory.write("saved.txt", "before\n");

    let mut editor = headless_editor();
    let id = editor.open_document(&path).expect("opened");
    editor.type_text("edited ");
    assert!(editor.document(id).unwrap().is_dirty());

    editor.request_save(id).expect("the save was admitted");
    assert!(editor.is_saving(id));
    assert!(editor.document(id).unwrap().is_dirty(), "still dirty until the pump");

    pump_until(&mut editor, |editor| !editor.is_saving(id));
    assert!(!editor.document(id).unwrap().is_dirty(), "clean once the completion is applied");
    assert_eq!(editor.document(id).unwrap().persistence_state(), PersistenceState::SaveSucceeded);
    assert!(directory.read_string("saved.txt").starts_with("edited "));
}

#[test]
fn save_as_gives_an_untitled_document_a_path_and_a_title() {
    // Section 8: Save As is how an untitled document acquires a name, and the
    // tab and window title both follow from it.
    let directory = TempDir::new("surface-save-as");
    let path = directory.path().join("named.txt");

    let mut editor = launched_empty();
    let id = editor.active().unwrap();
    editor.type_text("content\n");

    editor.request_save_as(id, path.clone()).expect("the save was admitted");
    pump_until(&mut editor, |editor| !editor.is_saving(id));

    let document = editor.document(id).unwrap();
    assert_eq!(document.display_name(), "named.txt");
    assert!(document.path().is_some(), "the document now has a path");
    assert!(!document.is_dirty());
    // A new document takes the platform's line ending, so the bytes on disk are
    // whatever that document says they should be.
    let expected = format!("content{}", document.line_ending().as_str());
    assert_eq!(directory.read_string("named.txt"), expected);
}

// --- the tab bar and the status bar read core state ---------------------------

#[test]
fn the_dirty_indicator_comes_from_document_state_not_from_the_shell() {
    // Section 9: the asterisk in the tab bar is `Document::is_dirty`, so it
    // cannot disagree with whether the file actually needs saving.
    let mut editor = launched_empty();
    let id = editor.active().unwrap();
    let dirty_flag = |editor: &EditorCore| {
        editor.tab_presentations().into_iter().find(|tab| tab.id == id).unwrap().dirty
    };

    assert!(!dirty_flag(&editor));
    editor.type_text("x");
    assert!(dirty_flag(&editor));
    assert_eq!(editor.document(id).unwrap().content_state(), ContentState::Dirty);

    editor.undo_active();
    assert!(!dirty_flag(&editor), "undoing back to the saved token cleans the tab");
}

#[test]
fn the_status_bar_reads_position_encoding_and_line_ending_from_the_document() {
    // Section 13: every field the status bar shows is authoritative core state.
    let directory = TempDir::new("surface-status");
    let path = directory.write("crlf.txt", "one\r\ntwo\r\n");

    let mut editor = headless_editor();
    let id = editor.open_document(&path).expect("opened");
    editor.go_to(1, 2);

    let document = editor.document(id).unwrap();
    assert_eq!(document.line_ending().label(), "CRLF");
    assert_eq!(document.encoding().label(), "UTF-8");

    let snapshot = frame(&mut editor);
    let cursor = snapshot.cursors.iter().find(|c| c.primary).unwrap();
    assert_eq!((cursor.line.get(), cursor.column_chars), (1, 2));
}

#[test]
fn switching_tabs_changes_which_document_the_snapshot_describes() {
    // Section 9: clicking a tab is `set_active`, and the viewport that the
    // renderer draws follows the active document.
    let mut editor = launched_empty();
    let first = editor.active().unwrap();
    editor.type_text("first document");

    let second = editor.new_document();
    editor.type_text("second document");
    assert_eq!(editor.active(), Some(second));

    editor.set_active(first).unwrap();
    let snapshot = frame(&mut editor);
    assert_eq!(&*snapshot.lines[0].text, "first document");

    let actives: Vec<bool> = editor.tab_presentations().iter().map(|tab| tab.active).collect();
    assert_eq!(actives.iter().filter(|active| **active).count(), 1);
}

// --- closing -------------------------------------------------------------------

#[test]
fn a_clean_document_closes_without_a_question() {
    // Section 16: only unsaved work is worth interrupting the user for.
    let mut editor = launched_empty();
    let id = editor.active().unwrap();
    assert!(editor.close_document(id).is_ok());
    assert!(editor.tabs().is_empty());
}

#[test]
fn a_dirty_document_refuses_to_close_until_the_answer_is_given() {
    // Section 16: the shell shows Save / Don't Save / Cancel because the core
    // refuses the close. Cancel is simply not calling again.
    let mut editor = launched_empty();
    let id = editor.active().unwrap();
    editor.type_text("unsaved");

    let refused = editor.close_document(id);
    assert!(
        matches!(refused, Err(ls_core::EditorError::UnsavedChanges(_))),
        "closing dirty work has to be refused, not silently accepted"
    );
    assert_eq!(editor.tabs().len(), 1, "cancelling leaves the document open");

    // "Don't Save" is the explicit discarding close.
    editor.close_document_discarding_changes(id).expect("discarding closes it");
    assert!(editor.tabs().is_empty());
}

#[test]
fn choosing_save_on_close_waits_for_the_save_before_the_document_goes_away() {
    // Section 16: answering Save must not discard anything. The shell holds the
    // close until the save completes, then closes cleanly.
    let directory = TempDir::new("surface-close-save");
    let path = directory.write("close.txt", "before\n");

    let mut editor = headless_editor();
    let id = editor.open_document(&path).expect("opened");
    editor.type_text("after ");

    editor.request_save(id).expect("admitted");
    assert!(
        editor.close_document(id).is_err(),
        "the document is still dirty while the save is in flight"
    );

    pump_until(&mut editor, |editor| !editor.is_saving(id) && !editor.has_queued_save(id));
    editor.close_document(id).expect("the save cleaned it, so the close succeeds");
    assert!(editor.tabs().is_empty());
    assert!(directory.read_string("close.txt").starts_with("after "));
}

// --- errors are reported, not thrown away ---------------------------------------

#[test]
fn a_failed_open_leaves_a_message_and_no_tab() {
    // Section 17: errors are surfaced non-modally and never silently dropped.
    let directory = TempDir::new("surface-open-error");
    let missing = directory.path().join("does-not-exist.txt");

    let mut editor = launched_empty();
    let before = editor.tabs().len();
    match editor.request_open_document(&missing) {
        Ok(request) => {
            let id = request.document;
            pump_until(&mut editor, |editor| !editor.is_loading(id));
            assert_eq!(editor.tabs().len(), before, "a failed load leaves no tab behind");
        }
        Err(error) => editor.report_open_failure(error),
    }
    assert!(editor.take_last_error().is_some(), "the user is told what went wrong");
}

#[test]
fn a_failed_save_says_so_and_keeps_the_document_dirty() {
    // Section 17: a save that did not happen must never look like one that did.
    let directory = TempDir::new("surface-save-error");
    let path = directory.write("readonly.txt", "before\n");

    let mut editor = headless_editor();
    let id = editor.open_document(&path).expect("opened");
    editor.type_text("edit ");

    // Saving into a directory that no longer exists cannot succeed.
    let gone = directory.path().join("removed").join("file.txt");
    editor.request_save_as(id, gone).expect("the request itself is admitted");
    pump_until(&mut editor, |editor| !editor.is_saving(id));

    let document = editor.document(id).unwrap();
    assert_eq!(document.persistence_state(), PersistenceState::SaveFailed);
    assert!(document.is_dirty(), "a failed save leaves the work unsaved");
}

// --- the view menu ----------------------------------------------------------------

#[test]
fn view_toggles_are_requests_to_the_shell_not_editor_state() {
    // Section 18: chrome visibility belongs to the window, so the core records
    // the request and the shell acts on it. The editor keeps no window state.
    let mut editor = launched_empty();
    editor.execute("view.toggle_status_bar", CommandArgs::None).unwrap();
    editor.execute("view.toggle_performance_overlay", CommandArgs::None).unwrap();

    let requests = editor.take_shell_requests();
    assert!(requests.contains(&ls_core::ShellRequest::ToggleStatusBar));
    assert!(requests.contains(&ls_core::ShellRequest::TogglePerformanceOverlay));
    assert!(editor.take_shell_requests().is_empty(), "requests are consumed once");
}

// --- nothing the UI does happens off the event loop ---------------------------------

#[test]
fn no_document_state_changes_without_an_explicit_pump() {
    // Sections 8 and 20: background work never mutates the editor behind the
    // shell's back. Without a pump, nothing observable moves.
    let directory = TempDir::new("surface-pump");
    let path = directory.write("pump.txt", "content\n");

    let mut editor = launched_empty();
    let request = editor.request_open_document(&path).expect("admitted");
    let id: DocumentId = request.document;

    // Give the worker every chance to finish while the loop is not pumping.
    std::thread::sleep(Duration::from_millis(50));
    assert!(editor.is_loading(id), "the result waits for the event loop");
    assert!(editor.document(id).is_none(), "there is no document until the pump");

    pump_until(&mut editor, |editor| !editor.is_loading(id));
    assert!(editor.document(id).is_some());
}
