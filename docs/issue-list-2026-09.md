# LightSpeed IDE — Issue & Feature Tracker (2026-09)

Generated after a large batch of fixes/features. Nothing in this file is committed to git yet — see the session notes for what's in the working tree.

## Done this session (verified: build + full test suite + clippy green, most visually confirmed in a running instance)

- **Terminal `cls`/`clear`** — was hitting a real PowerShell error (no console attached to a piped shell); now handled locally.
- **Terminal history across sessions** — Up-arrow now recalls commands from every past session (permanent transcript) plus, on Windows, PSReadLine's own history.
- **Dedicated task shell** — Run/Install/Uninstall never touch the user's own terminal anymore; they use a separate background shell.
- **15 languages supported** for one-click install (was 8): added TypeScript, Ruby, PHP, Perl, Zig, Lua, PowerShell scripts, each with a verified real winget package id.
- **Git graph shows real branch/merge topology** — was a single straight line; now computes actual lanes from `%P` parent hashes.
- **Git graph text overlap** — dots were drawn using a mismatched unit from the text margin; fixed by reserving exact icon-width slots.
- **Title bar Play button hover highlight** — was entirely untracked; now has one, same as every other icon.
- **Search match highlighting** — was a color change on the matched text; now a real background box, computed from the actual shaped glyph positions (same mechanism as text selection).
- **Terminal text selection, copy, and Ctrl+Left/Right word-jump** — none of this existed; click-drag now selects real text (multi-line, mid-word), Ctrl+C copies it, and the input line supports word-wise cursor movement.
- **Uninstall an installed language with one click** — "Uninstall" row appears under any installed toolchain; runs in the background task shell, and the Run panel's status quietly catches up ~6s later with no manual refresh needed.
- **Dependency graph edge labels** — edges were unlabeled lines; each now carries the actual import/reference text (`models.User`, `./widget`, etc.) stamped along the line, using the same collision-aware grid as node labels.
- **LSP: real request/response tracking (first time)** + **Go to Definition** — Ctrl+Click in the editor asks the active file's language server where a symbol is declared and jumps there. This is the first piece of the eventual real LSP client (previously notification-only, diagnostics only).
- **Markdown preview toggle (Ctrl+Shift+V)** — renders headings, bold/italic, inline/fenced code, lists, and links into a readable, colored form in place of the raw source. **Scoped honestly**: no variable font sizes (the editor's whole rendering pipeline is one fixed-height grid — see "Not started" below), no images, no tables.

## Confirmed already working (no changes needed — investigated because reported as broken)

- **Menu dropdown hover highlight** — the mechanism is correct; a screenshot at the wrong instant made it look absent. Confirmed with a live, moving-cursor test.

## Not started — real scope, needs its own pass

1. **PDF viewing** — genuinely out of scope for a quick addition. This project has stayed dependency-free for everything it reasonably can (`regex_lite.rs`, hand-rolled `gitignore.rs`, its own JSON parser), but PDF rendering is not "reasonably can" — it needs either a real Rust PDF-rendering crate (rasterizing pages to an image, which this renderer can then draw as a quad) or a native library like `pdfium` (a bundled DLL). **This is a decision worth making deliberately with you** — which crate/approach, and whether a native dependency is acceptable — rather than picked silently.
2. **Markdown preview with real typography** — the current preview (this session) proves the toggle and the parsing; genuine heading sizes, image rendering, and tables would need the editor's rendering pipeline to support variable line heights, which today it deliberately doesn't (one fixed grid, for speed and simplicity everywhere else). A real architecture call, not a quick patch.
3. **Split-pane editing** — the tab row's Split icon is currently inert on purpose (see "Known, intentional gaps" below); building it means a second independent viewport, cursor, and scroll state.
4. **LSP: everything past Go to Definition** — Find References, Hover, Completion, Rename, Code Actions. The request/response plumbing this session added makes all of these easier, but each still needs its own request type, response parser, and UI surface.
5. **ConPTY-backed terminal** — still plain piped stdio (see `docs/adr/ADR-0016-terminal-transport.md`); full-screen programs (an editor, `htop`) still print garbage.
6. **Full-scale workspace search performance validation** — only measured at 100–5,000 files; a 100k-file repository is unverified.
7. **Multi-cursor editing** — `SelectionSet` is hardcoded to exactly one selection.
8. **Light theme** — only `Theme::dark()` exists; no light variant or a setting to choose one.
9. **Code folding** — no buffer-line-to-screen-line mapping exists to collapse a block.

## Known, intentional gaps (not bugs — a deliberate "be honest rather than fake it" call)

- **Extensions activity icon** — renders dimmed, does nothing. There's no extension/marketplace subsystem to open.
- **Tab row's Split icon** — renders dimmed, does nothing. See "Split-pane editing" above.

## Process note

Nothing above has been committed to git — everything is sitting in the working tree, exactly as requested ("do not commit yourself"). `main` still matches `origin/main`.
