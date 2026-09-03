//! Tab switching, tab closing and editing focus.
//!
//! These are the core-side halves of four interaction defects. The geometry
//! halves -- which rectangle a click lands in, which layer a surface is drawn
//! in, where the wheel goes -- are unit-tested in the shell next to the code
//! that computes them (`app/src/tabs.rs`, `app/src/compose.rs`,
//! `app/src/menu.rs`, `app/src/app.rs`), because that is where they can be
//! asserted on directly rather than described.
//!
//! What is tested here is the part that must be true whatever the window does:
//! that a switch changes which document is authoritative, that an edit lands in
//! the document the user is looking at, that dirty state stays attached to its
//! own document, and that closing acts on a `DocumentId` rather than on a
//! position in a list.

use ls_core::{CommandArgs, DocumentId, EditorCore, EditorError, Viewport};
use ls_tests::{headless_editor, TempDir};
use std::time::{Duration, Instant};

fn pump_until(editor: &mut EditorCore, mut settled: impl FnMut(&EditorCore) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !settled(editor) {
        editor.pump_completions();
        assert!(Instant::now() < deadline, "timed out waiting for background work");
        std::thread::yield_now();
    }
}

/// The text of the document the renderer would be given a snapshot of.
fn visible_text(editor: &mut EditorCore) -> String {
    let id = editor.active().expect("a document is active");
    let snapshot = editor.render_snapshot(id, Viewport::default()).expect("the document exists");
    snapshot.lines.iter().map(|line| line.text.to_string()).collect::<Vec<_>>().join("\n")
}

/// Two documents with distinct content, B active, as the shell would have them
/// after opening A and then B.
fn two_documents() -> (EditorCore, DocumentId, DocumentId) {
    let mut editor = headless_editor();
    let a = editor.new_document();
    editor.type_text("document A");
    let b = editor.new_document();
    editor.type_text("document B");
    (editor, a, b)
}

// --- switching tabs -----------------------------------------------------------

#[test]
fn clicking_a_tab_activates_that_document_and_the_snapshot_follows() {
    // The shell resolves a click to a `DocumentId` and calls `set_active`. What
    // has to be true afterwards is that the snapshot describes the document
    // that was clicked, not the one that used to be active.
    let (mut editor, a, b) = two_documents();
    assert_eq!(editor.active(), Some(b));

    editor.set_active(a).expect("A is open");
    assert_eq!(editor.active(), Some(a));
    assert_eq!(visible_text(&mut editor), "document A");

    editor.set_active(b).expect("B is open");
    assert_eq!(editor.active(), Some(b));
    assert_eq!(visible_text(&mut editor), "document B");
}

#[test]
fn exactly_one_tab_is_active_after_a_switch() {
    let (mut editor, a, _b) = two_documents();
    editor.set_active(a).unwrap();

    let presentations = editor.tab_presentations();
    let active: Vec<DocumentId> =
        presentations.iter().filter(|tab| tab.active).map(|tab| tab.id).collect();
    assert_eq!(active, vec![a], "the tab bar must agree with the core about which is active");
}

#[test]
fn ctrl_tab_cycles_forwards_and_ctrl_shift_tab_backwards() {
    // `Ctrl+Tab` is `view.next_tab` in the registry; the shell binds the key
    // and runs the command, so cycling is tested through the command.
    let mut editor = headless_editor();
    let a = editor.new_document();
    let b = editor.new_document();
    let c = editor.new_document();
    assert_eq!(editor.active(), Some(c));

    editor.execute("view.next_tab", CommandArgs::None).unwrap();
    assert_eq!(editor.active(), Some(a), "next wraps from the last tab to the first");
    editor.execute("view.next_tab", CommandArgs::None).unwrap();
    assert_eq!(editor.active(), Some(b));

    editor.execute("view.previous_tab", CommandArgs::None).unwrap();
    assert_eq!(editor.active(), Some(a));
    editor.execute("view.previous_tab", CommandArgs::None).unwrap();
    assert_eq!(editor.active(), Some(c), "previous wraps from the first tab to the last");
}

#[test]
fn typing_after_a_switch_lands_in_the_newly_active_document() {
    // The stale-reference case: open A, type, open B, type, switch back to A,
    // type. Every keystroke has to reach the document the user is looking at.
    let mut editor = headless_editor();
    let a = editor.new_document();
    editor.type_text("A1");
    let b = editor.new_document();
    editor.type_text("B1");

    editor.set_active(a).unwrap();
    editor.type_text("A2");
    editor.set_active(b).unwrap();
    editor.type_text("B2");

    assert_eq!(editor.document(a).unwrap().text().to_string(), "A1A2");
    assert_eq!(editor.document(b).unwrap().text().to_string(), "B1B2");
}

#[test]
fn dirty_state_stays_attached_to_its_own_document() {
    let directory = TempDir::new("interaction-dirty");
    let path = directory.write("saved.txt", "on disk\n");

    let mut editor = headless_editor();
    let saved = editor.open_document(&path).expect("opened");
    let scratch = editor.new_document();
    editor.type_text("only this one is edited");

    let dirty_of = |editor: &EditorCore, id: DocumentId| {
        editor.tab_presentations().into_iter().find(|tab| tab.id == id).unwrap().dirty
    };
    assert!(dirty_of(&editor, scratch), "the edited document is dirty");
    assert!(!dirty_of(&editor, saved), "switching tabs must not spread dirtiness");

    editor.set_active(saved).unwrap();
    assert!(dirty_of(&editor, scratch), "and it stays dirty while another tab is active");
    assert!(!dirty_of(&editor, saved));
}

#[test]
fn switching_to_and_from_a_loading_document_is_safe() {
    // A loading tab exists before its document does. Activating it must not
    // invent a document, and switching away and back must not lose the load.
    let directory = TempDir::new("interaction-loading");
    let path = directory.write("slow.txt", "loaded content\n");

    let mut editor = headless_editor();
    let scratch = editor.new_document();
    editor.type_text("scratch");
    let request = editor.request_open_document(&path).expect("admitted");
    let loading = request.document;

    assert!(editor.is_loading(loading));
    editor.set_active(loading).expect("a loading tab can be selected");
    assert_eq!(editor.active(), Some(loading));
    assert!(editor.document(loading).is_none(), "there is no document behind it yet");
    assert!(
        editor.render_snapshot(loading, Viewport::default()).is_none(),
        "a loading tab has nothing to draw, and asking for it must not panic"
    );

    editor.set_active(scratch).unwrap();
    editor.type_text(" more");

    pump_until(&mut editor, |editor| !editor.is_loading(loading));
    editor.set_active(loading).unwrap();
    assert!(visible_text(&mut editor).starts_with("loaded content"));
    assert_eq!(editor.document(scratch).unwrap().text().to_string(), "scratch more");
}

// --- closing tabs -------------------------------------------------------------

#[test]
fn closing_a_clean_tab_closes_it_immediately() {
    let (mut editor, a, b) = two_documents();
    // Make A clean by undoing back to empty.
    editor.set_active(a).unwrap();
    editor.undo_active();
    assert!(!editor.document(a).unwrap().is_dirty());

    editor.close_document(a).expect("a clean document closes without a question");
    assert_eq!(editor.tabs(), &[b]);
}

#[test]
fn closing_a_dirty_tab_is_refused_until_the_user_answers() {
    let (mut editor, a, _b) = two_documents();
    assert!(matches!(editor.close_document(a), Err(EditorError::UnsavedChanges(_))));
    assert!(editor.tabs().contains(&a), "cancelling leaves it open");

    // "Don't Save".
    editor.close_document_discarding_changes(a).expect("discarding closes it");
    assert!(!editor.tabs().contains(&a));
}

#[test]
fn saving_from_the_close_prompt_then_closing_keeps_the_content() {
    let directory = TempDir::new("interaction-close-save");
    let path = directory.write("close.txt", "before\n");

    let mut editor = headless_editor();
    let id = editor.open_document(&path).expect("opened");
    editor.type_text("after ");

    editor.request_save(id).expect("admitted");
    pump_until(&mut editor, |editor| !editor.is_saving(id) && !editor.has_queued_save(id));
    editor.close_document(id).expect("the save cleaned it");

    assert!(editor.tabs().is_empty());
    assert!(directory.read_string("close.txt").starts_with("after "));
}

#[test]
fn closing_a_tab_acts_on_its_document_not_on_its_position() {
    // Three tabs with the same visible name. Only the id distinguishes them,
    // which is exactly the case a position-based or name-based close gets
    // wrong.
    let mut editor = headless_editor();
    let first = editor.new_document();
    let second = editor.new_document();
    let third = editor.new_document();
    let names: Vec<String> =
        editor.tab_presentations().iter().map(|tab| tab.title.clone()).collect();
    assert_eq!(names.len(), 3);

    editor.close_document(second).expect("clean");
    assert_eq!(editor.tabs(), &[first, third], "the middle document is the one that went");
}

#[test]
fn closing_the_active_tab_activates_a_neighbour() {
    let mut editor = headless_editor();
    let first = editor.new_document();
    let second = editor.new_document();
    let third = editor.new_document();

    editor.set_active(second).unwrap();
    editor.close_document(second).expect("clean");
    assert!(editor.active().is_some(), "something has to receive input afterwards");
    assert!(editor.tabs().contains(&first) && editor.tabs().contains(&third));
    assert!(!editor.tabs().contains(&second));
}

#[test]
fn closing_an_inactive_tab_leaves_the_active_one_alone() {
    let (mut editor, a, b) = two_documents();
    assert_eq!(editor.active(), Some(b));

    editor.set_active(a).unwrap();
    editor.undo_active();
    editor.set_active(b).unwrap();

    editor.close_document(a).expect("clean and inactive");
    assert_eq!(editor.active(), Some(b), "closing another tab must not steal the selection");
    assert_eq!(visible_text(&mut editor), "document B");
}

#[test]
fn closing_the_only_tab_leaves_no_document_active() {
    let mut editor = headless_editor();
    let only = editor.new_document();
    editor.close_document(only).expect("clean");
    assert!(editor.tabs().is_empty());
    assert_eq!(editor.active(), None);
}

#[test]
fn closing_a_loading_tab_cancels_the_load_rather_than_waiting_for_it() {
    let directory = TempDir::new("interaction-close-loading");
    let path = directory.write("loading.txt", "content\n");

    let mut editor = headless_editor();
    let request = editor.request_open_document(&path).expect("admitted");
    let loading = request.document;

    editor.close_document(loading).expect("a loading tab closes");
    pump_until(&mut editor, |editor| editor.loading_count() == 0);
    assert!(!editor.tabs().contains(&loading), "the tab is gone once the cancellation lands");
    assert!(editor.document(loading).is_none());
}

// --- the menu is still only the registry ---------------------------------------

#[test]
fn a_disabled_menu_action_does_nothing_when_it_is_run_anyway() {
    // The shell dims an item the registry refuses and refuses to run it. Even
    // if that check were bypassed, executing the command must not mutate
    // anything -- enablement is the registry's rule, not the menu's.
    let mut editor = headless_editor();
    let id = editor.new_document();
    assert!(!editor.is_command_enabled("edit.undo"));

    let before = editor.document(id).unwrap().revision();
    let _ = editor.execute("edit.undo", CommandArgs::None);
    assert_eq!(
        editor.document(id).unwrap().revision(),
        before,
        "a refused action must leave the document untouched"
    );
}

#[test]
fn enablement_tracks_the_active_document_not_the_last_one_looked_at() {
    let mut editor = headless_editor();
    let edited = editor.new_document();
    editor.type_text("something");
    assert!(editor.is_command_enabled("edit.undo"));

    let fresh = editor.new_document();
    assert!(!editor.is_command_enabled("edit.undo"), "the new document has nothing to undo");

    editor.set_active(edited).unwrap();
    assert!(editor.is_command_enabled("edit.undo"));
    editor.set_active(fresh).unwrap();
    assert!(!editor.is_command_enabled("edit.undo"));
}
