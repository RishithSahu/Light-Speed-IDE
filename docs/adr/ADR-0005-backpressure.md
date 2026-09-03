# ADR-0005: Backpressure

**Status:** Accepted for Stage 1 producers; extended by Foundation Amendment 001
**Date:** 2026-08-25

> Per-producer contracts for search, filesystem events, terminal output and
> language services are specified in `docs/foundation-amendment-001.md`
> section 4. The Stage 1 event queue below is unchanged.

## Context

Specification section 43 requires every producer/consumer path to be bounded,
and names the allowed mechanisms: bounded queues, coalescing, cancellation,
stale-result dropping and producer throttling. Section 55 requires CI to verify
that queues are bounded.

Stage 1 has exactly one producer/consumer path: the event queue between the
editor core and the shell (section 37). The paths that motivate the rule —
filesystem event storms, search results, terminal output floods, language server
messages — belong to the Foundation Stage.

## Decision

The event queue is a bounded ring of 1024 events. On overflow it **drops the
oldest event and increments a counter** that is exposed through
`EditorCore::dropped_events()` and shown in the performance overlay.

Two other Stage 1 paths are bounded by construction rather than by a queue:

* **Latency samples** — `ls-perf` keeps a fixed ring of 4096 samples per metric.
  Count, sum, maximum and budget violations are tracked outside the ring, so
  long-run totals stay exact while memory stays constant.
* **Render snapshots** — exactly one snapshot per frame, built on demand. The
  core never accumulates snapshots; an old one lives only as long as the shell
  holds its `Arc`.

## Alternatives

**Unbounded event queue.** Rejected: a program that emits events faster than the
shell drains them would grow without limit, and the failure would appear as
memory exhaustion far from its cause.

**Block the producer when full.** Rejected for Stage 1: the producer is the
interactive thread, so blocking it to protect a diagnostic queue would trade a
dropped event for a dropped frame.

**Drop the newest event.** Rejected: recent events are the ones a shell most
needs (a save failure, a budget violation), so the oldest is the right thing to
lose.

## Reasoning

Dropping with a visible counter turns an invisible failure into a reported
number. The architecture test
(`tests/tests/architecture.rs::queues_are_bounded`) drives three times the
queue's capacity through the core and asserts both that the drain stays within
capacity and that the drop counter moved — so a future change to an unbounded
`Vec` fails the build rather than passing quietly.

## Consequences

* Events are diagnostic and best-effort. Nothing in the editor's correctness
  depends on observing every event, which is what makes dropping acceptable.
* The overlay surfaces the drop count, so a shell that stops draining is visible
  during development rather than in a bug report.

## Reconsideration criteria

Every Foundation Stage producer must declare its own mechanism before merging:
filesystem events coalesce, search results cancel on a newer request, terminal
output keeps a bounded scrollback, language server responses are rejected when
their `content_revision` is stale.
