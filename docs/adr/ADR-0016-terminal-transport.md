# ADR-0016: Terminal transport

**Status:** Accepted (interim), PTY deferred
**Date:** 2026-09-01

## Question

What should the embedded command-runner panel (item 10) actually promise:
a full terminal, or something narrower? And if narrower now, what is the
target architecture it should grow into?

## Context

The panel that shipped spawns the platform shell with piped stdio
(`Stdio::piped()` on stdin/stdout/stderr), reads the child's output on a
dedicated thread, strips ANSI/VT100 escape sequences for readability, and
displays the result as a scrolling text log. It works for exactly the
commands a developer runs most:

```text
cargo build
cargo test
python script.py
git status
```

It does not work for anything that expects an actual terminal:

```text
vim, top, htop           -- draw a full screen and expect it drawn back
ssh                      -- often negotiates a pty itself
interactive bash/pwsh    -- prompts, job control, line editing all assume a tty
```

These fail immediately and visibly: with no pseudo-console, the child's
`isatty()` check reports false, so a well-behaved program either refuses to
start interactively or falls back to dumb output that this panel cannot
render meaningfully either way (there is no VT100 interpreter here -- escape
sequences are stripped, not acted on).

## Alternatives

**1. Plain OS pipes (what shipped).** `Stdio::piped()`, three file descriptors,
no pseudo-terminal. Cheap, portable, already implemented. The child process
sees a pipe, not a terminal: no `isatty()`, no terminal size, no line
discipline, no job control signals. Every full-screen or interactive program
either detects this and degrades (many do print a warning and exit) or
behaves as if piped to a file.

**2. A real PTY, mediated by the platform.** On Windows this is ConPTY
(`CreatePseudoConsole`, available since Windows 10 1809); on Linux/macOS it is
`posix_openpt`/`forkpty`. The child gets a real console handle: `isatty()`
succeeds, cursor positioning and screen redraws come through as VT sequences
the client renders (or at minimum forwards), terminal resize is a real event,
job control and signals (`Ctrl+C` reaching the right process group) work as
users expect.

**3. A PTY plus a full VT100/xterm emulator.** Alternative 2 gets the *process*
side right; a real terminal *display* also needs a 2D screen buffer that
interprets cursor movement, scrolling regions, alternate screen buffers (what
`vim` and `htop` use), and color/attribute state -- not just strip escape
codes but execute them. This is what `vte`, `alacritty_terminal`, or a
hand-rolled state machine would provide.

## Decision

**Interim (Stage 1.2, this milestone): keep plain pipes.** The panel is
explicitly a *command runner*, not a terminal, and is documented as such in
[`app/src/terminal.rs`](../../app/src/terminal.rs)'s module doc comment. It is
useful today and costs nothing further to keep.

**Target architecture: ConPTY on Windows (alternative 2), matched by a real
PTY on other platforms when LightSpeed is not Windows-only.** Windows-first
makes ConPTY the concrete next step rather than an abstract "some PTY
eventually": the `windows-sys` crate this project already depends on
(`crates/platform/src/dialog.rs`) exposes the Win32 pseudo-console API
directly, so no new low-level dependency is needed, only new code:

```text
CreatePseudoConsole              -> a real console handle for the child
spawn the child bound to it      -> isatty() succeeds, job control works
read/write the PTY's pipes       -> same reader-thread shape this ADR keeps
```

**Alternative 3 (a screen-buffer VT100 emulator) is deferred past ConPTY, not
decided against.** A PTY without an emulator upgrades "which programs will
even start interactively" but not "will their screen render correctly" --
`vim` would run instead of refusing to, but its redraws still would not paint
into anything today's plain scrolling log understands. Whether to hand-roll a
minimal screen buffer or pull in `vte`/`alacritty_terminal` is a separate
decision, made after ConPTY lands and it is clear how much of a real emulator
the panel actually needs.

## Why not build the PTY now

The instruction that produced this ADR was explicit: decide the architecture,
do not rush the implementation. Two reasons that holds:

1. **Scope.** ConPTY plus a screen buffer is comparable in size to everything
   built for items 5-11 combined in the prior milestone. Rushing it risks the
   same failure mode this whole project's discipline exists to prevent:
   something that looks finished and is not verified.
2. **Sequencing.** The pipe-based panel already answers "can I run `cargo
   test` from inside the editor" today. ConPTY is worth doing *carefully*,
   with its own inspection-first pass, its own tests, and its own review gate
   -- the same treatment every other subsystem in this codebase got.

## Consequences

* `app/src/terminal.rs`'s doc comment already states the pipe limitation; this
  ADR is the place that decision is tracked and the trigger for revisiting it.
* No new dependency is added now. `windows-sys` already provides what ConPTY
  will need when that work starts.
* The panel must keep advertising itself as a command runner, not a terminal,
  in the View menu and its own UI text, until this ADR's target lands.

## Reconsideration criteria

* A user workflow that genuinely needs `vim`/`ssh`/interactive shells inside
  the editor, rather than accepting an external terminal for those.
* Once ConPTY lands, whether the resulting "programs start correctly but may
  render oddly" state is good enough, or whether a screen-buffer emulator
  (alternative 3) is warranted next.
