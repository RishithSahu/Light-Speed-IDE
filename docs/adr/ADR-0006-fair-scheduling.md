# ADR-0006: Fair scheduling

**Status:** Deferred in Stage 1; superseded by Foundation Amendment 001; implemented in Stage 1.1
**Date:** 2026-08-25

> The aging formula and the base-priority policy table are specified in
> `docs/foundation-amendment-001.md` section 5, which carries the same three-term
> formula as this ADR: `base_priority + aging + deadline_pressure`.

## Context

Specification section 44 states the fairness policy:

```text
effective_priority = base_priority + aging + deadline_pressure
```

with interactive work at the highest practical priority and a guarantee that
lower-priority background work is not starved indefinitely while resources are
available.

Stage 1 has no scheduler (ADR-0004) and no background work, so there is nothing
to schedule fairly.

## Decision

Defer the implementation. Record two things that Stage 1 must not compromise:

1. **Interactive work is never queued behind anything.** In Stage 1 this is
   trivially true — the interactive thread is the only thread. The property to
   preserve is that a keystroke never waits for a queue.
2. **Every task will need queue-wait accounting.** Section 45 lists
   `queue_wait_time` alongside wall and CPU time. `ls-perf` already records
   latency distributions per named metric with budget comparison, so a future
   task's queue wait is a metric, not a new subsystem.

## Alternatives

**Fixed priority bands with no aging.** Simplest, and wrong in a predictable
way: Git status refreshes and indexing at the bottom band would never run during
sustained typing, which is exactly the starvation section 44 forbids.

**Strict FIFO.** Fair by construction, but it puts a 30-second workspace scan
ahead of a 5 ms diagnostic refresh, which is the opposite of what a user
perceives as responsive.

## Reasoning

Aging plus deadline pressure is a well-understood way to keep priorities
meaningful without starvation, and the specification already commits to it. What
is not yet knowable is the tuning: how fast aging accrues, what counts as a
deadline, and how admission interacts with memory pressure. Those need real
workloads (search over a large repository, an indexing pass, a Git status on a
big tree) to answer with measurements rather than intuition.

## Consequences

* No Stage 1 code assumes work is instantaneous *because* it is on the
  interactive thread. Operations are measured individually, so moving one to a
  worker later changes where it runs, not what it costs.
* The two blocking operations that exist (large file open and save) are already
  identified as the first admission candidates.

## Reconsideration criteria

Design and implement alongside ADR-0004 when the first background subsystem
lands. Tuning must be driven by the adversarial workloads in section 53 — in
particular A1 (typing while search runs) and A3 (typing while indexing runs),
which are the cases where a fairness bug becomes a latency bug.
