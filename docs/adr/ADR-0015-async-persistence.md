# ADR-0015: Async persistence and revision-aware saves

**Status:** Accepted (Stage 1.1)
**Date:** 2026-08-25
**Amends:** baseline §24, §25, §29, §30 via `docs/foundation-amendment-001.md`
§7-§10

## Question

How can files ≥100 MB be opened and saved without blocking the interactive
thread, while preserving atomicity and revision correctness?

## Context

Stage 1 measurements, release build:

```text
100 MB open   ~354 ms   blocking
100 MB save   ~617 ms   blocking
 10 MB open   ~33 ms    blocking
 10 MB save   ~49 ms    blocking
```

Both run on the interactive thread, which baseline §40 forbids. Every other
interactive operation measured in microseconds, so this is the whole of the
remaining responsiveness debt.

Two things must not be lost while fixing it:

1. **Atomicity.** Baseline §29's sequence — temporary file, write, flush, fsync,
   atomic replace — exists so that an interrupted save can lose the new contents
   but never the old ones. Stage 1 has tests proving the original survives a
   failed write.
2. **Revision correctness.** A save takes time. The document keeps changing
   during it. Finishing a save says something about the revision that was
   written, not about the document as it is now.

## Alternatives

**1. Keep saving synchronous, make it faster.** Streaming already avoids a
second full allocation; the remaining cost is the write itself and the fsync.
There is no version of "write 100 MB and fsync it" that fits in a frame.
Rejected as arithmetic.

**2. Lock the document for the duration of the save.** Correct and simple, and
it converts a 617 ms save into a 617 ms freeze of editing. Rejected: it fixes
the thread and not the user experience.

**3. Copy the document, then save the copy.** Correct, and it doubles peak
memory for a 100 MB file at exactly the moment memory is under pressure.

**4. Immutable persistence snapshot + scheduler + streaming sink.** The rope is
already copy-on-write, so a snapshot is an `Arc` clone: O(1), no copy, and the
document stays editable while its past self is written.

## Decision

**Adopt alternative 4.**

### Open

```rust
fn request_open_document(path: PathBuf) -> TaskId;
```

```text
request
 ↓
scheduler (admission)
 ↓
read / decode / detect encoding / detect line endings
 ↓
construct document
 ↓
publish DocumentLoaded
```

The tab appears immediately in a `Loading` state and can be cancelled. A second
request for a path that is already loading joins the in-flight task rather than
starting a second one, preserving the "one file, one document" identity rule
(baseline §24).

### Save

```text
Document revision N
        ↓
immutable persistence snapshot   (Arc clone of the rope root, O(1))
        ↓
Scheduler (admission)
        ↓
stream encoded chunks
        ↓
temporary file
        ↓
flush
        ↓
fsync
        ↓
atomic replace
        ↓
SaveCompleted(N)
```

The durability sequence is unchanged from baseline §29. Only where it runs
changes.

### Streaming sink

```rust
trait ByteSink {
    fn write(&mut self, bytes: &[u8]) -> Result<(), PersistenceError>;
}
```

```text
Rope leaves (<=1 KB each)
    ↓
encoding transform      UTF-8 | UTF-8 BOM | UTF-16 LE | UTF-16 BE
    ↓
line-ending transform   LF | CRLF | CR
    ↓
ByteSink
    ↓
temporary file
```

**A save must never materialize the document as a second contiguous
allocation.** Peak memory during a save is the document plus a small constant.

Chunk-wise transformation is already proven safe in Stage 1: chunk boundaries
never split a character, and the line-ending transform only rewrites `\n`, so
chunk-by-chunk output is byte-identical to whole-text output.

### Revision-aware completion

```text
revision 40
   ↓ Save pressed
task captures document_id = X, revision = 40, content = snapshot(40)
   ↓ user keeps typing
revision 41
revision 42
   ↓ task completes
SaveCompleted(X, 40)
   ↓ interactive thread compares transaction tokens
captured token != current token
   ↓
ContentState = Dirty
```

A completed save clears dirty state **only** when the transaction token captured
when the save started is still the document's current token. The disk stamp is
recorded regardless, because the file on disk really does now hold revision 40 —
external-change detection needs that fact so it does not report our own write as
a conflict.

**Two mechanisms, two jobs** (amendment §8.1). `content_revision` identifies the
exact content an asynchronous operation observed: it drives staleness, the
`SaveCompleted` payload, persistence event identity and disk metadata. The
transaction token decides clean versus dirty. They are not interchangeable, and
they differ in exactly one case: undoing back to the saved content moves the
revision forward while returning the token to the saved value, so the document is
`Clean` again. That is intended, and it is the Stage 1 behaviour.

Concurrent saves of one document are serialized: a save requested while one is
in flight queues behind it, and a third request supersedes the queued one rather
than growing a chain.

## Implementation evidence (Stage 1.1, item 2)

Async opening landed first. Two measurements shaped it:

| | before | after |
| --- | ---: | ---: |
| Keystrokes accepted during a 100 MB open | 0 (frozen 354 ms) | 279,992 |
| Keystroke P95 during that load | n/a | 1.1 µs |
| Worst completion-apply frame | 110 ms | 62 µs |

The second row is the one that changed the design. The first implementation had
the worker return decoded text and the interactive thread build the rope, which
moved the freeze from "during the read" to "at the moment of completion" - a
110 ms frame. Building the rope on the worker (the buffer is `Send`) removed it.
"Construct TextBuffer" is task-side work, and now actually is.

## Implementation evidence (Stage 1.1, item 3)

Async saving landed second, on the same measurement pattern. `document.save` is
the old synchronous helper, still measured for comparison; `A2` samples input
latency *during* the asynchronous write.

| | 10 MB | 100 MB |
| --- | ---: | ---: |
| Synchronous save on the interactive thread (P95) | 46.0 ms | 7.63 s |
| Asynchronous save, request cost on that thread (P95) | 50.4 us | 99.6 us |
| Keystrokes accepted while the save ran | 23,777 | 420,069 |
| Keystroke P99 during the save | 3.0 us | 2.9 us |
| End-to-end save (request to applied completion, P95) | 68.2 ms | 719.6 ms |
| RSS: before / after snapshot / during write / after | 29.5 / 29.5 / 33.2 / 33.2 MB | 225 / 225 / 261 / 261 MB |

Two things in that table are worth stating plainly. The request cost does not
scale with document size, because the snapshot is an `Arc` clone of the rope
rather than a copy of the text. And RSS does not double: the growth during the
write is the copy-on-write cost of the edits made *while* saving, not a second
copy of the document -- the 100 MB row grew by 36 MB across 420,069 keystrokes,
and returned to the same level once the save completed.

Two implementation findings changed the design as written above.

**The transaction token is captured at request time, not read at completion
time.** The original text said the completion compares the document's token
against the saved one. That is wrong when the document was edited during the
save: the token read at completion is the *current* one, so every save would
look clean. `SaveSnapshot` now carries the token that was current when the
snapshot was taken, and `mark_saved_at` sets `saved_token` from that captured
value. Clean/dirty then falls out of comparing it to the token now.

**The undo boundary is forced at request time, not at completion.** A boundary
forced when the save completes belongs to whatever the user was typing at that
moment, which is arbitrary. `mark_saving` now calls `history.force_boundary()`,
so the boundary is where the user asked to save -- which is also what makes
"undo back to the saved content reports Clean" reachable in one step.

## Consequences

* `EditorCore::save` becomes a request. The shell learns the outcome from
  `SaveCompleted` / `SaveFailed` events rather than from a return value.
* `PersistenceState::SaveSucceeded` carries a revision.
* Quit and close-tab flows must account for a save in flight: closing a document
  with a pending save waits for it or cancels it deliberately, and never
  silently discards it.
* A failed save leaves the previous file intact, exactly as in Stage 1, and the
  document stays dirty.
* Save latency stops being an interactive metric and becomes a task metric
  (queue wait + wall time + bytes written), which is where it belongs.

## Validation

Re-run the Stage 1 workloads and add the interactive measurement that Stage 1
could not make:

```text
100 MB open   input latency sampled *during* the load    met: P99 2.2 us
100 MB save   input latency sampled *during* the save    met: P99 2.9 us
100 MB save   peak RSS during save                       met: 225 -> 261 MB, not 450
              save correctness: bytes identical to the synchronous path
              revision correctness: the 40/41/42 case
              token correctness: undo back to saved content reports Clean
              failure path: original intact, document still dirty
              cancellation: partial write removed, no temporary files left
```

"Interactive thread never blocked" is the claim; sampled input latency during a
100 MB operation is the evidence. Results are in
`benchmarks/results/stage-1.1-item3.txt`; the correctness rows are the tests in
`tests/tests/async_save.rs`.

## Reconsideration criteria

* If per-document save serialization proves too coarse (a slow network drive
  blocking an unrelated local save), move serialization from per-document to
  per-volume.
* If cancellation latency for a large read is dominated by a single blocking
  syscall, revisit chunked reads with explicit cancellation checkpoints.
* If `estimated_cost` for I/O tasks turns out to be a poor admission signal,
  replace it with a measured moving average per subsystem.
