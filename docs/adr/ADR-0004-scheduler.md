# ADR-0004: Scheduler

**Status:** Deferred in Stage 1; superseded by Foundation Amendment 001; implemented in Stage 1.1
**Date:** 2026-08-25

> The deferral below was correct for Stage 1 and is now spent. Measurement
> showed 100 MB open and save blocking the interactive thread, which makes the
> scheduler infrastructure required to remove a known architecture violation
> rather than an optimization. The full contract is in
> `docs/foundation-amendment-001.md` section 3; the sequencing argument is in
> `docs/prioritization-001.md`.
>
> **Implemented** in `crates/scheduler` (Stage 1.1, item 1).

## Context

Specification section 33 defines the scheduler contract (`submit`, `cancel`,
`pause`, `resume`), section 41 makes scheduler admission mandatory for every
non-interactive operation, and section 45 requires per-task resource accounting.
Section 58 excludes the scheduler from Stage 1; section 72 places it in the
Foundation Stage alongside the subsystems that need it — search, Git, language
services, indexing, the filesystem watcher.

Stage 1 has none of those subsystems. Implementing a scheduler now would mean
designing admission, fairness and backpressure against zero real workloads.

## Decision

Do not implement a scheduler in Stage 1. Instead, hold Stage 1 to the two
properties that keep the scheduler's future arrival cheap and safe:

1. **No threads exist** (ADR-0003), enforced by an architecture test. There is
   no ad-hoc concurrency for a scheduler to have to reclaim later.
2. **Every expensive operation is already measured** through `ls-perf`, so when
   work moves to a worker its interactive cost is a known number rather than a
   guess.

## Alternatives

**A minimal `submit`/`cancel` scheduler now.** Rejected. Section 73 requires a
full contract before implementation, and the design decisions that matter —
priority bands, aging, deadline pressure, admission under memory pressure — are
only answerable with real background work to schedule. A placeholder would
either be replaced wholesale or, worse, quietly define the semantics by accident.

**A thread pool without admission.** This is the specific failure mode section
41 exists to prevent, and it is much harder to remove than to never add.

## Reasoning

The scheduler's purpose is to protect the interactive thread from background
work. Stage 1 has no background work, so the protection has nothing to protect
against, but the *invariant* it enforces — that expensive work never runs
inline — is already testable: the benchmark suite asserts the interactive
budgets, and the two operations that genuinely block (open and save of very
large files) are documented as the first candidates for admission.

## Consequences

* File open and save block the frame loop in Stage 1 (ADR-0003).
* The editor core owns no `Arc<Mutex<...>>` shared state, because nothing is
  shared across threads. When the scheduler lands, the sharing boundary will be
  a deliberate design step rather than an inherited accident.
* `RenderSnapshot` is already immutable and `Arc`-shared (ADR-0007), so handing
  a snapshot to a worker for background analysis needs no new machinery.

## Reconsideration criteria

Implement when the first Foundation Stage subsystem needs it — expected to be
workspace search, which needs cancellation and incremental results on day one.
The contract to satisfy at that point is sections 33, 41, 43, 44 and 45
together, not the `submit`/`cancel` signature alone.
