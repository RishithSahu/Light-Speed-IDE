# Stage 1 milestone report

**Date:** 2026-08-25
**Scope:** application shell, editor core, buffer model, cursor/selection,
file open/save, tabs, basic rendering, performance instrumentation.

Measured on Windows 11, Intel i5-12450HX (12 cores), 16 GB RAM, NVIDIA RTX 3050
Laptop, release build, workload definitions v1.

---

## 1. Architecture

```text
                          LightSpeed
                               |
      +------------------------+------------------------+
      |                        |                        |
 Application               Editor Core               Renderer
 keymap -> commands         Document                 winit + wgpu + glyphon
 window, input, scroll      TextBuffer (rope)        quad pipeline + text
 status bar, overlay        SelectionSet             draws snapshots only
      |                     EditHistory
      |                     encoding / line endings
      |                     Workspace (file I/O)
      |                     CommandRegistry
      |                     EventQueue (bounded)
      +------------------------+------------------------+
                               |
                     Platform abstraction
        clipboard | dialogs | atomic replace | paths | RSS/CPU
```

Dependency direction is one-way — `log <- platform <- core`, `buffer <- core`,
`core <- app` — and enforced: an architecture test fails the build if `ls-core`
ever names winit, wgpu, glyphon or cosmic-text.

The renderer receives an immutable `Arc<RenderSnapshot>` and can reach nothing
else. A second architecture test scans the renderer's source for `EditorCore`,
`Document` and history types and rejects any reference.

## 2. Files created

15,079 lines of Rust across eight crates:

| Crate | Lines | Contents |
| --- | ---: | --- |
| `crates/log` | 485 | levels, structured records, sinks, redaction, error vocabulary |
| `crates/perf` | 602 | latency rings, percentiles, budgets, counters, gauges |
| `crates/platform` | 1,198 | Win32 clipboard and dialogs, atomic replace, path identity, RSS/CPU |
| `crates/buffer` | 1,887 | rope, position types, grapheme and column queries, line endings |
| `crates/core` | 6,044 | documents, revisions, selection, undo, encoding, workspace, commands, events, config, snapshots |
| `app` | 3,025 | window, keymap, layout, quad pipeline, text engine, renderer, shell |
| `benchmarks` | 892 | workload generator, harness, reporting |
| `tests` | 946 | integration, architecture, regression |

Plus eleven ADRs in `docs/adr/`, a README, and benchmark results in
`benchmarks/results/`.

## 3. Core data structures

**`TextBuffer`** (ADR-0001) — B-tree rope. Leaves hold ≤1 KB UTF-8 chunks;
internal nodes hold ≤8 children; every node caches `{bytes, chars, line_breaks}`
for its subtree. Nodes sit behind `Arc` and are edited copy-on-write, so a
snapshot is a pointer clone. Measured depth: 1 at 1 KB, 6 at 100 MB.

**Position types** — `ByteOffset`, `CharOffset`, `LineIndex`, `DisplayColumn`
are distinct types. "1 byte = 1 character" and "1 character = 1 column" are not
expressible without writing a conversion, which is the point.

**`Document`** — buffer, `SelectionSet`, `EditHistory`, encoding, line-ending
policy, language, `content_revision`, and three independent state dimensions:
`ContentState` (clean/dirty), `ExternalState`
(unchanged/changed/missing/conflict), `PersistenceState`
(idle/saving/succeeded/failed).

**`EditHistory`** — operation-based. An `Edit` is `{at, removed, inserted}` and
its own inverse; a `Transaction` groups edits with the selection before and
after. Typing coalesces within a 500 ms window while the caret stays contiguous;
a cursor jump, a paste, a save, an undo or any command boundary closes the group.

**`RenderSnapshot`** (ADR-0007) — immutable, viewport-sized, carries the
`content_revision` it was built from plus the invalidation since the last one.

**Command registry** — every action is `{id, display_name, enabled, execute}`.
The keymap resolves a key to a command id and nothing else; a test asserts every
bound id exists in the registry.

## 4. Measured performance

### Editing latency (target P95 2 ms, failure 5 ms)

| Workload | Type at start | Type in middle | Type at end | Backspace |
| --- | ---: | ---: | ---: | ---: |
| 1 KB | 1.6 µs | 800 ns | 900 ns | 1.0 µs |
| 64 KB | 1.3 µs | 1.2 µs | 1.2 µs | 1.1 µs |
| 1 MB | 1.2 µs | 1.1 µs | 1.2 µs | 1.2 µs |
| 10 MB | 2.0 µs | 2.0 µs | 1.9 µs | 2.4 µs |
| **100 MB** | **1.2 µs** | **1.4 µs** | **1.4 µs** | **1.3 µs** |
| 1 MB Unicode | 2.0 µs | 2.2 µs | 2.3 µs | 3.8 µs |
| 10 MB one line | 1.0 µs | 1.0 µs | 900 ns | 1.1 µs |

Flat across five orders of magnitude, at under 0.1% of budget. Run-to-run
variance matters at this scale: an earlier run taken while the workspace was
compiling measured 2.3-2.4 µs at 100 MB, still three orders of magnitude inside
the contract.

### Other interactive operations (P95)

| Operation | 1 KB | 1 MB | 100 MB | Target |
| --- | ---: | ---: | ---: | ---: |
| Cursor char | 2.5 µs | 1.5 µs | 1.3 µs | 4 ms |
| Cursor word | 2.5 µs | 2.4 µs | 2.1 µs | 4 ms |
| Selection extend | 4.2 µs | 4.4 µs | 4.5 µs | 4 ms |
| Undo | 1.2 µs | 1.2 µs | 1.6 µs | 5 ms |
| Redo | 1.1 µs | 1.0 µs | 1.0 µs | 5 ms |
| Tab switch | 200 ns | 300 ns | 200 ns | 2 ms |
| Snapshot (50 lines) | 41 µs | 87 µs | 72 µs | 8 ms |

### File operations (P95)

| Workload | Open | Save |
| --- | ---: | ---: |
| 1 KB | 345 µs | 2.8 ms |
| 64 KB | 358 µs | 4.2 ms |
| 1 MB | 3.3 ms | 7.7 ms |
| 10 MB | 32.8 ms | 49.3 ms |
| 100 MB | 354 ms | 617 ms |

Small-file open (≤20 ms target) is met with three orders of magnitude to spare.
Large-file open and save block the interactive thread — see limitations.

### Startup (target 500 ms, failure 1 s)

Five cold launches, per-phase:

| Phase | All backends (before) | Native backend first (after) |
| --- | ---: | ---: |
| Window creation | 75-118 ms | 75-118 ms |
| GPU adapter | 510-562 ms | 277-306 ms |
| GPU device | 48-64 ms | 94-223 ms |
| Pipelines | 1-2 ms | 7-23 ms |
| Font system | 36-39 ms | 56-74 ms |
| **Total to usable** | **943-1057 ms** | **542-740 ms** |

Asking for the platform's native backend (DX12) before falling back to a full
backend search removed ~40% of startup. **The 1 s failure threshold is met; the
500 ms target is not.** Remaining time is dominated by GPU adapter enumeration,
which is driver work before any LightSpeed code runs.

### Memory

| Workload | Measured RSS | Target | Failure |
| --- | ---: | ---: | ---: |
| W1 empty editor | **151.6 MB** | 120 MB | 160 MB |
| 5 documents, 10.3 MB source | **164.7 MB** | 180 MB | 250 MB |
| Headless core, no window | 5.4 MB | — | — |

The empty editor **misses its 120 MB target** while staying under the 160 MB
failure threshold. The breakdown explains it: the editor core itself is 5.4 MB;
the remaining ~146 MB is the GPU driver, wgpu and the font system. Opening 10 MB
of source added only 13 MB, so the footprint is baseline, not per-document.

Rope overhead against document size: 0.87x at 64 KB, 1.36x at 1 MB, 1.16x at
10 MB, **1.16x at 100 MB** (116 MB resident for 100 MB of text). The 1 KB
document reports 15.9x because a document smaller than a page measures allocator
granularity rather than the rope.

## 5. Tests

277 tests, all passing:

| Suite | Count | Covers |
| --- | ---: | --- |
| `ls-buffer` | 45 | rope invariants, offsets, graphemes, columns, line endings, large edits |
| `ls-core` | 137 | documents, revisions, selection, undo coalescing, encoding, persistence, commands, events, config, snapshots |
| `ls-platform` | 18 | clipboard round trip, atomic save, failed save, path identity, RSS/CPU |
| `app` | 21 | keymap resolution, layout, sRGB conversion, argument parsing |
| `ls-perf` | 9 | percentiles, ring behaviour, budget status |
| `ls-log` | 5 | timestamps, redaction, truncation, level filtering |
| `ls-bench` | 4 | workload generation determinism |
| integration | 16 | open/edit/save/reopen, identity, tabs, encodings, external changes, 100 MB document |
| architecture | 13 | the section 55 invariants |
| regression | 9 | deterministic replay, coalescing boundaries, cursor stability, chunk-spanning deletes |

Unicode coverage includes ASCII, accents, combining marks, emoji, ZWJ sequences,
regional indicators, CJK width and RTL text. The core is fully testable without
opening a window; every suite above runs headless.

## 6. Known limitations

**Contracts not yet met**

1. *Startup 542-740 ms against a 500 ms target.* Dominated by GPU adapter
   enumeration (~285 ms) and device creation. The fix is to show the window and
   first text before the GPU is ready, or to cache adapter selection across
   launches; both are shell changes, not architectural ones.
2. *Empty-editor RSS 151.6 MB against a 120 MB target.* The core is 5.4 MB of
   it. Reducing the rest means either accepting the GPU driver's footprint or
   adding a software rendering path.

**Deliberately absent (Foundation Stage)**

Syntax highlighting, workspace search, Git, terminal, language services, the
filesystem watcher, the file tree and the task scheduler. Their contracts are in
the specification; this stage does not pretend to implement them. External
change *detection* exists and is tested (`refresh_external_state`), but nothing
polls it yet — that needs the watcher.

**Gaps within Stage 1 scope**

* **No IME support.** Composition events are not handled, so CJK input methods
  cannot type into the editor (paste works). This is the largest shell gap.
* **Large file open and save block the frame loop** (354 ms / 617 ms at 100 MB).
  This is the direct consequence of having no scheduler (ADR-0003) and is the
  first thing admission will fix.
* **Multi-cursor is absent by instruction**, but `SelectionSet` already has the
  shape for it.
* **A very large paste is `O(n log n)`**, not `O(n)`: a paste is chunked and
  inserted piecewise rather than spliced as a subtree.
* **No caret blink**, deliberately: a blinking caret means waking the frame loop
  twice a second forever, against a 2% idle CPU contract.
* **Encodings beyond UTF-8, UTF-8 BOM and UTF-16 LE/BE are refused**, not
  guessed.
* **Windows only.** The platform layer is the boundary; macOS and Linux need
  clipboard and dialog implementations behind the existing trait.

## 7. Where the time goes

The point of this stage was to be able to answer that question. Every
interactive operation records into a named metric with a declared budget; F12
shows the live distribution; the benchmark harness reports P50/P95/P99/max and
RSS per workload and compares against the contract; startup is broken into five
phases.

The startup work above is the demonstration: a 1,057 ms launch was not a mystery
to be guessed at, it was 562 ms of adapter enumeration, and the number moved
because the measurement pointed at it.

## 8. What happens next

The two largest findings in this report - 100 MB open and save blocking the
interactive thread, and the missing IME - are addressed by the next milestone,
Stage 1.1 (Async Interactive Core). Its contracts are in
`docs/foundation-amendment-001.md`, its sequence and the evidence behind that
sequence are in `docs/prioritization-001.md`, and the four decisions it rests on
are ADR-0012 through ADR-0015. Stage 1's Definition of Done is not reopened:
Stage 1.1 extends it and must keep it passing.
