# LightSpeed IDE

A native code editor whose engineering property is that **interactive editing
stays responsive and predictable while everything else is happening**.

Stage 1 is the editor core and the application shell. It is deliberately narrow:
no syntax highlighting, no search, no Git, no terminal, no language services, no
background indexing. Those are the Foundation Stage, and they will be built on
the contracts this stage establishes rather than beside them.

**Stage 1 is complete and measured.** Stage 1.1 — Async Interactive Core — is
specified in [Foundation Amendment 001](docs/foundation-amendment-001.md) and
sequenced in [Prioritization Memo 001](docs/prioritization-001.md). It is in
progress: the scheduler (item 1 of 7) is implemented.

```text
                        LightSpeed
                             |
        +--------------------+--------------------+
        |                    |                    |
   Application           Editor Core           Renderer
   commands, keys        documents             winit + wgpu
   window, input         text buffer           glyphon text
        |                cursor/selection      immutable snapshots
        |                undo history
        |                encoding/line endings
        +--------------------+--------------------+
                             |
                     Platform abstraction
              clipboard, dialogs, atomic replace,
                path semantics, process stats
```

## Building and running

```bash
cargo run --release -p lightspeed -- path/to/file.rs
```

```text
Ctrl+N / Ctrl+O / Ctrl+S    new, open, save        Ctrl+Shift+S  save as
Ctrl+Z / Ctrl+Y             undo, redo             Ctrl+W        close tab
Ctrl+C / Ctrl+X / Ctrl+V    copy, cut, paste       Ctrl+A        select all
Ctrl+Tab                    next tab               F12           performance overlay
Esc                         cancel a load          F9            loading panel
Arrows, Home/End, PageUp/Down, Ctrl+arrows for word movement; Shift extends.

The loading panel (F9) shows what the scheduler is doing: the current task, its
timings, which duplicate requests joined which load, and a heartbeat that keeps
ticking while a large file is read. `F5` issues several requests for one path at
once, `F6` injects a slow load and `F7` a failing one, so each state is
reachable without hunting for a 100 MB file.
```

`LIGHTSPEED_LOG=debug` raises the log level; `LIGHTSPEED_LOG_FILE=path` also
writes to a file.

## Repository layout

```text
crates/log        structured logging, shared error vocabulary
crates/perf       latency metrics, budgets, counters
crates/platform   Windows clipboard, dialogs, atomic replace, paths, RSS/CPU
crates/scheduler  task admission, priority, fairness, cancellation, accounting
crates/buffer     TextBuffer (rope), offsets, grapheme and column queries
crates/core       documents, revisions, selection, undo, encoding, snapshots
app               window, input, command routing, GPU renderer
benchmarks        workload definitions and the measurement harness
tests             integration, architecture and regression suites
docs/adr          architecture decision records
```

The dependency graph runs one way: `log <- platform <- core`,
`buffer <- core`, `scheduler <- core`, `core <- app`. The editor core has no GUI
dependency, and the scheduler knows nothing about documents; architecture tests
enforce both.

## Testing

```bash
cargo test --workspace
```

333 test functions (plus one documentation test): unit tests per crate, and
cross-crate suites in `tests/`:

* **integration** — open, edit, save, reopen; tabs; encodings; external changes;
  a 100 MB document;
* **architecture** — the invariants section 55 of the specification requires CI
  to enforce (no ad-hoc threads, renderer cannot mutate editor state, snapshots
  are immutable, queues are bounded, persistence goes through the atomic layer,
  traversal is lazy, revisions only move forward);
* **regression** — behaviour with a reason attached, including deterministic
  replay of an edit script.

## Benchmarking

```bash
cargo run --release -p ls-bench -- --json benchmarks/results/full.json
cargo run --release -p ls-bench -- --quick        # skip the 100 MB workloads
```

Workloads run from 1 KB to 100 MB, plus a Unicode-heavy document and a 10 MB
single-line file. Every scenario reports P50/P95/P99/max and RSS, and compares
against the declared budget. Results live in `benchmarks/results/`.

Headline: typing latency P95 is **1.4 µs on a 100 MB document** against a 2 ms
target, and flat across five orders of magnitude of document size. Opening that
same 100 MB file no longer blocks interaction: the A1 workload types **279,992
keystrokes at P95 1.1 µs while the load runs**.

## Performance contracts

Contracts are a target and a failure threshold, never a single number
(specification section 48). Current status is in
[`docs/milestone-1-report.md`](docs/milestone-1-report.md), including the two
contracts that are not yet met and why.

## Documentation

* [`docs/foundation-spec.md`](docs/foundation-spec.md) — the baseline
  engineering specification this implementation answers to
* [`docs/foundation-amendment-001.md`](docs/foundation-amendment-001.md) —
  Amendment 001, Async Interactive Core; supersedes the baseline where they
  conflict
* [`docs/prioritization-001.md`](docs/prioritization-001.md) — the implementation
  sequence for Stage 1.1 and the evidence behind it
* [`docs/milestone-1-report.md`](docs/milestone-1-report.md) — what was built,
  measured numbers, known limitations
* [`docs/adr/`](docs/adr/) — fifteen architecture decision records, each with
  alternatives considered, benchmark evidence and reconsideration criteria
