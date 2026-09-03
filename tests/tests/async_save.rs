//! Revision-aware asynchronous saving (amendment sections 8, 8.1, 9, 10).
//!
//! The centrepiece is [`the_40_41_42_case`]: it is the reason the editor tracks
//! two different versions of a document, and the reason a completed save cannot
//! simply declare the document clean.

use ls_core::{ContentState, EditorCore, PersistenceState, SaveDisposition};
use ls_scheduler::TaskState;
use ls_tests::{headless_editor, TempDir};
use std::time::{Duration, Instant};

fn pump_until(editor: &mut EditorCore, mut settled: impl FnMut(&EditorCore) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !settled(editor) {
        editor.pump_completions();
        assert!(Instant::now() < deadline, "timed out waiting for a save to settle");
        std::thread::yield_now();
    }
}

fn wait_for_task(editor: &EditorCore, task: ls_scheduler::TaskId) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !editor.scheduler().state(task).map(TaskState::is_terminal).unwrap_or(false) {
        assert!(Instant::now() < deadline, "timed out waiting for the worker");
        std::thread::yield_now();
    }
}

/// Opens a file and returns an editor with it active.
fn editor_with(
    contents: &str,
    directory: &TempDir,
    name: &str,
) -> (EditorCore, ls_core::DocumentId, std::path::PathBuf) {
    let path = directory.write(name, contents);
    let mut editor = headless_editor();
    let id = editor.open_document(&path).expect("opened");
    (editor, id, path)
}

// --- the two-version rule ----------------------------------------------------

#[test]
fn the_40_41_42_case() {
    // A save captures one exact version. The document moves on, then comes back
    // to the same content by undoing. The content revision has advanced; the
    // transaction token has not. Clean/dirty follows the token.
    let directory = TempDir::new("save-404142");
    let (mut editor, id, _path) = editor_with("original\n", &directory, "case.txt");

    let revision_at_save = editor.document(id).unwrap().revision();
    let token_at_save = editor.document(id).unwrap().transaction_token();

    let outcome = editor.request_save(id).expect("admitted");
    assert_eq!(outcome.revision, revision_at_save);
    assert_eq!(editor.document(id).unwrap().persistence_state(), PersistenceState::Saving);

    // The user keeps working while the save runs: an edit, then an undo that
    // returns the content to exactly what is being written.
    editor.type_text("edited ");
    let revision_after_edit = editor.document(id).unwrap().revision();
    assert!(revision_after_edit > revision_at_save);
    assert_eq!(editor.document(id).unwrap().content_state(), ContentState::Dirty);

    editor.undo_active();
    let revision_after_undo = editor.document(id).unwrap().revision();
    assert!(revision_after_undo > revision_after_edit, "revisions only increase");
    assert_eq!(
        editor.document(id).unwrap().transaction_token(),
        token_at_save,
        "undoing returned the history to the saved position"
    );

    pump_until(&mut editor, |editor| !editor.is_saving(id));

    let document = editor.document(id).unwrap();
    assert_eq!(document.saved_token(), token_at_save, "the captured token was recorded");
    assert_eq!(document.revision(), revision_after_undo, "the current revision is untouched");
    assert!(document.revision() > revision_at_save);
    assert_eq!(
        document.content_state(),
        ContentState::Clean,
        "the content on disk matches the history position, so the document is clean"
    );
    assert_eq!(document.text().to_string(), "original\n");
}

#[test]
fn a_save_that_lands_stale_leaves_the_document_dirty() {
    let directory = TempDir::new("save-stale");
    let (mut editor, id, path) = editor_with("before\n", &directory, "stale.txt");

    editor.request_save(id).expect("admitted");
    // Edit while the save runs, and do not undo: the token has moved on.
    editor.type_text("changed ");
    pump_until(&mut editor, |editor| !editor.is_saving(id));

    let document = editor.document(id).unwrap();
    assert_eq!(
        document.content_state(),
        ContentState::Dirty,
        "the file holds an older version than the buffer"
    );
    assert_eq!(document.persistence_state(), PersistenceState::SaveSucceeded);
    // What reached disk is the version that was captured.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\n");

    let record = editor.save_activity().recent().next().unwrap();
    assert_eq!(record.succeeded, Some(true));
    assert!(record.stale, "the record says the save landed stale");
}

#[test]
fn a_save_that_lands_current_makes_the_document_clean() {
    let directory = TempDir::new("save-current");
    let (mut editor, id, path) = editor_with("start\n", &directory, "current.txt");
    editor.type_text("more ");
    assert_eq!(editor.document(id).unwrap().content_state(), ContentState::Dirty);

    editor.request_save(id).expect("admitted");
    pump_until(&mut editor, |editor| !editor.is_saving(id));

    assert_eq!(editor.document(id).unwrap().content_state(), ContentState::Clean);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "more start\n");
    let record = editor.save_activity().recent().next().unwrap();
    assert!(!record.stale);
    assert!(record.bytes_written > 0);
}

// --- ownership ----------------------------------------------------------------

#[test]
fn save_state_changes_only_when_the_event_loop_pumps() {
    let directory = TempDir::new("save-ownership");
    let (mut editor, id, _path) = editor_with("content\n", &directory, "owned.txt");
    editor.type_text("x");

    let outcome = editor.request_save(id).expect("admitted");
    let task = outcome.task.expect("a fresh save has a task");
    assert_eq!(editor.document(id).unwrap().persistence_state(), PersistenceState::Saving);

    // Let the worker finish entirely, then deliberately do not pump.
    wait_for_task(&editor, task);
    std::thread::sleep(Duration::from_millis(20));

    assert!(editor.is_saving(id), "still in flight until the loop pumps");
    assert_eq!(
        editor.document(id).unwrap().content_state(),
        ContentState::Dirty,
        "a finished worker must not have cleaned the document by itself"
    );

    assert_eq!(editor.pump_completions(), 1);
    assert!(!editor.is_saving(id));
    assert_eq!(editor.document(id).unwrap().content_state(), ContentState::Clean);
}

#[test]
fn a_save_never_touches_text_cursor_selection_or_history() {
    let directory = TempDir::new("save-immutable");
    let (mut editor, id, _path) = editor_with("line one\nline two\n", &directory, "quiet.txt");

    editor.type_text("edited ");
    editor.execute("cursor.document_start", ls_core::CommandArgs::None).unwrap();
    editor.execute("cursor.right.select", ls_core::CommandArgs::None).unwrap();

    let before = editor.document(id).unwrap();
    let text = before.text().to_string();
    let selection = before.selections().primary();
    let undo_depth = before.undo_depth();
    let redo_depth = before.redo_depth();
    let revision = before.revision();

    editor.request_save(id).expect("admitted");
    pump_until(&mut editor, |editor| !editor.is_saving(id));

    let after = editor.document(id).unwrap();
    assert_eq!(after.text().to_string(), text, "text unchanged");
    assert_eq!(after.selections().primary(), selection, "selection unchanged");
    assert_eq!(after.undo_depth(), undo_depth, "undo history unchanged");
    assert_eq!(after.redo_depth(), redo_depth, "redo history unchanged");
    assert_eq!(after.revision(), revision, "save completion does not bump the revision");
}

// --- serialization -------------------------------------------------------------

#[test]
fn a_second_save_queues_and_a_third_supersedes_the_queued_one() {
    let directory = TempDir::new("save-serialize");
    let (mut editor, id, path) = editor_with("v0\n", &directory, "serial.txt");

    editor.type_text("a");
    let first = editor.request_save(id).expect("admitted");
    assert_eq!(first.disposition, SaveDisposition::Started);
    assert!(first.task.is_some());

    editor.type_text("b");
    let second = editor.request_save(id).expect("queued");
    assert_eq!(second.disposition, SaveDisposition::Queued);
    assert!(second.task.is_none(), "a queued save has no task yet");
    assert!(editor.has_queued_save(id));

    editor.type_text("c");
    let third = editor.request_save(id).expect("supersedes");
    assert_eq!(
        third.disposition,
        SaveDisposition::SupersededQueued,
        "the newer request replaces the queued one rather than adding to a chain"
    );

    pump_until(&mut editor, |editor| !editor.is_saving(id) && !editor.has_queued_save(id));

    // Exactly two writes happened: the first, and the newest queued one. The
    // superseded snapshot ("abv0") never reached disk.
    assert_eq!(editor.save_activity().len(), 2, "the superseded save never ran");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "abcv0\n",
        "the file holds the newest content, not the superseded snapshot"
    );
    assert_eq!(editor.document(id).unwrap().content_state(), ContentState::Clean);
}

#[test]
fn only_one_save_runs_at_a_time_for_a_document() {
    let directory = TempDir::new("save-one-writer");
    let (mut editor, id, _path) = editor_with("body\n", &directory, "one.txt");

    for index in 0..8 {
        editor.type_text(&index.to_string());
        editor.request_save(id).expect("admitted or queued");
        assert!(
            editor.saving_count() <= 1,
            "two writers for one document would race on the same file"
        );
    }
    pump_until(&mut editor, |editor| !editor.is_saving(id) && !editor.has_queued_save(id));
    assert!(editor.save_activity().len() <= 2, "queued saves collapse to the newest");
}

// --- failure and durability -----------------------------------------------------

#[test]
fn a_failed_save_leaves_the_original_intact_and_the_document_dirty() {
    let directory = TempDir::new("save-failure");
    let (mut editor, id, _path) = editor_with("precious\n", &directory, "safe.txt");
    editor.type_text("new ");

    // A directory cannot be replaced by a file: a real, typed failure.
    let blocked = directory.path().join("a-directory");
    std::fs::create_dir_all(&blocked).unwrap();

    editor.request_save_as(id, blocked.clone()).expect("admitted");
    pump_until(&mut editor, |editor| !editor.is_saving(id));

    let document = editor.document(id).unwrap();
    assert_eq!(document.content_state(), ContentState::Dirty, "nothing was persisted");
    assert_eq!(document.persistence_state(), PersistenceState::SaveFailed);
    assert!(blocked.is_dir(), "the directory is untouched");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("safe.txt")).unwrap(),
        "precious\n",
        "the original file is untouched"
    );

    let record = editor.save_activity().recent().next().unwrap();
    assert_eq!(record.succeeded, Some(false));
    assert!(record.error.is_some());
}

#[test]
fn the_disk_stamp_is_recorded_so_our_own_write_is_not_an_external_change() {
    let directory = TempDir::new("save-stamp");
    let (mut editor, id, _path) = editor_with("watched\n", &directory, "stamp.txt");
    editor.type_text("edit ");

    editor.request_save(id).expect("admitted");
    pump_until(&mut editor, |editor| !editor.is_saving(id));

    assert!(editor.document(id).unwrap().disk_stamp().is_some());
    assert_eq!(
        editor.refresh_external_state(id),
        Some(ls_core::ExternalState::Unchanged),
        "the editor must not report its own save as an external modification"
    );
}

#[test]
fn a_stale_completion_still_records_the_stamp() {
    let directory = TempDir::new("save-stale-stamp");
    let (mut editor, id, _path) = editor_with("v1\n", &directory, "stale-stamp.txt");

    editor.request_save(id).expect("admitted");
    editor.type_text("later ");
    pump_until(&mut editor, |editor| !editor.is_saving(id));

    assert_eq!(editor.document(id).unwrap().content_state(), ContentState::Dirty);
    assert_eq!(
        editor.refresh_external_state(id),
        Some(ls_core::ExternalState::Unchanged),
        "a stale save is still our write, not somebody else's"
    );
}

#[test]
fn encoding_and_line_endings_survive_an_async_save() {
    let directory = TempDir::new("save-encoding");
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in "héllo\r\nwörld".encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let path = directory.write("utf16.txt", &utf16);
    let mut editor = headless_editor();
    let id = editor.open_document(&path).expect("opened");

    editor.request_save(id).expect("admitted");
    pump_until(&mut editor, |editor| !editor.is_saving(id));

    assert_eq!(directory.read("utf16.txt"), utf16, "bytes round trip exactly");
}

#[test]
fn save_as_adopts_the_new_path_and_leaves_the_original() {
    let directory = TempDir::new("save-as-async");
    let (mut editor, id, original) = editor_with("content\n", &directory, "original.txt");
    editor.type_text("new ");

    let copy = directory.path().join("copy.txt");
    editor.request_save_as(id, copy.clone()).expect("admitted");
    pump_until(&mut editor, |editor| !editor.is_saving(id));

    assert_eq!(std::fs::read_to_string(&original).unwrap(), "content\n");
    assert_eq!(std::fs::read_to_string(&copy).unwrap(), "new content\n");
    let document = editor.document(id).unwrap();
    assert_eq!(document.path().unwrap().as_path(), copy);
    assert_eq!(document.content_state(), ContentState::Clean);
    assert_eq!(document.display_name(), "copy.txt");
}

// --- accounting -----------------------------------------------------------------

#[test]
fn saves_are_accounted_under_the_document_io_subsystem() {
    let directory = TempDir::new("save-accounting");
    let (mut editor, id, _path) =
        editor_with(&"a line of text\n".repeat(500), &directory, "counted.txt");
    editor.type_text("x");

    let outcome = editor.request_save(id).expect("admitted");
    let task = outcome.task.unwrap();
    pump_until(&mut editor, |editor| !editor.is_saving(id));

    let record = editor
        .scheduler()
        .recent_records()
        .into_iter()
        .find(|record| record.task_id == task)
        .expect("the scheduler kept an accounting record");
    assert_eq!(record.subsystem, ls_scheduler::SubsystemId::DOCUMENT_IO);
    assert_eq!(record.outcome, TaskState::Completed);
    assert!(record.bytes_written > 0, "the save reported what it wrote");
    assert!(record.workspace.is_some());

    let activity = editor.save_activity().recent().next().unwrap();
    assert!(activity.queue_wait.is_some());
    assert!(activity.wall_time.is_some());
    assert!(activity.total.is_some());
}

// --- the interactive guarantee ----------------------------------------------------

#[test]
fn editing_stays_responsive_while_a_large_file_saves() {
    let directory = TempDir::new("save-responsive");
    let (mut editor, id, _path) =
        editor_with(&"a line of source code here\n".repeat(300_000), &directory, "large.txt");

    let scratch = editor.new_document();
    editor.set_active(id).unwrap();
    editor.request_save(id).expect("admitted");
    editor.set_active(scratch).unwrap();

    let mut worst = Duration::ZERO;
    let mut keystrokes = 0u32;
    while editor.is_saving(id) {
        let started = Instant::now();
        editor.type_text("x");
        worst = worst.max(started.elapsed());
        keystrokes += 1;
        editor.pump_completions();
        assert!(keystrokes < 5_000_000, "the save never finished");
    }

    assert!(keystrokes > 0, "the save finished before a keystroke: use a bigger file");
    assert!(
        worst < Duration::from_millis(5),
        "worst keystroke during a large save was {worst:?}, past the 5 ms failure threshold"
    );
    assert_eq!(
        editor.document(scratch).unwrap().text().len_chars(),
        keystrokes as usize,
        "every keystroke landed"
    );
}
