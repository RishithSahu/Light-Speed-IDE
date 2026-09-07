//! Architecture tests (specification sections 42, 55).
//!
//! The specification requires CI to enforce a set of structural invariants, not
//! just behaviour. Some are checked by reading the source (a rule about what
//! code may exist), the rest by exercising the API (a rule about what the code
//! must do).
//!
//! These are cheap to run and expensive to violate, which is the point: an
//! architecture that is only described in a document erodes.

use ls_core::{CommandArgs, EffectiveConfig, LineIndex, Viewport};
use ls_scheduler::SubsystemId;
use ls_tests::{headless_editor, source_files, source_without_tests, workspace_root, TempDir};

/// Crates that ship as part of the editor, in dependency order.
const LIBRARY_ROOTS: &[&str] = &[
    "crates/log/src",
    "crates/perf/src",
    "crates/platform/src",
    "crates/buffer/src",
    "crates/scheduler/src",
    "crates/core/src",
];

/// The only production location permitted to create worker threads
/// (amendment section 3.5).
///
/// This list is literal on purpose: adding an entry is a reviewable change to
/// this file, not an invisible consequence of writing `thread::spawn`
/// somewhere.
const WORKER_CREATION_ALLOW_LIST: &[&str] = &[
    "crates/scheduler/src/worker.rs",
    // A shelled-out command's stdout/stderr pipes block on read, and the
    // scheduler's task model is bounded one-shot work, not a standing pump --
    // there is no task shape for "read forever until the child exits". These
    // reader threads only append bytes to a shared buffer and wake the event
    // loop; every byte is interpreted and every state change applied on the
    // event-loop thread, same as everywhere else (item 10).
    "app/src/terminal.rs",
    // Same reasoning, for a language server's stdout (item 9): reading its
    // JSON-RPC stream blocks for the process's whole lifetime, which has no
    // shape as a scheduler task either.
    "app/src/lsp.rs",
    // And again for the native folder picker. `IFileOpenDialog::Show` blocks
    // until a human answers it, which is not a bounded unit of work the
    // scheduler could admit, estimate or cancel. It ran on the event-loop
    // thread until measurement showed the shell taking *seconds* to put the
    // dialog on screen (populating Quick Access, cloud providers, mapped
    // drives -- none of it this program's work), during which the editor did
    // not repaint at all. The thread here only shows the dialog and hands the
    // chosen path back through the event loop; nothing is interpreted off it.
    "crates/platform/src/dialog.rs",
];

const SHELL_ROOT: &str = "app/src";

// --- Rules checked by reading the source ------------------------------------

#[test]
fn no_subsystem_creates_its_own_workers() {
    // Specification sections 41-42, amendment section 3.5: worker creation
    // belongs to the scheduler and nowhere else. Every other subsystem reaches
    // background execution through admission.
    let forbidden = [
        "thread::spawn",
        "thread::Builder",
        "rayon::",
        "tokio::spawn",
        "tokio::runtime",
        "futures::executor::ThreadPool",
        "std::thread::scope",
    ];
    let mut violations = Vec::new();
    for path in source_files(LIBRARY_ROOTS).iter().chain(source_files(&[SHELL_ROOT]).iter()) {
        if is_allow_listed(path) {
            continue;
        }
        let source = source_without_tests(path);
        for pattern in forbidden {
            if source.contains(pattern) {
                violations.push(format!("{}: {pattern}", path.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "background workers may only be created by the scheduler:\n{}",
        violations.join("\n")
    );
}

#[test]
fn every_child_process_is_spawned_without_a_console_window() {
    // A GUI process on Windows has no console, so spawning a console
    // application through a bare `Command::new` makes Windows allocate one
    // and *show* it. That is not cosmetic: `git status` flashed a black
    // console box on screen on every Source Control refresh, and opening the
    // terminal panel put a separate `cmd.exe` window next to the editor
    // showing the same session the panel was already showing.
    //
    // `ls_platform::command` applies `CREATE_NO_WINDOW` and is the only place
    // allowed to construct a `Command`, so a new spawn site cannot
    // reintroduce the window by writing the obvious thing.
    const HELPER: &str = "crates/platform/src/process.rs";
    let mut violations = Vec::new();
    for path in source_files(LIBRARY_ROOTS).iter().chain(source_files(&[SHELL_ROOT]).iter()) {
        let normalized = path.to_string_lossy().replace('\\', "/");
        if normalized.ends_with(HELPER) {
            continue;
        }
        let source = source_without_tests(path);
        if source.contains("Command::new") {
            violations.push(normalized);
        }
    }
    assert!(
        violations.is_empty(),
        "child processes must be spawned through `ls_platform::command`, \
         which suppresses the console window:\n{}",
        violations.join("\n")
    );
}

/// Normalizes separators so the allow-list reads the same on every platform.
fn is_allow_listed(path: &std::path::Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    WORKER_CREATION_ALLOW_LIST.iter().any(|allowed| normalized.ends_with(allowed))
}

#[test]
fn the_worker_allow_list_is_exactly_the_scheduler() {
    // Two failure modes, both caught here: the list growing silently beyond
    // what is reviewed here, and the list naming a file that no longer
    // creates workers, which would let a real violation hide behind a stale
    // exemption.
    assert_eq!(
        WORKER_CREATION_ALLOW_LIST,
        &[
            "crates/scheduler/src/worker.rs",
            "app/src/terminal.rs",
            "app/src/lsp.rs",
            "crates/platform/src/dialog.rs",
        ],
        "the allow-list may only contain what this test itself reviews"
    );

    for allowed in WORKER_CREATION_ALLOW_LIST {
        let source = source_without_tests(&workspace_root().join(allowed));
        assert!(
            source.contains("thread::Builder"),
            "{allowed}: allow-listed but does not actually create a worker"
        );
    }
}

#[test]
fn the_shell_opens_documents_through_the_scheduler() {
    // Amendment section 7: the application requests a document and gets it back
    // through a completion. The blocking helper still exists for tests and
    // benchmarks, but the shell must never call it - that would put file I/O
    // back on the event-loop thread.
    for file in ["app.rs", "main.rs", "devpanel.rs", "renderer.rs"] {
        let path = workspace_root().join(SHELL_ROOT).join(file);
        let source = source_without_tests(&path);
        assert!(
            !source.contains(".open_document("),
            "{}: the shell must use request_open_document, not the blocking helper",
            path.display()
        );
    }

    let app = source_without_tests(&workspace_root().join(SHELL_ROOT).join("app.rs"));
    assert!(
        app.contains("request_open_document_with"),
        "the shell should open documents through the request API"
    );
    assert!(
        app.contains("pump_completions"),
        "the shell should apply completions on the event-loop thread"
    );
}

#[test]
fn only_the_editor_core_talks_to_the_scheduler() {
    // The dependency exists so documents can be loaded under admission. It is
    // not a general-purpose escape hatch for the shell.
    let shell = std::fs::read_to_string(workspace_root().join("app/Cargo.toml"))
        .expect("the shell manifest should be readable");
    assert!(
        !shell.contains("ls-scheduler"),
        "the shell reaches background work through ls-core, not directly"
    );

    let core = std::fs::read_to_string(workspace_root().join("crates/core/Cargo.toml"))
        .expect("the core manifest should be readable");
    assert!(core.contains("ls-scheduler"), "the editor core submits document loads");
}

#[test]
fn document_loads_run_under_the_document_io_subsystem() {
    // Accounting only means something if every load is attributed the same way
    // (amendment section 6).
    let source = source_without_tests(&workspace_root().join("crates/core/src/editor.rs"));
    assert!(
        source.contains("SubsystemId::DOCUMENT_IO"),
        "document loads must declare their subsystem"
    );
    assert!(source.contains("with_workspace("), "document loads must be attributed to a workspace");
    assert!(source.contains("with_cost("), "document loads must estimate their cost");
}

#[test]
fn the_scheduler_does_not_depend_on_the_editor_core() {
    // Amendment section 3: the scheduler must not know about documents,
    // rendering, Git, filesystem semantics or project state. The dependency
    // direction is what enforces that.
    let manifest = std::fs::read_to_string(workspace_root().join("crates/scheduler/Cargo.toml"))
        .expect("the scheduler manifest should be readable");
    for forbidden in ["ls-core", "ls-buffer", "ls-platform", "winit", "wgpu"] {
        assert!(!manifest.contains(forbidden), "ls-scheduler must not depend on {forbidden}");
    }
}

#[test]
fn the_renderer_cannot_mutate_editor_state() {
    // Specification section 9.3: the renderer owns presentation, never content.
    // It is allowed to name the immutable snapshot types and nothing else.
    let root = workspace_root();
    let forbidden = ["EditorCore", "Document {", "&mut Document", "apply_edit", "EditHistory"];
    for file in ["renderer.rs", "text.rs", "quads.rs", "theme.rs", "layout.rs"] {
        let path = root.join(SHELL_ROOT).join(file);
        let source = source_without_tests(&path);
        for pattern in forbidden {
            assert!(
                !source.contains(pattern),
                "{}: the renderer must not reference {pattern}",
                path.display()
            );
        }
    }
}

#[test]
fn render_snapshots_expose_no_mutation() {
    // Specification section 26: once published, a snapshot is immutable.
    let source = source_without_tests(&workspace_root().join("crates/core/src/render.rs"));
    let snapshot_impl = source
        .split("impl RenderSnapshot")
        .nth(1)
        .expect("RenderSnapshot should have an inherent impl");
    let body = snapshot_impl.split("\n}").next().unwrap_or("");
    assert!(
        !body.contains("&mut self"),
        "RenderSnapshot must not expose mutating methods:\n{body}"
    );
}

#[test]
fn persistence_goes_through_the_atomic_platform_layer() {
    // Specification section 29: the core never writes a file directly.
    let source = source_without_tests(&workspace_root().join("crates/core/src/workspace.rs"));
    assert!(
        source.contains("write_file_atomic"),
        "the workspace must save through the atomic replacement helper"
    );
    for path in source_files(&["crates/core/src"]) {
        let source = source_without_tests(&path);
        for pattern in ["fs::write(", "File::create("] {
            assert!(
                !source.contains(pattern),
                "{}: saving must go through ls_platform::fsops, found {pattern}",
                path.display()
            );
        }
    }
}

#[test]
fn workspace_traversal_is_lazy() {
    // Specification section 31: enumerate_children reads exactly one level.
    let source = source_without_tests(&workspace_root().join("crates/core/src/workspace.rs"));
    assert!(source.contains("fn enumerate_children"));
    for pattern in ["walkdir", "fn walk_recursive", "read_dir_recursive"] {
        assert!(
            !source.contains(pattern),
            "recursive traversal belongs to a scheduled background task, found {pattern}"
        );
    }
}

#[test]
fn the_editor_core_does_not_depend_on_a_gui() {
    // Specification section 8: the core must be usable, and testable, headless.
    let manifest = std::fs::read_to_string(workspace_root().join("crates/core/Cargo.toml"))
        .expect("the core manifest should be readable");
    for forbidden in ["winit", "wgpu", "glyphon", "cosmic-text"] {
        assert!(!manifest.contains(forbidden), "ls-core must not depend on {forbidden}");
    }
}

#[test]
fn library_crates_do_not_print() {
    // Specification section 26: diagnostics go through the logging subsystem.
    // `main.rs` is exempt: a command line tool answers --help on stdout.
    let mut violations = Vec::new();
    for path in source_files(LIBRARY_ROOTS) {
        let source = source_without_tests(&path);
        for pattern in ["println!", "print!", "dbg!"] {
            if source.contains(pattern) {
                violations.push(format!("{}: {pattern}", path.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "library crates must log rather than print:\n{}",
        violations.join("\n")
    );
}

#[test]
fn every_command_has_an_enabled_predicate_and_a_display_name() {
    // Specification section 12: a command is id + display name + predicate +
    // execute, so menus and palettes can be built from the registry alone.
    for command in ls_core::commands::all() {
        assert!(!command.id.is_empty());
        assert!(!command.display_name.is_empty(), "{} has no display name", command.id);
        assert!(command.id.contains('.'), "{} should be namespaced like `edit.undo`", command.id);
    }
}

// --- Rules checked by exercising the API ------------------------------------

#[test]
fn queues_are_bounded() {
    // Specification section 43: every producer/consumer path is bounded, and
    // overflow drops with a counter rather than growing without limit.
    let mut editor = headless_editor();
    editor.new_document();
    for index in 0..(ls_core::events::DEFAULT_CAPACITY * 3) {
        editor.execute("edit.insert_text", CommandArgs::Text(format!("{index}"))).unwrap();
    }
    let drained = editor.drain_events();
    assert!(
        drained.len() <= ls_core::events::DEFAULT_CAPACITY,
        "event queue grew to {} entries",
        drained.len()
    );
    assert!(editor.dropped_events() > 0, "dropped events should be counted, not hidden");
}

#[test]
fn document_revisions_only_move_forward() {
    // Specification section 22: undo is a new revision, never an older one.
    let mut editor = headless_editor();
    let id = editor.new_document();
    let mut revisions = Vec::new();
    editor.execute("edit.insert_text", CommandArgs::Text("hello".into())).unwrap();
    revisions.push(editor.document(id).unwrap().revision().get());
    editor.execute("edit.insert_text", CommandArgs::Text(" world".into())).unwrap();
    revisions.push(editor.document(id).unwrap().revision().get());
    editor.execute("edit.undo", CommandArgs::None).unwrap();
    revisions.push(editor.document(id).unwrap().revision().get());
    editor.execute("edit.redo", CommandArgs::None).unwrap();
    revisions.push(editor.document(id).unwrap().revision().get());

    for pair in revisions.windows(2) {
        assert!(pair[1] > pair[0], "revisions must increase: {revisions:?}");
    }
}

#[test]
fn line_ending_metadata_survives_a_round_trip() {
    // Specification section 18.
    let directory = TempDir::new("architecture-line-endings");
    let path = directory.write("crlf.txt", "alpha\r\nbeta\r\n");
    let mut editor = headless_editor();
    let id = editor.open_document(&path).unwrap();
    editor.execute("edit.insert_text", CommandArgs::Text("x".into())).unwrap();
    editor.save(id).unwrap();

    let saved = directory.read_string("crlf.txt");
    assert!(saved.contains("\r\n"), "CRLF must be preserved: {saved:?}");
    assert!(!saved.contains("\n\n"), "no stray bare newlines: {saved:?}");
}

#[test]
fn snapshots_only_carry_the_visible_region() {
    // Specification section 66: rendering is viewport-based, so a huge document
    // must not produce a huge snapshot.
    let mut editor = headless_editor();
    let directory = TempDir::new("architecture-viewport");
    let path = directory.write("big.txt", "a line of text\n".repeat(200_000));
    let id = editor.open_document(&path).unwrap();

    let viewport = Viewport {
        first_line: LineIndex::new(100_000),
        visible_lines: 50,
        first_column: ls_core::DisplayColumn::ZERO,
        visible_columns: 200,
    };
    let snapshot = editor.render_snapshot(id, viewport).unwrap();
    assert_eq!(snapshot.lines.len(), 50);
    assert_eq!(snapshot.total_lines, 200_001);
    assert_eq!(snapshot.lines[0].index, LineIndex::new(100_000));
}

#[test]
fn the_core_runs_without_a_window() {
    // Specification section 54: the core must be testable headless. This whole
    // suite is the evidence; this test states the requirement explicitly.
    let mut editor = ls_core::EditorCore::new(EffectiveConfig::default());
    let id = editor.new_document();
    editor.execute("edit.insert_text", CommandArgs::Text("headless".into())).unwrap();
    assert_eq!(editor.document(id).unwrap().text().to_string(), "headless");
}

// --- the shell is a window, not an editor -------------------------------------

/// Every source file in the shell.
fn shell_sources() -> Vec<std::path::PathBuf> {
    source_files(&[SHELL_ROOT])
}

#[test]
fn the_shell_never_saves_synchronously() {
    // Amendment sections 9 and 10: a save is a scheduled task with an immutable
    // snapshot. The blocking helpers remain for tests and benchmarks; calling
    // one from the window would put an fsync on the event-loop thread and undo
    // the whole milestone.
    // `save`, `save_as` and `await_save` block until the bytes are on disk.
    // `save_active` and `save_active_as` do not -- they are the shell-facing
    // wrappers that call `request_save` -- so they are not listed here.
    let forbidden = [".await_save(", "core.save(", "core.save_as(", ".save(id)", ".save_as(id"];
    let mut violations = Vec::new();
    for path in shell_sources() {
        let source = source_without_tests(&path);
        for pattern in forbidden {
            if source.contains(pattern) {
                violations.push(format!("{}: {pattern}", path.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "the shell must save through request_save / request_save_as:\n{}",
        violations.join("\n")
    );

    let app = source_without_tests(&workspace_root().join(SHELL_ROOT).join("app.rs"));
    assert!(app.contains("request_save"), "the shell should request saves");

    // The wrappers the shell does use must themselves be asynchronous.
    let core = source_without_tests(&workspace_root().join("crates/core/src/editor.rs"));
    for (wrapper, body) in [("save_active", "request_save"), ("save_active_as", "request_save_as")]
    {
        let start = core.find(&format!("pub fn {wrapper}(")).unwrap_or_else(|| {
            panic!("{wrapper} should exist for the shell to call");
        });
        let region = &core[start..(start + 600).min(core.len())];
        assert!(
            region.contains(body),
            "{wrapper} must go through {body} rather than blocking the caller"
        );
    }
}

#[test]
fn the_shell_reads_documents_only_through_the_snapshot() {
    // Specification section 9: the renderer consumes the published
    // `RenderSnapshot`. If it could reach a `Document` or a `TextBuffer` it
    // could read editor state mid-frame, which is exactly what the snapshot
    // exists to prevent.
    let renderer = source_without_tests(&workspace_root().join(SHELL_ROOT).join("renderer.rs"));
    for forbidden in [".document(", "active_document", "TextBuffer", "EditorCore"] {
        assert!(
            !renderer.contains(forbidden),
            "renderer.rs must not reach past the snapshot, but mentions {forbidden}"
        );
    }
    assert!(renderer.contains("RenderSnapshot"), "the renderer draws from the published snapshot");
}

#[test]
fn the_shell_never_writes_files_itself() {
    // Persistence has one implementation. A convenience `fs::write` in the
    // window would bypass the atomic replace, the encoder and the revision
    // bookkeeping all at once.
    let mut violations = Vec::new();
    for path in shell_sources() {
        let source = source_without_tests(&path);
        for pattern in ["fs::write", "File::create", "OpenOptions"] {
            if source.contains(pattern) {
                violations.push(format!("{}: {pattern}", path.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "only the persistence layer writes documents:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_menu_bar_is_only_a_view_over_the_command_registry() {
    // Specification section 12: a menu item is a command id. If the menu could
    // edit, there would be two implementations of every action and they would
    // drift.
    let menu = source_without_tests(&workspace_root().join(SHELL_ROOT).join("menu.rs"));
    // Item labels legitimately read "Delete" and "Undo", so the rule is about
    // capability, not vocabulary: the menu is handed the core immutably and
    // therefore cannot edit anything even if it wanted to.
    for forbidden in ["&mut EditorCore", "document_mut", "type_text", ".execute("] {
        assert!(
            !menu.contains(forbidden),
            "menu.rs must route through the registry, but mentions {forbidden}"
        );
    }
    assert!(menu.contains("&EditorCore"), "the menu reads the registry through a shared borrow");
    // Every item is a command id, and `menu.rs` proves that against the
    // registry in its own tests. Here we only check the shape.
    assert!(
        menu.contains("pub command: Option<&'static str>"),
        "a menu item is a command id and nothing else"
    );
}

#[test]
fn the_caret_blinks_on_a_deadline_rather_than_a_frame_loop() {
    // ADR-0013: an idle editor must not render. The caret is the only thing on
    // a clock and it asks for a single wakeup, so `Poll` must appear nowhere.
    let mut violations = Vec::new();
    for path in shell_sources() {
        let source = source_without_tests(&path);
        if source.contains("ControlFlow::Poll") {
            violations.push(path.display().to_string());
        }
    }
    assert!(
        violations.is_empty(),
        "a continuous frame loop is forbidden; use WaitUntil:\n{}",
        violations.join("\n")
    );

    let app = source_without_tests(&workspace_root().join(SHELL_ROOT).join("app.rs"));
    assert!(
        app.contains("ControlFlow::WaitUntil") && app.contains("ControlFlow::Wait"),
        "the loop sleeps until the caret's deadline, and fully idles when there is none"
    );
}

// --- saving stays on the scheduler ----------------------------------------------

#[test]
fn document_saves_run_under_the_document_io_subsystem() {
    // Amendment section 5: a save is admitted at the DOCUMENT IO priority like
    // every other document task, so it competes fairly rather than jumping the
    // queue or starving behind rendering.
    let directory = TempDir::new("arch-save-subsystem");
    let path = directory.write("saved.txt", "before\n");

    let mut editor = headless_editor();
    let id = editor.open_document(&path).expect("opened");
    editor.execute("edit.insert_text", CommandArgs::Text("edit ".into())).unwrap();

    let outcome = editor.request_save(id).expect("admitted");
    let task = outcome.task.expect("a save that starts has a task");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while editor.is_saving(id) {
        editor.pump_completions();
        assert!(std::time::Instant::now() < deadline, "timed out waiting for the save");
        std::thread::yield_now();
    }

    let record = editor
        .scheduler()
        .recent_records()
        .into_iter()
        .find(|record| record.task_id == task)
        .expect("the save was accounted for");
    assert_eq!(record.subsystem, SubsystemId::DOCUMENT_IO);
    assert!(record.workspace.is_some(), "a save is attributed to its workspace");
    assert!(record.bytes_written > 0, "a save reports the bytes it wrote");
}

#[test]
fn a_save_carries_a_snapshot_rather_than_a_borrow_of_the_editor() {
    // Amendment sections 9 and 10: the worker owns an immutable copy of exactly
    // the version it is writing. Nothing it touches can be mutated by the
    // interactive thread while the write is in flight.
    let persistence =
        source_without_tests(&workspace_root().join("crates/core/src/persistence.rs"));
    assert!(
        persistence.contains("pub struct SaveSnapshot"),
        "a save is described by an immutable snapshot"
    );
    for forbidden in ["&mut Document", "&mut EditorCore", "document_mut"] {
        assert!(
            !persistence.contains(forbidden),
            "the persistence layer must not be able to mutate a document, but mentions {forbidden}"
        );
    }
}

#[test]
fn only_the_persistence_layer_writes_document_bytes() {
    // One atomic-replace implementation, in one file. Anything else writing a
    // document would skip the temp file, the fsync, or both.
    let allowed = ["persistence.rs", "workspace.rs"];
    let mut violations = Vec::new();
    for path in source_files(&["crates/core/src"]) {
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if allowed.contains(&name.as_str()) {
            continue;
        }
        let source = source_without_tests(&path);
        for pattern in ["fs::write", "File::create"] {
            if source.contains(pattern) {
                violations.push(format!("{}: {pattern}", path.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "document bytes are written in one place only:\n{}",
        violations.join("\n")
    );
}

// --- one geometry, two consumers -------------------------------------------

#[test]
fn interaction_geometry_is_computed_once_and_shared() {
    // The rule the menu and tab defects both came from breaking: a rectangle
    // that is drawn and a rectangle that catches clicks must be the same
    // rectangle. Both come from a pure `geometry()` function, so a font or
    // scale change moves the visual and the hit region together or neither.
    let root = workspace_root().join(SHELL_ROOT);
    let tabs = source_without_tests(&root.join("tabs.rs"));
    let menu = source_without_tests(&root.join("menu.rs"));
    for (name, source) in [("tabs.rs", &tabs), ("menu.rs", &menu)] {
        assert!(source.contains("pub fn geometry("), "{name} must expose one layout computation");
        assert!(source.contains("pub fn hit("), "{name} must resolve clicks against that layout");
    }

    // The renderer is handed geometry; it must not invent its own.
    let renderer = source_without_tests(&root.join("renderer.rs"));
    for forbidden in ["tab_boxes", "TabHitBox", "close_rect"] {
        assert!(
            !renderer.contains(forbidden),
            "renderer.rs must not own hit geometry, but mentions {forbidden}"
        );
    }

    // And the click handler routes through the shared hit functions rather
    // than recomputing rectangles inline.
    let app = source_without_tests(&root.join("app.rs"));
    assert!(app.contains("tabs::hit("), "tab clicks go through the shared hit test");
    assert!(app.contains("menu::hit("), "menu clicks go through the shared hit test");
}

#[test]
fn overlay_surfaces_are_composited_above_the_editor() {
    // There is one quad pass and one text pass. A panel emitted "last" in a
    // single list still lands under every glyph, which is what made an open
    // dropdown transparent. Layers are what fix it, so the layer split has to
    // exist and the draw order has to be base-then-overlay.
    let root = workspace_root().join(SHELL_ROOT);
    let compose = source_without_tests(&root.join("compose.rs"));
    assert!(compose.contains("pub enum Layer"), "composition must be layered");
    assert!(compose.contains("Overlay"), "there must be a layer above the editor");

    let renderer = source_without_tests(&root.join("renderer.rs"));
    let order = ["render_base", "Layer::Base", "render_overlay", "Layer::Overlay"];
    let mut cursor = 0usize;
    for step in order {
        let found = renderer[cursor..]
            .find(step)
            .unwrap_or_else(|| panic!("the frame must draw {step}, in order"));
        cursor += found + step.len();
    }
}

#[test]
fn menu_surfaces_are_opaque() {
    // A translucent dropdown is not a style choice here: it means the text
    // underneath is still visible through it, which is the defect.
    // The binding, not the field declaration. The runtime assertion lives in
    // the shell's own composition tests, which check the alpha channel of the
    // quad that is actually emitted.
    let theme = source_without_tests(&workspace_root().join(SHELL_ROOT).join("theme.rs"));
    for field in ["menu_background", "menu_highlight", "menu_border"] {
        let binding = format!("{field}: Color::");
        let start =
            theme.find(&binding).unwrap_or_else(|| panic!("{field} should be bound to a color"));
        let end = theme[start..].find('\n').map(|offset| start + offset).unwrap_or(theme.len());
        let line = &theme[start..end];
        assert!(
            line.contains("Color::rgb("),
            "{field} must be an opaque rgb color, not rgba: {line}"
        );
    }
}

#[test]
fn the_shell_keeps_no_second_copy_of_a_document() {
    // Interaction fixes are not allowed to grow a parallel UI state model. The
    // shell may remember where a document is scrolled to; it may not remember
    // what it says.
    let mut violations = Vec::new();
    for path in source_files(&[SHELL_ROOT]) {
        let source = source_without_tests(&path);
        for pattern in ["UiDocument", "ShadowBuffer", "WidgetDocument", "UiTabDocument"] {
            if source.contains(pattern) {
                violations.push(format!("{}: {pattern}", path.display()));
            }
        }
        // A `TextBuffer` field in the shell would be exactly that second copy.
        if source.contains(": TextBuffer") || source.contains("Vec<TextBuffer>") {
            violations.push(format!("{}: stores a TextBuffer", path.display()));
        }
    }
    assert!(
        violations.is_empty(),
        "the active document lives in the core and nowhere else:\n{}",
        violations.join("\n")
    );
}

#[test]
fn tab_actions_are_addressed_by_document_id() {
    // Closing or activating "the third tab" is how a click ends up on the
    // wrong document when the tab list changes shape between frames.
    let tabs = source_without_tests(&workspace_root().join(SHELL_ROOT).join("tabs.rs"));
    assert!(
        tabs.contains("Body(DocumentId)") && tabs.contains("Close(DocumentId)"),
        "a tab hit must carry the document it acts on"
    );
    let app = source_without_tests(&workspace_root().join(SHELL_ROOT).join("app.rs"));
    assert!(
        !app.contains("tabs().get(index)"),
        "the shell must not resolve a tab click through a positional index"
    );
}

// --- Resource Center (item 4: admission, accounting, pressure) -------------

#[test]
fn the_resource_center_reads_the_scheduler_only_through_ls_core() {
    // Same boundary as `only_the_editor_core_talks_to_the_scheduler`, checked
    // from the panel's side: it must reach admission and accounting state
    // through re-exported types, never by adding ls-scheduler to the shell's
    // own manifest.
    let shell = std::fs::read_to_string(workspace_root().join("app/Cargo.toml"))
        .expect("the shell manifest should be readable");
    assert!(!shell.contains("ls-scheduler"), "the Resource Center must not add this dependency");

    let resources = source_without_tests(&workspace_root().join(SHELL_ROOT).join("resources.rs"));
    assert!(
        resources.contains("ls_core::") || resources.contains("use ls_core"),
        "the panel reads scheduler state through ls_core's re-exports"
    );
    assert!(!resources.contains("ls_scheduler::"), "never through the scheduler crate directly");
}

#[test]
fn the_scheduler_still_has_no_automatic_retry() {
    // The overload policy is reject, not retry-with-backoff (amendment
    // section 3.5.1), and that is a deliberate, reviewed decision. This pins
    // it down so a future "just add retry" change does not slip in silently
    // as part of an unrelated patch -- it would need to update this test and
    // the amendment together, which is the point.
    let queue = source_without_tests(&workspace_root().join("crates/scheduler/src/queue.rs"));
    let lib = source_without_tests(&workspace_root().join("crates/scheduler/src/lib.rs"));
    for forbidden in ["fn retry", "Backoff", "exponential_backoff"] {
        assert!(!queue.contains(forbidden) && !lib.contains(forbidden), "found {forbidden}");
    }
}
