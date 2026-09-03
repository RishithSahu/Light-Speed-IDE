# ADR-0003: Thread ownership

**Status:** Accepted (Stage 1); extended by Foundation Amendment 001
**Date:** 2026-08-25

> Foundation Amendment 001 introduces Stage 1.1 and with it scheduler workers.
> The rule below did not change: it gained exactly one allow-list entry,
> `crates/scheduler/src/worker.rs`, enforced by
> `tests/tests/architecture.rs::the_worker_allow_list_is_exactly_the_scheduler`.
> See `docs/foundation-amendment-001.md` sections 3.5 and 3.6.

## Context

Specification section 40 divides work between an interactive thread, scheduler
workers and external processes. Section 41 makes it an absolute rule that every
non-interactive operation passes through scheduler admission, and that no
subsystem creates `thread::spawn`, a rayon pool, a tokio runtime or any other
executor outside scheduler-owned infrastructure. Section 42 requires CI to
enforce this.

The scheduler itself is Foundation Stage work (section 72), not Stage 1.

## Decision

**Stage 1 has exactly one thread and creates no others.** Input, commands,
editing, snapshot construction and rendering all run on the interactive thread.
Every operation the editor performs is synchronous and measured.

The rule is enforced by an architecture test
(`tests/tests/architecture.rs::no_subsystem_creates_its_own_workers`) that scans
every shipped source file for thread and executor creation and fails the build
if any appears.

## Alternatives

**Build a minimal scheduler now.** Rejected: section 58 excludes the scheduler
from Stage 1 and section 73 requires a full contract (data model, states,
events, backpressure, resource budget, security) before any subsystem is
implemented. A placeholder scheduler would be an abstraction with no current
consumer — and the one place background work would matter today, saving a large
file, is better solved by keeping saves fast than by making them concurrent.

**Move file I/O to a worker thread immediately.** Same objection, plus it would
introduce the hardest part of the design (cancellation, stale results,
backpressure) with none of the contracts that make it safe.

## Reasoning

Having no threads is not a limitation being deferred; it is what makes the Stage
1 interactive contracts trivially true. There is no lock contention, no
cross-thread invalidation, no possibility of the renderer observing a
half-applied edit, and every latency number in the benchmark report is the
complete cost of the operation.

It also sets the enforcement mechanism up before the temptation arrives: when
the scheduler lands, the architecture test's allow-list grows by exactly one
module, and every other subsystem stays unable to spawn work.

## Consequences

* Opening or saving a very large file blocks the frame loop. Measured: 354 ms
  P95 to open 100 MB, 617 ms P95 to save it. For files up to 10 MB the same
  operations are 33 ms and 49 ms. This is a real, documented limitation of
  Stage 1 and the first thing the scheduler will fix.
* Everything else — typing, cursor movement, selection, undo, tab switching,
  snapshot construction — is microseconds, so the single-threaded model is not
  a constraint on interactive work at all.
* `pollster::block_on` appears once, at startup, to await GPU adapter and device
  creation. It blocks the thread that is starting up rather than creating one,
  which is why the architecture test permits it.

## Reconsideration criteria

Revisit when the scheduler arrives (Foundation Stage). At that point:

* file loading and saving move behind scheduler admission with cancellation;
* the architecture test grows an allow-list entry for the scheduler crate;
* the interactive thread keeps ownership of input, commands and snapshots, which
  does not change.
