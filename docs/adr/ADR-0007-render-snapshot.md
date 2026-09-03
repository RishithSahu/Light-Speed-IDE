# ADR-0007: RenderSnapshot

**Status:** Accepted (Stage 1)
**Date:** 2026-08-25

## Context

Specification section 26 defines `RenderSnapshot` as an immutable presentation
snapshot for exactly one rendering update, and section 27 requires the renderer
to consume snapshots rather than read mutable editor state. Section 28 requires
invalidation to be regional: no operation invalidates the whole document unless
correctness demands it. Section 66 requires the snapshot to be viewport-based.

## Decision

`EditorCore::render_snapshot(document, viewport) -> Option<Arc<RenderSnapshot>>`
builds a fresh snapshot containing:

* the document id and its `content_revision` at build time;
* the viewport, and only the visible lines' text (as `Arc<str>` per line);
* cursor and selection presentations, in both character columns and display
  columns;
* the accumulated `Invalidation` since the previous snapshot;
* a `DocumentPresentation` header (name, path, dirty, language, encoding, line
  ending, external and persistence state) for the tab and status bars.

The type has no `&mut self` methods and is published behind `Arc`. Diagnostics
and decorations are present as empty vectors: they are part of the shape the
Foundation Stage will fill, and leaving the fields out would force a breaking
change to every consumer later.

## Alternatives

**Let the renderer read the `Document` directly.** Simplest, and the thing
section 27 exists to prevent. It would let a frame observe a half-applied edit
and would make the renderer's read set an invisible constraint on every future
change to the core.

**One long-lived mutable snapshot, updated in place.** Cheaper in allocations,
but "immutable once published" becomes a convention rather than a property, and
handing a snapshot to a background task (Foundation Stage) would need locking.

**Snapshot the whole document.** Rejected by section 66 and by arithmetic: a
100 MB document would copy 100 MB per frame.

## Reasoning

Building a snapshot is bounded by the viewport, not the document, so its cost is
flat: measured P95 is 41-141 µs for 50 lines regardless of whether the document
is 1 KB or 100 MB (the 141 µs case is the Unicode workload, where multi-byte
text costs more to slice). That is under 3% of the 8 ms input-to-frame budget.

Sharing line text as `Arc<str>` means the snapshot borrows nothing from the
document and still copies only the visible region.

The `Invalidation` record is carried even though the Stage 1 renderer redraws
everything each frame. It is what lets the renderer skip re-shaping unchanged
regions today (`TextEngine::set_text` compares before shaping), and it is the
input a partial-redraw path would need later.

## Consequences

* An old snapshot keeps showing the revision it was built from — asserted in the
  integration suite. This is what makes a snapshot safe to hand to anything.
* The architecture test
  (`render_snapshots_expose_no_mutation`) fails the build if a mutating method
  is ever added to `RenderSnapshot`.
* Column values are computed in both characters and display columns at build
  time, so the renderer never has to reason about tabs or wide characters.

## Benchmark evidence

`render.snapshot_50_lines` P95: 41 µs (1 KB), 80 µs (64 KB), 87 µs (1 MB),
84 µs (10 MB), 72 µs (100 MB), 141 µs (1 MB Unicode), 60 µs (10 MB single line).

## Reconsideration criteria

* If snapshot construction exceeds ~1 ms P95, move line-text extraction to a
  lazily evaluated form rather than eagerly copying the viewport.
* When diagnostics and decorations become real, re-measure: they add per-line
  work that the current numbers do not include.
