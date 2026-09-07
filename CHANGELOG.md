# Changelog

Notable changes to LightSpeed IDE, in the format of
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

`Cargo.toml` has read `0.1.0` throughout this work and no tags exist, so the
version numbers below are not build-tagged releases — they are this
changelog's own way of splitting one long stretch of uncommitted work into
three reviewable stages, oldest first. Nothing under 0.0.2 or 0.0.3 has been
committed.

Numbers quoted here were measured on this repository (79 files, 156 import
edges) unless stated otherwise, not estimated.

---

## [0.0.3] — Interactive dependency graph, settings screen

### Added

**Dependency view rewrite: from a static picture to a live canvas.**

- Force-directed layout drawn as a round cluster that fits the pane whole
  (`app/src/depgraph.rs`), replacing the layered layout from 0.0.2. Circles
  are sized by how connected a file is, coloured by whether anything imports
  it, and joined by arrowheaded edges.
- Pan by dragging, zoom on the wheel anchored under the pointer, double-click
  empty canvas to refit.
- Click a node to open that file. The graph steps aside so the file is
  visible, keeping its pan and zoom for when you come back.
- Hover to trace: the node takes a ring, its edges are drawn brighter and
  thicker, and the status bar names the file with its import counts.
- Per-folder caching (`crates/platform/src/depgraph_cache.rs`). The settled
  graph is written to `%APPDATA%\LightSpeed\graphs\` (or `$XDG_CACHE_HOME`)
  and read back on later visits: first open costs a 62 ms scan plus a 294 ms
  simulation, later opens 216 µs to read plus 580 µs to fit — roughly **400×
  faster**, from a 4.4 KB file. `Ctrl+Shift+R` rescans.

**Settings screen** — `Ctrl+,` or the gear in the title bar.

- A searchable, categorised screen with a control per setting: checkboxes,
  text and number fields, and option pills for fixed choices
  (`app/src/settings_ui.rs`).
- One descriptor table drives the screen, the search, the file format and the
  validation, so adding a setting is a single row
  (`crates/core/src/settings.rs`). 18 settings across Text Editor, Workbench,
  Terminal, Dependency View and Performance.
- Every value is bounded by its own declared kind: numbers clamp into range,
  choices refuse anything off the list, text is cut to length. A settings file
  edited into nonsense costs only the lines that are wrong.
- Two layered files (`crates/platform/src/settings_file.rs`): a per-user one
  that follows you between projects, and `.lightspeed/settings.conf` inside a
  workspace that a team can commit. Only what differs from a default is
  written.
- Font size, font family and line height apply live, without a restart.
- Settings that cannot apply until restart say so on their own row rather than
  pretending otherwise.

### Changed

- **Dependency graph layout: layered → force-directed.** A layered (Sugiyama)
  layout is the canonical choice for a dependency graph and was the wrong one
  here: a codebase is shallow and wide, so this repository laid out at
  **12218×210 px** — eleven screens across, with about seven of seventy-nine
  files legible at once. A force layout has no ranks to spread along and packs
  the same graph into one cluster that fits the window. `layout-rs` (added in
  0.0.2) was dropped for `force_graph` accordingly.
- **Rust import extraction now reads `use`, not only `mod`.** `mod` appears
  solely in a crate root, so the 0.0.2 graph was a bare star out of
  `main.rs` — the module *declaration* tree, not which file takes which as
  input. Edges on this repository went from **61 to 156**.
- The wheel zooms in the dependency view rather than scrolling it; the picture
  is already fitted to the pane, so there is nothing above or below to reach.
- `EditorCore` keeps a live copy of the document settings, so tab width and
  Insert Spaces can move under an open file. The configuration loaded at
  startup remains a snapshot of what the build booted with.

### Fixed

- **Graph labels shivered while dragging.** Labels are quantised to a
  character cell, and quantising against the pane meant each name crossed its
  cell boundary at a different sub-pixel moment, so nothing could be read
  while the graph moved. The grid is now pinned to the graph: the pan drops
  out of the rounding entirely and every label travels with its circle.
- **The force simulation diverged.** `force_graph`'s default `node_speed` of
  7000 is meant for animating a handful of nodes a frame at a time; with
  eighty nodes pushing at once a single step moved them thousands of pixels.
  Settled positions spanned two million pixels, the graph collapsed into a dot
  when fitted, and 22 of 156 edges survived.
- **Crowded graphs lost their edges.** Circles were a fixed size while
  positions were squeezed to fit the pane, so the connected nodes a force
  layout deliberately pulls *together* overlapped and no line was drawn
  between them — 88% of this repository's edges went missing. Circles are now
  sized from the spacing actually achieved.
- **Dragging the graph opened a file.** The drag handler advanced its own
  anchor every frame, so on release the pointer had always "just" moved zero
  pixels and every drag ended by opening whatever it started on.
- **A hard drag lost the graph off screen.** The pan clamp guarded the pane
  the scene was fitted to, but a small graph occupies a patch in the middle of
  it. The middle of the circles is now what is kept on screen.
- **Clicking the settings screen could crash the application.** The screen is
  measured on one frame and clicked on the next, and a search typed in between
  leaves a shorter list; the hit test indexed it.
- `Ctrl+,` did nothing whenever the explorer or the terminal had the
  keyboard — which is most of the time somebody wants it. It is now global,
  the same standing the command palette's shortcut has.
- Labels sized to their circle rendered every filename alike — even a hub is
  only about eleven characters across, so `workspace_search.rs` read as
  `workspac…`.

### Internal

- Diagnostic, `#[ignore]`d benchmarks reporting what this checkout actually
  measures, for the dependency scan, the simulation and the cache.

---

## [0.0.2] — Terminal overhaul, command palette, language support, first dependency graph

### Added

**Dependency view (first pass)** — a new sixth icon on the activity bar (also
`Ctrl+Shift+D`, or *View ▸ Toggle Dependency View*). Scans the workspace for
which file imports which and draws the result.

- Import scanning for Rust, Python, JavaScript, TypeScript, C, C++ and C#
  (`crates/core/src/dependency_graph.rs`). Rust read `mod` declarations only
  at this point (see 0.0.3 for why that changed); C-family reads
  `#include "…"` and deliberately not `<…>`; Go is skipped, because resolving
  its packages needs `go.mod`.
- Layered (Sugiyama) graph layout via the `layout-rs` crate.

**Terminal**

- Scrollback that actually scrolls, sized from the panel's height rather than
  a fixed twelve lines, holding position when scrolled back and snapping to
  the tail on Enter.
- Command history on `↑`/`↓`.
- A permanent transcript of every command and its output, appended across
  sessions (`crates/platform/src/terminal_log.rs`).
- A visible cursor, with `←`/`→`, `Home`, `End`, `Delete` and `Backspace`.
- Shell selection across `pwsh` / `powershell` / `cmd` on Windows and
  `bash` / `sh` elsewhere, with Git's GNU tools appended to `PATH` so common
  Unix commands work. Appended, never prepended, so Windows' own `find` and
  `sort` are not shadowed.

**Command palette** (`app/src/palette.rs`) — the floating, fuzzy-filtered
command list opened from the title bar's command field or `Ctrl+Shift+P`.
Filtering is a pure function of the registry and the query.

**Language support** — `Language::ALL` now covers plain text, Rust, Python, C,
C++, C#, Go, JavaScript, TypeScript, JSON, TOML, YAML, Markdown and Shell,
with detection, highlighting and workspace search exercised per language.

**LSP** — server specification per language, and a manager that starts,
retires and drains diagnostics from more than one server (`app/src/lsp.rs`).

**Other**

- `lightspeed .` opens a directory as the workspace. It previously reported
  "… is a directory" and started on an empty buffer with no workspace at all.
- An asynchronous folder picker, so choosing a folder no longer blocks the
  event loop (`crates/platform/src/dialog.rs`).
- Ellipse and rotated-line primitives in the quad renderer, feathered with an
  SDF in the fragment shader (`app/src/quads.rs`) — laid down for the
  dependency graph above.
- `ls_platform::process::command`, the single place a child process is
  spawned, applying `CREATE_NO_WINDOW`.

### Changed

- Terminal shells are started with `-NoLogo` but **not** `-NoProfile`.
  Measured: pwsh with its profile 370 ms, `-NoProfile` 383 ms. It bought
  nothing and cost the user their own aliases.

### Fixed

- The dependency view crashed on an empty workspace: `layout-rs` panics
  outright on a graph with no nodes.
- "Cannot build the dependency graph: the scheduler is shutting down" was
  reported when the real reason was that no folder was open.
- **Child processes opened console windows on Windows.** Release builds use
  `windows_subsystem = "windows"` and so have no console to inherit, and every
  bare `Command::new` allocated a visible one.
- Block comments in the highlighter: a blank line inside one, a line holding
  only the terminator, and a multi-line comment with a gap in it.

### Internal

- Architecture test `every_child_process_is_spawned_without_a_console_window`,
  verified to fail when violated.
- `tests/tests/languages.rs`: per-language coverage in two tiers — hermetic
  fixtures in the ordinary suite, and `#[ignore]`d checks against real cloned
  repositories, kept out of the default run so `cargo test` stays fast and
  offline.
- Workspace search gained an ASCII fast path and a bounded file-reading cache
  that releases oversized buffers.

---

## [0.0.1] — Initial commits

Commits already on `main` before this work began, newest first.

- Merge pull request #26 — file icons by extension (fixes #18).
- Merge pull request #23 — open a file from the in-app explorer (fixes #10).
- Folder picker moved off the legacy dialog (fixes #8); resizable sidebar
  width (fixes #6).
- `version 0.0.1` — initial commits.
