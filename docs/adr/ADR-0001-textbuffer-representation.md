# ADR-0001: TextBuffer representation

**Status:** Accepted (Stage 1)
**Date:** 2026-08-25

## Context

The specification (section 16) requires a text container supporting insert,
delete, replace, range access, line access and offset conversion, applying
incremental edits without rebuilding the document. It names three candidates —
piece table, rope, gap buffer — and requires the choice to be benchmark-driven.

The performance contracts that bear on this decision:

| Operation | Target P95 | Failure P95 |
| --- | ---: | ---: |
| input → editor state | 2 ms | 5 ms |
| cursor movement | 4 ms | 10 ms |

Workloads run from 1 KB to 100 MB, including a 10 MB single-line file, and
memory contracts cap the whole editor at 120 MB for an empty session.

## Decision

A **B-tree rope**: leaves hold UTF-8 chunks of at most 1 KB, internal nodes hold
at most 8 children, and every node caches a `TextInfo` summary of its subtree
(bytes, characters, line breaks). Nodes are held behind `Arc` and edited
copy-on-write.

## Alternatives

**Gap buffer.** Fast for typing in one place, but moving the gap is `O(n)`
memcpy. On a 100 MB file a cursor jump plus one keystroke moves tens of
megabytes; at ~10 GB/s that is several milliseconds — the failure threshold, for
one keystroke. Line lookup also needs a separate index that must be maintained
on every edit.

**Piece table.** Excellent for append-heavy editing and gives cheap undo, but
the piece list degrades with edit count: a session with 50,000 scattered edits
turns every offset lookup into a scan of 50,000 pieces unless the list is itself
indexed by a tree — at which point the design *is* a rope, with an extra
indirection to the original and added buffers.

**Immutable `String` rebuilt per edit.** Explicitly ruled out by the brief, and
correctly: 100 MB copied per keystroke.

## Reasoning

The rope's cost is bounded by tree depth, not document size. Measured depth is 1
at 1 KB and 6 at 100 MB, so an edit rewrites one 1 KB chunk plus six small
nodes. Every position query — character to line, line to character, character to
byte — walks the same six nodes using the cached summaries, which is why cursor
movement in a 100 MB file costs the same order as in a 1 KB file.

Two properties beyond raw speed decided it:

* **`Arc` + copy-on-write makes a snapshot O(1).** The renderer is handed an
  immutable snapshot every frame (ADR-0007); with any in-place representation
  that would mean copying or locking.
* **Chunking bounds the worst case.** A 1 KB chunk is one page-ish memcpy, so
  even the pathological 10 MB single-line workload edits in microseconds
  (measured P95 1.0 µs) because line length never enters the cost.

## Benchmark evidence

`cargo run --release -p ls-bench` on Windows 11, i5-12450HX, 16 GB
(`benchmarks/results/full.json`, workload definitions v1):

| Workload | Type char (start) P95 | Type char (middle) P95 | Cursor char P95 | Depth | Memory |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 KB | 1.6 µs | 800 ns | 2.5 µs | 1 | — |
| 64 KB | 1.3 µs | 1.2 µs | 1.9 µs | 3 | 0.87x |
| 1 MB | 1.2 µs | 1.1 µs | 1.5 µs | 4 | 1.36x |
| 10 MB | 2.0 µs | 2.0 µs | 3.9 µs | 5 | 1.16x |
| 100 MB | 1.2 µs | 1.4 µs | 1.3 µs | 6 | 1.16x |
| 1 MB Unicode | 2.0 µs | 2.2 µs | 4.0 µs | 4 | — |
| 10 MB one line | 1.0 µs | 1.0 µs | 500 ns | 5 | 1.07x |

Typing latency is flat across five orders of magnitude of document size, at well
under 0.1% of the 2 ms target. Overhead settles near 1.16x of document bytes
at scale (the 15.9x figure at 1 KB is allocator granularity on a document
smaller than one page, not a trend).

## Consequences

* Chunk boundaries never split a character or a `\r\n` pair, so every chunk is
  independently valid UTF-8 and can be written to disk directly.
* A large paste is chunked into leaf-sized pieces and inserted sequentially,
  which is `O(n log n)` rather than the `O(n)` a split/concat rope would give.
  Measured at 1 KB pastes this is single-digit microseconds; a 100 MB paste
  would be slow, and is a known limitation rather than a supported operation.
* `TextBuffer::validate()` checks the invariants (uniform leaf depth, cached
  summaries matching actual text, fill bounds) and is asserted after every
  structural test.

## Reconsideration criteria

Revisit if any of these becomes true:

* P95 typing latency exceeds 200 µs on any workload (100x margin lost);
* memory overhead exceeds 1.5x of document bytes at 100 MB;
* multi-cursor editing (Stage 2+) makes per-edit tree traversal the profile's
  hot spot, which would argue for batching edits into a single traversal.
