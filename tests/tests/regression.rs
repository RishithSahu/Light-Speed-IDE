//! Regression tests.
//!
//! Each test here records a specific behaviour that was wrong at some point, or
//! that is subtle enough that a future change could plausibly break it without
//! anyone noticing. Every test says what it is protecting and why.

use ls_core::{CommandArgs, DisplayColumn, LineIndex, Viewport};
use ls_tests::{headless_editor, TempDir};

fn viewport(first_line: usize, lines: usize) -> Viewport {
    Viewport {
        first_line: LineIndex::new(first_line),
        visible_lines: lines,
        first_column: DisplayColumn::ZERO,
        visible_columns: 120,
    }
}

#[test]
fn replaying_an_edit_script_is_deterministic() {
    // Specification section 25. Future Time Machine features reconstruct state
    // by replaying edits, so identical input must give identical output - down
    // to the revision number and the cursor.
    let script: &[(&str, CommandArgs)] = &[
        ("edit.insert_text", CommandArgs::Text("fn main() {\n".into())),
        ("edit.insert_text", CommandArgs::Text("    let x = 1;\n".into())),
        ("edit.insert_text", CommandArgs::Text("}\n".into())),
        ("cursor.document_start", CommandArgs::None),
        ("cursor.down", CommandArgs::None),
        ("cursor.line_end", CommandArgs::None),
        ("edit.insert_text", CommandArgs::Text(" // comment".into())),
        ("edit.undo", CommandArgs::None),
        ("edit.redo", CommandArgs::None),
        ("cursor.word_left", CommandArgs::None),
        ("edit.delete_word_backward", CommandArgs::None),
    ];

    let run = || {
        let mut editor = headless_editor();
        let id = editor.new_document();
        for (command, args) in script {
            editor.execute(command, args.clone()).unwrap();
        }
        let document = editor.document(id).unwrap();
        (
            document.text().to_string(),
            document.revision().get(),
            document.selections().primary().head.get(),
            document.undo_depth(),
        )
    };

    assert_eq!(run(), run());
}

#[test]
fn undo_after_a_paste_removes_the_whole_paste() {
    // A paste is one transaction even though it inserts many characters, and it
    // must not merge with typing that happened just before it.
    let mut editor = headless_editor();
    let id = editor.new_document();
    editor.execute("edit.insert_text", CommandArgs::Text("start".into())).unwrap();
    editor.execute("edit.paste", CommandArgs::Text("<pasted block>".into())).unwrap();
    assert_eq!(editor.document(id).unwrap().text().to_string(), "start<pasted block>");

    editor.execute("edit.undo", CommandArgs::None).unwrap();
    assert_eq!(editor.document(id).unwrap().text().to_string(), "start");
    editor.execute("edit.undo", CommandArgs::None).unwrap();
    assert_eq!(editor.document(id).unwrap().text().to_string(), "");
}

#[test]
fn a_cursor_jump_ends_the_typing_group() {
    // Specification section 23: moving the caret is a coalescing boundary, so
    // undo does not swallow text typed somewhere else.
    let mut editor = headless_editor();
    let id = editor.new_document();
    editor.execute("edit.insert_text", CommandArgs::Text("hello world".into())).unwrap();
    editor.execute("cursor.document_start", CommandArgs::None).unwrap();
    editor.execute("edit.insert_text", CommandArgs::Text("A".into())).unwrap();

    editor.execute("edit.undo", CommandArgs::None).unwrap();
    assert_eq!(
        editor.document(id).unwrap().text().to_string(),
        "hello world",
        "undo should only remove the text typed after the jump"
    );
}

#[test]
fn editing_before_the_cursor_keeps_it_on_the_same_text() {
    // The specification's own example: a cursor must stay logically attached to
    // the text it pointed at when an edit happens before it.
    let mut editor = headless_editor();
    let id = editor.new_document();
    editor.execute("edit.insert_text", CommandArgs::Text("Hello World".into())).unwrap();
    editor.execute("cursor.go_to", CommandArgs::Position { line: 0, column: 6 }).unwrap();

    // Insert in front of the caret via the document API used by the shell.
    editor.insert(id, ls_core::Position::new(0, 0), ">> ").unwrap();

    let document = editor.document(id).unwrap();
    let head = document.selections().primary().head;
    assert_eq!(document.text().to_string(), ">> Hello World");
    assert_eq!(head.get(), 9, "the caret should still sit before `World`");
    assert_eq!(document.text().slice(head..document.text().end()), "World");
}

#[test]
fn deleting_a_selection_that_spans_chunks_leaves_valid_text() {
    // Guards the rope's rebalancing: a delete that removes whole internal nodes
    // used to be the easiest way to corrupt the tree.
    let mut editor = headless_editor();
    let directory = TempDir::new("regression-large-delete");
    let path = directory.write("big.txt", "0123456789\n".repeat(50_000));
    let id = editor.open_document(&path).unwrap();

    let (start, end) = {
        let buffer = editor.document(id).unwrap().text();
        (buffer.line_to_char(LineIndex::new(10)), buffer.line_to_char(LineIndex::new(49_990)))
    };
    editor.set_selection(ls_core::Selection { anchor: start, head: end, goal_column: None });
    editor.execute("edit.delete_forward", CommandArgs::None).unwrap();

    let document = editor.document(id).unwrap();
    document.text().validate().expect("the rope must stay valid after a huge delete");
    assert_eq!(document.text().len_lines(), 21);
    assert!(document.text().to_string().starts_with("0123456789\n"));
}

#[test]
fn snapshot_columns_are_characters_not_bytes() {
    // A wide or accented character must not shift the caret's reported column:
    // the classic byte/char confusion the position types exist to prevent.
    let mut editor = headless_editor();
    let id = editor.new_document();
    editor.execute("edit.insert_text", CommandArgs::Text("héllo \u{4F60}\u{597D}".into())).unwrap();

    let snapshot = editor.render_snapshot(id, viewport(0, 4)).unwrap();
    let cursor = &snapshot.cursors[0];
    assert_eq!(cursor.column_chars, 8, "eight characters precede the caret");
    assert_eq!(
        cursor.display_column,
        DisplayColumn::new(10),
        "the two wide characters take two columns each"
    );
}

#[test]
fn a_document_ending_without_a_newline_keeps_its_shape() {
    // Saving must not helpfully add or remove a trailing newline.
    let directory = TempDir::new("regression-trailing-newline");
    let path = directory.write("no-newline.txt", "last line has no break");
    let mut editor = headless_editor();
    let id = editor.open_document(&path).unwrap();

    editor.execute("cursor.document_end", CommandArgs::None).unwrap();
    editor.execute("edit.insert_text", CommandArgs::Text("!".into())).unwrap();
    editor.save(id).unwrap();

    assert_eq!(directory.read_string("no-newline.txt"), "last line has no break!");
}

#[test]
fn saving_twice_in_a_row_is_stable() {
    // The second save must be a no-op on disk, not a rewrite with different
    // bytes, and must not leave temporary files behind.
    let directory = TempDir::new("regression-double-save");
    let path = directory.write("stable.txt", "alpha\r\nbeta\r\n");
    let mut editor = headless_editor();
    let id = editor.open_document(&path).unwrap();

    editor.execute("edit.insert_text", CommandArgs::Text("x".into())).unwrap();
    editor.save(id).unwrap();
    let first = directory.read("stable.txt");
    editor.save(id).unwrap();
    assert_eq!(directory.read("stable.txt"), first);

    let leftovers: Vec<String> = std::fs::read_dir(directory.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("lightspeed-"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left behind: {leftovers:?}");
}

#[test]
fn switching_tabs_does_no_work_proportional_to_the_document() {
    // Specification section 48: a loaded tab switch has a 2 ms target. The
    // guard here is structural - switching must not re-read or re-scan - so the
    // test uses a document big enough that any rescan would be obvious.
    let directory = TempDir::new("regression-tab-switch");
    let small = directory.write("small.txt", "small\n");
    let large = directory.write("large.txt", "a line of text\n".repeat(400_000));

    let mut editor = headless_editor();
    let small_id = editor.open_document(&small).unwrap();
    let large_id = editor.open_document(&large).unwrap();

    let started = std::time::Instant::now();
    for _ in 0..20 {
        editor.set_active(small_id).unwrap();
        editor.set_active(large_id).unwrap();
    }
    let elapsed = started.elapsed() / 40;
    assert!(
        elapsed < std::time::Duration::from_millis(2),
        "a tab switch took {elapsed:?} on average"
    );
}
