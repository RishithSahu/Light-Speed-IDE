//! End-to-end tests through the public editor-core API.
//!
//! Each test is a scenario a user can actually perform: open a file, type,
//! undo, save, reopen. They exercise the same entry points the shell uses, so a
//! green suite means the editor works even though no window was ever created.

use ls_core::{
    CommandArgs, ContentState, DisplayColumn, Encoding, ExternalState, LineIndex, PersistenceState,
    Viewport,
};
use ls_tests::{headless_editor, TempDir};

fn viewport(first_line: usize, lines: usize) -> Viewport {
    Viewport {
        first_line: LineIndex::new(first_line),
        visible_lines: lines,
        first_column: DisplayColumn::ZERO,
        visible_columns: 200,
    }
}

#[test]
fn open_edit_save_reopen() {
    let directory = TempDir::new("open-edit-save");
    let path = directory.write("main.rs", "fn main() {}\n");
    let mut editor = headless_editor();

    let id = editor.open_document(&path).unwrap();
    assert_eq!(editor.document(id).unwrap().text().to_string(), "fn main() {}\n");
    assert_eq!(editor.document(id).unwrap().content_state(), ContentState::Clean);

    editor.execute("cursor.document_end", CommandArgs::None).unwrap();
    editor.execute("edit.insert_text", CommandArgs::Text("// done\n".into())).unwrap();
    assert!(editor.document(id).unwrap().is_dirty());

    editor.save(id).unwrap();
    assert_eq!(editor.document(id).unwrap().content_state(), ContentState::Clean);
    assert_eq!(editor.document(id).unwrap().persistence_state(), PersistenceState::SaveSucceeded);
    assert_eq!(directory.read_string("main.rs"), "fn main() {}\n// done\n");

    // A second editor sees exactly what the first one wrote.
    let mut reopened = headless_editor();
    let reopened_id = reopened.open_document(&path).unwrap();
    assert_eq!(
        reopened.document(reopened_id).unwrap().text().to_string(),
        "fn main() {}\n// done\n"
    );
}

#[test]
fn one_file_is_one_document_however_it_is_addressed() {
    // Specification section 24.
    let directory = TempDir::new("document-identity");
    let path = directory.write("notes.txt", "hello");
    let indirect = directory.path().join(".").join("notes.txt");
    let mut editor = headless_editor();

    let first = editor.open_document(&path).unwrap();
    let second = editor.open_document(&indirect).unwrap();
    assert_eq!(first, second);
    assert_eq!(editor.tabs().len(), 1);
}

#[test]
fn typing_is_one_undo_step_and_undo_restores_the_selection() {
    let mut editor = headless_editor();
    let id = editor.new_document();
    for character in "hello".chars() {
        editor.execute("edit.insert_text", CommandArgs::Text(character.to_string())).unwrap();
    }
    assert_eq!(editor.document(id).unwrap().text().to_string(), "hello");
    assert_eq!(editor.document(id).unwrap().undo_depth(), 1, "typing should coalesce");

    editor.execute("edit.undo", CommandArgs::None).unwrap();
    assert_eq!(editor.document(id).unwrap().text().to_string(), "");
    assert_eq!(editor.document(id).unwrap().selections().primary().head.get(), 0);

    editor.execute("edit.redo", CommandArgs::None).unwrap();
    assert_eq!(editor.document(id).unwrap().text().to_string(), "hello");
    assert_eq!(editor.document(id).unwrap().selections().primary().head.get(), 5);
}

#[test]
fn clipboard_round_trip_through_the_core() {
    let mut editor = headless_editor();
    let id = editor.new_document();
    editor.execute("edit.insert_text", CommandArgs::Text("copy me".into())).unwrap();
    editor.execute("edit.select_all", CommandArgs::None).unwrap();
    editor.execute("edit.copy", CommandArgs::None).unwrap();
    editor.execute("cursor.document_end", CommandArgs::None).unwrap();
    editor.execute("edit.paste", CommandArgs::None).unwrap();
    assert_eq!(editor.document(id).unwrap().text().to_string(), "copy mecopy me");

    editor.execute("edit.select_all", CommandArgs::None).unwrap();
    editor.execute("edit.cut", CommandArgs::None).unwrap();
    assert_eq!(editor.document(id).unwrap().text().to_string(), "");
}

#[test]
fn tabs_reference_documents_without_reloading_them() {
    let directory = TempDir::new("tabs");
    let first = directory.write("a.txt", "first file");
    let second = directory.write("b.txt", "second file");
    let mut editor = headless_editor();

    let a = editor.open_document(&first).unwrap();
    let b = editor.open_document(&second).unwrap();
    assert_eq!(editor.tabs(), &[a, b]);
    assert_eq!(editor.active(), Some(b));

    // Edit b, switch away and back: the unsaved edit is still there, which is
    // only true if the tab references the document rather than the file.
    editor.execute("edit.insert_text", CommandArgs::Text("EDIT".into())).unwrap();
    editor.set_active(a).unwrap();
    editor.set_active(b).unwrap();
    assert!(editor.document(b).unwrap().text().to_string().starts_with("EDIT"));
    assert!(editor.document(b).unwrap().is_dirty());

    // The file on disk is untouched until a save.
    assert_eq!(directory.read_string("b.txt"), "second file");
}

#[test]
fn closing_a_dirty_tab_is_refused_and_closing_a_clean_one_is_not() {
    let directory = TempDir::new("close-tab");
    let path = directory.write("a.txt", "content");
    let mut editor = headless_editor();
    let id = editor.open_document(&path).unwrap();

    editor.execute("edit.insert_text", CommandArgs::Text("x".into())).unwrap();
    assert!(editor.close_document(id).is_err(), "unsaved work must not vanish silently");

    editor.save(id).unwrap();
    assert!(editor.close_document(id).is_ok());
    assert!(editor.tabs().is_empty());
}

#[test]
fn external_modification_is_detected_and_conflicts_are_reported() {
    // Specification section 25: content state and external state are separate.
    let directory = TempDir::new("external-change");
    let path = directory.write("watched.txt", "original\n");
    let mut editor = headless_editor();
    let id = editor.open_document(&path).unwrap();
    assert_eq!(editor.refresh_external_state(id), Some(ExternalState::Unchanged));

    // Someone else edits the file.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&path, "changed by another program\n").unwrap();
    assert_eq!(editor.refresh_external_state(id), Some(ExternalState::ExternallyChanged));

    // With local edits as well, it is a conflict, and the buffer is untouched.
    editor.execute("edit.insert_text", CommandArgs::Text("mine".into())).unwrap();
    assert_eq!(editor.refresh_external_state(id), Some(ExternalState::Conflict));
    assert!(editor.document(id).unwrap().text().to_string().starts_with("mine"));

    // Deleting the file is a third, distinct state.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(editor.refresh_external_state(id), Some(ExternalState::Missing));
}

#[test]
fn utf16_and_bom_files_round_trip_byte_for_byte() {
    // Specification section 19.
    let directory = TempDir::new("encodings");

    let mut utf16 = vec![0xFF, 0xFE];
    for unit in "héllo\r\nwörld".encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let utf16_path = directory.write("utf16.txt", &utf16);

    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice("with bom\n".as_bytes());
    let bom_path = directory.write("bom.txt", &bom);

    let mut editor = headless_editor();
    let utf16_id = editor.open_document(&utf16_path).unwrap();
    assert_eq!(editor.document(utf16_id).unwrap().encoding(), Encoding::Utf16Le);
    assert_eq!(editor.document(utf16_id).unwrap().text().to_string(), "héllo\nwörld");
    editor.save(utf16_id).unwrap();
    assert_eq!(directory.read("utf16.txt"), utf16);

    let bom_id = editor.open_document(&bom_path).unwrap();
    assert_eq!(editor.document(bom_id).unwrap().encoding(), Encoding::Utf8Bom);
    editor.save(bom_id).unwrap();
    assert_eq!(directory.read("bom.txt"), bom);
}

#[test]
fn binary_files_are_refused_rather_than_mangled() {
    // Specification sections 19 and 47.
    let directory = TempDir::new("binary");
    let path = directory.write("image.png", [0x89, b'P', b'N', b'G', 0x00, 0x1A, 0x0A, 0xFF]);
    let mut editor = headless_editor();
    let error = editor.open_document(&path).unwrap_err();
    assert!(error.to_string().contains("binary"), "{error}");
    assert!(editor.tabs().is_empty());
}

#[test]
fn save_as_moves_the_document_and_leaves_the_original() {
    let directory = TempDir::new("save-as");
    let original = directory.write("original.txt", "content\n");
    let mut editor = headless_editor();
    let id = editor.open_document(&original).unwrap();

    editor.execute("edit.insert_text", CommandArgs::Text("new ".into())).unwrap();
    let copy = directory.path().join("copy.txt");
    editor.save_as(id, copy.clone()).unwrap();

    assert_eq!(directory.read_string("original.txt"), "content\n");
    assert_eq!(directory.read_string("copy.txt"), "new content\n");
    assert_eq!(editor.document(id).unwrap().path().unwrap().as_path(), copy);
    assert!(!editor.document(id).unwrap().is_dirty());
}

#[test]
fn a_snapshot_matches_the_document_it_was_taken_from() {
    let mut editor = headless_editor();
    let id = editor.new_document();
    editor.execute("edit.insert_text", CommandArgs::Text("alpha\nbeta\ngamma".into())).unwrap();

    let snapshot = editor.render_snapshot(id, viewport(0, 10)).unwrap();
    assert_eq!(snapshot.total_lines, 3);
    assert_eq!(snapshot.lines.len(), 3);
    assert_eq!(&*snapshot.lines[1].text, "beta");
    assert_eq!(snapshot.content_revision, editor.document(id).unwrap().revision());
    assert_eq!(snapshot.cursors.len(), 1);
    assert_eq!(snapshot.cursors[0].line, LineIndex::new(2));
    assert!(snapshot.document.dirty);

    // An older snapshot keeps showing the revision it was built from.
    editor.execute("edit.insert_text", CommandArgs::Text("!".into())).unwrap();
    assert_eq!(&*snapshot.lines[2].text, "gamma");
    let newer = editor.render_snapshot(id, viewport(0, 10)).unwrap();
    assert_eq!(&*newer.lines[2].text, "gamma!");
    assert!(newer.content_revision > snapshot.content_revision);
}

#[test]
fn selection_spans_cover_multiple_lines() {
    let mut editor = headless_editor();
    let id = editor.new_document();
    editor.execute("edit.insert_text", CommandArgs::Text("one\ntwo\nthree".into())).unwrap();
    editor.execute("edit.select_all", CommandArgs::None).unwrap();

    let snapshot = editor.render_snapshot(id, viewport(0, 10)).unwrap();
    assert_eq!(snapshot.selections.len(), 3);
    assert!(snapshot.selections[0].includes_line_break);
    assert!(!snapshot.selections[2].includes_line_break);
    assert_eq!(snapshot.selections[2].end_column_chars, 5);
}

#[test]
fn go_to_line_accepts_a_position() {
    let mut editor = headless_editor();
    let id = editor.new_document();
    editor.execute("edit.insert_text", CommandArgs::Text("aaa\nbbb\nccc\n".into())).unwrap();
    editor.execute("cursor.go_to", CommandArgs::Position { line: 1, column: 2 }).unwrap();

    let document = editor.document(id).unwrap();
    let head = document.selections().primary().head;
    assert_eq!(document.text().char_to_line(head), LineIndex::new(1));
    assert_eq!(head.get(), 6);
}

#[test]
fn commands_are_disabled_when_they_do_not_apply() {
    let mut editor = headless_editor();
    assert!(!editor.is_command_enabled("edit.undo"));
    assert!(!editor.is_command_enabled("edit.copy"));

    let _id = editor.new_document();
    editor.execute("edit.insert_text", CommandArgs::Text("text".into())).unwrap();
    assert!(editor.is_command_enabled("edit.undo"));
    assert!(!editor.is_command_enabled("edit.copy"), "nothing is selected yet");

    editor.execute("edit.select_all", CommandArgs::None).unwrap();
    assert!(editor.is_command_enabled("edit.copy"));
}

#[test]
fn events_describe_what_happened() {
    // Specification section 37.
    let directory = TempDir::new("events");
    let path = directory.write("a.txt", "hello\n");
    let mut editor = headless_editor();

    let id = editor.open_document(&path).unwrap();
    editor.execute("edit.insert_text", CommandArgs::Text("x".into())).unwrap();
    editor.save(id).unwrap();

    let kinds: Vec<&'static str> =
        editor.drain_events().iter().map(|event| event.payload.name()).collect();
    assert!(kinds.contains(&"document_opened"), "{kinds:?}");
    assert!(kinds.contains(&"document_edited"), "{kinds:?}");
    assert!(kinds.contains(&"document_saved"), "{kinds:?}");
}

#[test]
fn a_hundred_megabyte_document_opens_and_edits() {
    // Specification section 21: large files must not be assumed away. This is
    // the smallest test that proves the viewport path never touches the whole
    // document.
    let directory = TempDir::new("large-file");
    let line = "a moderately long line of source code, about seventy characters ok\n";
    let content = line.repeat(1_500_000); // ~100 MB
    let path = directory.write("huge.txt", &content);

    let mut editor = headless_editor();
    let opened = std::time::Instant::now();
    let id = editor.open_document(&path).unwrap();
    let open_time = opened.elapsed();

    let document = editor.document(id).unwrap();
    assert_eq!(document.text().len_lines(), 1_500_001);

    // Snapshot the middle of the file and type there.
    let snapshot = editor.render_snapshot(id, viewport(750_000, 60)).unwrap();
    assert_eq!(snapshot.lines.len(), 60);

    editor.execute("cursor.go_to", CommandArgs::Position { line: 750_000, column: 0 }).unwrap();
    let typed = std::time::Instant::now();
    editor.execute("edit.insert_text", CommandArgs::Text("X".into())).unwrap();
    let type_time = typed.elapsed();

    assert!(
        type_time < std::time::Duration::from_millis(5),
        "typing in a 100 MB document took {type_time:?}"
    );
    assert_eq!(
        editor.document(id).unwrap().text().line_text(LineIndex::new(750_000)).chars().next(),
        Some('X')
    );
    // Opening is allowed to be slow-ish, but not absurd; this guards a
    // regression to a quadratic loader.
    assert!(open_time < std::time::Duration::from_secs(10), "open took {open_time:?}");
}
