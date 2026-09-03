//! Asynchronous document opening (amendment section 7, ADR-0015).
//!
//! These tests exercise the same entry points the shell uses. The one that
//! matters most is
//! [`document_state_changes_only_when_the_event_loop_pumps`]: it is the proof
//! that a worker never mutates editor state, which is the invariant every other
//! guarantee in Stage 1.1 rests on.

use ls_core::{EditorCore, EffectiveConfig, LoadInjection, LoadState, OpenDocumentError};
use ls_platform::MemoryClipboard;
use ls_scheduler::{SchedulerConfig, TaskState};
use ls_tests::{headless_editor, TempDir};
use std::time::{Duration, Instant};

/// Pumps until `settled`, failing rather than hanging.
fn pump_until(editor: &mut EditorCore, mut settled: impl FnMut(&EditorCore) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !settled(editor) {
        editor.pump_completions();
        assert!(Instant::now() < deadline, "timed out waiting for a load to settle");
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

fn event_names(editor: &mut EditorCore) -> Vec<&'static str> {
    editor.drain_events().iter().map(|event| event.payload.name()).collect()
}

// --- the load-ownership invariant -------------------------------------------

#[test]
fn document_state_changes_only_when_the_event_loop_pumps() {
    // A worker produces a value; the interactive thread applies it. Nothing
    // else may touch a document (amendment section 3.6).
    let directory = TempDir::new("async-ownership");
    let path = directory.write("owned.txt", "content that had to be read\n");
    let mut editor = headless_editor();

    let request = editor
        .request_open_document_with(&path, LoadInjection::delayed(Duration::from_millis(120)))
        .expect("admitted");
    let task = request.task.expect("a fresh load has a task");

    // Immediately after the request: a tab, no document.
    assert!(editor.is_loading(request.document));
    assert!(editor.document(request.document).is_none());
    assert_eq!(editor.tabs(), &[request.document]);

    // Let the worker finish completely, and deliberately do not pump.
    wait_for_task(&editor, task);
    std::thread::sleep(Duration::from_millis(20));

    assert!(
        editor.document(request.document).is_none(),
        "a finished worker must not have installed the document by itself"
    );
    assert!(editor.is_loading(request.document), "still loading until the loop pumps");

    // Pumping is what applies it.
    let applied = editor.pump_completions();
    assert_eq!(applied, 1);
    assert!(!editor.is_loading(request.document));
    let document = editor.document(request.document).expect("now it exists");
    assert_eq!(document.text().to_string(), "content that had to be read\n");
}

// --- the ordinary path -------------------------------------------------------

#[test]
fn a_valid_open_produces_a_document_and_its_events() {
    let directory = TempDir::new("async-valid");
    let path = directory.write("main.rs", "fn main() {}\n");
    let mut editor = headless_editor();

    let request = editor.request_open_document(&path).expect("admitted");
    assert!(!request.joined);
    assert!(!request.already_open);
    assert!(request.task.is_some());

    let names = event_names(&mut editor);
    assert!(names.contains(&"document_load_started"), "{names:?}");

    pump_until(&mut editor, |editor| !editor.is_loading(request.document));

    let document = editor.document(request.document).expect("loaded");
    assert_eq!(document.text().to_string(), "fn main() {}\n");
    assert_eq!(editor.active(), Some(request.document));

    let names = event_names(&mut editor);
    assert!(names.contains(&"document_opened"), "{names:?}");

    let record = editor.load_activity().recent().next().expect("an activity record");
    assert_eq!(record.state, LoadState::Loaded);
    assert_eq!(record.joins, 1);
    assert!(record.total.is_some());
    assert!(record.bytes > 0);
}

#[test]
fn the_loading_tab_is_visible_and_selectable_before_the_document_arrives() {
    let directory = TempDir::new("async-tab");
    let path = directory.write("slow.txt", "body");
    let mut editor = headless_editor();

    let request = editor
        .request_open_document_with(&path, LoadInjection::delayed(Duration::from_millis(150)))
        .expect("admitted");

    let tabs = editor.tab_presentations();
    assert_eq!(tabs.len(), 1);
    assert!(tabs[0].loading, "the tab reports that it is loading");
    assert!(tabs[0].active);
    assert_eq!(tabs[0].title, "slow.txt");
    assert!(editor.set_active(request.document).is_ok(), "a loading tab is selectable");

    pump_until(&mut editor, |editor| !editor.is_loading(request.document));
    assert!(!editor.tab_presentations()[0].loading);
}

// --- failure -----------------------------------------------------------------

#[test]
fn a_missing_file_fails_before_a_task_is_created() {
    // Canonicalization happens on the interactive thread, so a path that does
    // not exist is refused immediately rather than becoming a doomed task.
    let directory = TempDir::new("async-missing");
    let mut editor = headless_editor();
    let error = editor
        .request_open_document(&directory.path().join("nothing-here.txt"))
        .expect_err("a missing file cannot be opened");
    assert!(matches!(error, OpenDocumentError::NotFound(_)), "{error}");
    assert!(editor.tabs().is_empty(), "no tab is left behind");
}

#[test]
fn a_binary_file_fails_during_the_load_and_removes_its_tab() {
    let directory = TempDir::new("async-binary");
    let path = directory.write("image.png", [0x89, b'P', b'N', b'G', 0x00, 0x1A]);
    let mut editor = headless_editor();

    let request = editor.request_open_document(&path).expect("admitted");
    assert_eq!(editor.tabs().len(), 1, "the tab exists while the file is being read");

    pump_until(&mut editor, |editor| !editor.is_loading(request.document));

    assert!(editor.tabs().is_empty(), "a failed load leaves no tab");
    assert!(editor.document(request.document).is_none());
    let record = editor.load_activity().recent().next().expect("a record");
    assert_eq!(record.state, LoadState::Failed);
    assert!(record.error.as_deref().unwrap().contains("binary"), "{record:?}");

    let names = event_names(&mut editor);
    assert!(names.contains(&"document_load_failed"), "{names:?}");
}

#[test]
fn an_injected_failure_is_reported_to_the_application() {
    let directory = TempDir::new("async-injected");
    let path = directory.write("fine.txt", "this file is perfectly readable");
    let mut editor = headless_editor();

    let request =
        editor.request_open_document_with(&path, LoadInjection::failing()).expect("admitted");
    pump_until(&mut editor, |editor| !editor.is_loading(request.document));

    let record = editor.load_activity().recent().next().unwrap();
    assert_eq!(record.state, LoadState::Failed);
    assert!(record.error.as_deref().unwrap().contains("injected"), "{record:?}");
    assert!(editor.take_last_error().is_some(), "the shell is told");
}

// --- cancellation ------------------------------------------------------------

#[test]
fn a_load_can_be_cancelled_and_reports_cancelled_not_failed() {
    let directory = TempDir::new("async-cancel");
    let path = directory.write("big.txt", "line\n".repeat(200_000));
    let mut editor = headless_editor();

    let request = editor
        .request_open_document_with(&path, LoadInjection::delayed(Duration::from_secs(30)))
        .expect("admitted");

    assert!(editor.cancel_open(request.document), "an in-flight load is cancellable");
    pump_until(&mut editor, |editor| !editor.is_loading(request.document));

    assert!(editor.tabs().is_empty());
    assert!(editor.document(request.document).is_none());
    let record = editor.load_activity().recent().next().unwrap();
    assert_eq!(record.state, LoadState::Cancelled);
    assert!(record.error.is_none(), "cancellation is not a failure");

    let names = event_names(&mut editor);
    assert!(names.contains(&"document_load_cancelled"), "{names:?}");
}

#[test]
fn cancelling_a_document_that_is_not_loading_does_nothing() {
    let directory = TempDir::new("async-cancel-loaded");
    let path = directory.write("done.txt", "already here");
    let mut editor = headless_editor();
    let id = editor.open_document(&path).expect("opened");
    assert!(!editor.cancel_open(id), "a loaded document has no load to cancel");
    assert!(editor.document(id).is_some(), "and is left alone");
}

#[test]
fn closing_a_loading_tab_cancels_its_load() {
    let directory = TempDir::new("async-close-loading");
    let path = directory.write("closing.txt", "content");
    let mut editor = headless_editor();

    let request = editor
        .request_open_document_with(&path, LoadInjection::delayed(Duration::from_secs(30)))
        .expect("admitted");
    editor.close_document(request.document).expect("closing a loading tab is allowed");
    pump_until(&mut editor, |editor| !editor.is_loading(request.document));
    assert!(editor.tabs().is_empty());
}

// --- identity and joining ----------------------------------------------------

#[test]
fn two_requests_for_one_path_join_a_single_task() {
    let directory = TempDir::new("async-join");
    let path = directory.write("shared.txt", "one copy only");
    let mut editor = headless_editor();

    let first = editor
        .request_open_document_with(&path, LoadInjection::delayed(Duration::from_millis(200)))
        .expect("admitted");
    let second = editor.request_open_document(&path).expect("joins");

    assert_eq!(first.document, second.document, "one document");
    assert_eq!(first.task, second.task, "one task");
    assert!(second.joined);
    assert!(!second.already_open);
    assert_eq!(editor.tabs().len(), 1, "one tab");

    pump_until(&mut editor, |editor| !editor.is_loading(first.document));
    assert!(editor.document(first.document).is_some());
}

#[test]
fn n_duplicate_requests_result_in_exactly_one_underlying_load() {
    const REQUESTS: usize = 8;
    let directory = TempDir::new("async-join-many");
    let path = directory.write("hot.txt", "read me once");
    let mut editor = headless_editor();

    let first = editor
        .request_open_document_with(&path, LoadInjection::delayed(Duration::from_millis(300)))
        .expect("admitted");
    let task = first.task.expect("a task");

    for _ in 1..REQUESTS {
        let joined = editor.request_open_document(&path).expect("joins");
        assert_eq!(joined.task, Some(task));
        assert!(joined.joined);
    }

    assert_eq!(editor.tabs().len(), 1);
    assert_eq!(
        editor.pending_load(first.document).map(|pending| pending.joins),
        Some(REQUESTS as u32)
    );

    pump_until(&mut editor, |editor| !editor.is_loading(first.document));

    // Exactly one load ran, and the activity says how many rode on it.
    let records: Vec<_> = editor.load_activity().recent().collect();
    assert_eq!(records.len(), 1, "one load, not {REQUESTS}");
    assert_eq!(records[0].joins, REQUESTS as u32);
    assert!(records[0].is_joined());
    assert_eq!(records[0].task, task);
}

#[test]
fn a_request_for_an_already_open_document_does_not_reload_it() {
    let directory = TempDir::new("async-already-open");
    let path = directory.write("open.txt", "on disk");
    let mut editor = headless_editor();

    let id = editor.open_document(&path).expect("opened");
    // Change the file behind the editor's back: a reload would pick this up,
    // and it must not.
    std::fs::write(&path, "changed on disk").unwrap();

    let again = editor.request_open_document(&path).expect("already open");
    assert_eq!(again.document, id);
    assert!(again.already_open);
    assert!(again.task.is_none(), "nothing was scheduled");
    assert_eq!(editor.document(id).unwrap().text().to_string(), "on disk");
    assert_eq!(editor.load_activity().len(), 1, "no second load was recorded");
}

#[test]
fn different_paths_load_independently() {
    let directory = TempDir::new("async-independent");
    let first_path = directory.write("first.txt", "first file");
    let second_path = directory.write("second.txt", "second file");
    let mut editor = headless_editor();

    let first = editor.request_open_document(&first_path).expect("admitted");
    let second = editor.request_open_document(&second_path).expect("admitted");

    assert_ne!(first.document, second.document);
    assert_ne!(first.task, second.task, "different files get different tasks");
    assert!(!second.joined);
    assert_eq!(editor.tabs().len(), 2);

    pump_until(&mut editor, |editor| editor.loading_count() == 0);
    assert_eq!(editor.document(first.document).unwrap().text().to_string(), "first file");
    assert_eq!(editor.document(second.document).unwrap().text().to_string(), "second file");
    assert_eq!(editor.load_activity().len(), 2);
}

#[test]
fn path_identity_joins_different_spellings_of_one_file() {
    // The documented policy: canonicalize first, so relative components and
    // (on Windows) case differences resolve to one document - including while
    // the file is still loading.
    let directory = TempDir::new("async-identity");
    let path = directory.write("Ident.txt", "one file");
    let indirect = directory.path().join(".").join("Ident.txt");
    let mut editor = headless_editor();

    let first = editor
        .request_open_document_with(&path, LoadInjection::delayed(Duration::from_millis(200)))
        .expect("admitted");
    let second = editor.request_open_document(&indirect).expect("joins");
    assert_eq!(first.document, second.document);
    assert_eq!(first.task, second.task);

    if cfg!(windows) {
        let shouty = directory.path().join("IDENT.TXT");
        let third = editor.request_open_document(&shouty).expect("joins");
        assert_eq!(third.document, first.document, "case-insensitive identity");
    }

    pump_until(&mut editor, |editor| editor.loading_count() == 0);
    assert_eq!(editor.tabs().len(), 1);
}

// --- backpressure ------------------------------------------------------------

#[test]
fn queue_rejection_surfaces_as_a_typed_error_and_creates_no_tab() {
    // Amendment section 3.5.1: a full queue refuses the submission rather than
    // dropping work. The editor must pass that through, not swallow it.
    let directory = TempDir::new("async-rejection");
    let mut editor = EditorCore::with_clipboard_and_scheduler(
        EffectiveConfig::default(),
        Box::new(MemoryClipboard::new()),
        SchedulerConfig { workers: 1, queue_capacity: 1, ..SchedulerConfig::default() },
    );

    // Occupy the worker, then fill the single queue slot.
    let blocker = directory.write("blocker.txt", "x");
    editor
        .request_open_document_with(&blocker, LoadInjection::delayed(Duration::from_secs(30)))
        .expect("admitted");

    let mut rejected = None;
    for index in 0..64 {
        let path = directory.write(&format!("flood-{index}.txt"), "y");
        match editor.request_open_document(&path) {
            Ok(_) => continue,
            Err(error) => {
                rejected = Some(error);
                break;
            }
        }
    }

    let error = rejected.expect("a full queue eventually refuses a submission");
    assert!(matches!(error, OpenDocumentError::Rejected { .. }), "{error}");
    assert!(error.to_string().contains("queue is full"), "{error}");
    assert_eq!(
        ls_log::diag::LsError::code(&error),
        "document.open_rejected",
        "the rejection is typed, not a generic I/O error"
    );

    let tabs_before = editor.tabs().len();
    assert!(
        editor.tabs().iter().all(|id| !editor
            .pending_load(*id)
            .map(|pending| pending.path.file_name().starts_with("flood-63"))
            .unwrap_or(false)),
        "a rejected request leaves no tab"
    );
    assert_eq!(editor.tabs().len(), tabs_before);

    editor.cancel_all_loads();
    pump_until(&mut editor, |editor| editor.loading_count() == 0);
    assert!(editor.tabs().is_empty());
}

// --- accounting ---------------------------------------------------------------

#[test]
fn loads_are_accounted_under_the_document_io_subsystem() {
    let directory = TempDir::new("async-accounting");
    let path = directory.write("counted.txt", "some bytes to read\n".repeat(64));
    let mut editor = headless_editor();

    let request = editor.request_open_document(&path).expect("admitted");
    pump_until(&mut editor, |editor| !editor.is_loading(request.document));

    let record = editor
        .scheduler()
        .recent_records()
        .into_iter()
        .find(|record| record.task_id == request.task.unwrap())
        .expect("the scheduler kept an accounting record");

    assert_eq!(record.subsystem, ls_scheduler::SubsystemId::DOCUMENT_IO);
    assert_eq!(record.outcome, TaskState::Completed);
    assert!(record.bytes_read > 0, "the load reported what it read");
    assert!(record.estimated_cost.bytes > 0, "the submitter estimated the cost");
    assert!(record.workspace.is_some(), "the task is attributed to a workspace");
    if cfg!(windows) {
        assert!(record.cpu_time.is_some(), "per-task CPU time is measured on Windows");
    }

    let activity = editor.load_activity().recent().next().unwrap();
    assert!(activity.queue_wait.is_some());
    assert!(activity.wall_time.is_some());
}

// --- the interactive guarantee -------------------------------------------------

#[test]
fn editing_stays_responsive_while_a_large_file_loads() {
    // The product-level claim: a big load must not block interaction. The
    // editor keeps typing into a scratch document while the read runs.
    let directory = TempDir::new("async-responsive");
    let large = directory.write("large.txt", "a line of source code here\n".repeat(400_000));
    let mut editor = headless_editor();

    let scratch = editor.new_document();
    let request = editor.request_open_document(&large).expect("admitted");
    editor.set_active(scratch).unwrap();

    let mut worst = Duration::ZERO;
    let mut keystrokes = 0u32;
    while editor.is_loading(request.document) {
        let started = Instant::now();
        editor.type_text("x");
        worst = worst.max(started.elapsed());
        keystrokes += 1;
        editor.pump_completions();
        assert!(keystrokes < 5_000_000, "the load never finished");
    }

    assert!(keystrokes > 0, "the load finished before a single keystroke: use a bigger file");
    assert!(
        worst < Duration::from_millis(5),
        "worst keystroke during a large load was {worst:?}, past the 5 ms failure threshold"
    );
    assert_eq!(
        editor.document(scratch).unwrap().text().len_chars(),
        keystrokes as usize,
        "every keystroke landed"
    );
}
