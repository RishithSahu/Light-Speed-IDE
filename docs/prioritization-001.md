# Prioritization Memo 001 — Async Interactive Core

**Status:** Accepted (reviewed and approved 2026-08-25)
**Date:** 2026-08-25
**Decides:** the implementation sequence for the milestone defined in
`docs/foundation-amendment-001.md`

---

## 1. Decision

The next implementation milestone is **Stage 1.1 — Async Interactive Core**, in
this order:

```text
1. Scheduler
2. Async document open/save
3. Revision-aware save completion
4. Backpressure/resource accounting
5. Event-driven rendering
6. IME
7. Large-paste optimization
```

Nothing from the Foundation Stage feature list — file tree, filesystem watcher,
search, syntax highlighting, language services, terminal, Git — begins until
this sequence is complete and the combined Stage 1 + Stage 1.1 suite passes.

## 2. Evidence

Stage 1 was measured before this decision was made. Two numbers drive it:

```text
100 MB open   ~354 ms
100 MB save   ~617 ms
```

Both currently run on the interactive thread. Everything else measured well
inside contract:

| Property | Result |
| --- | --- |
| Text editing | 1.4 µs P95 at 100 MB, against a 2 ms target |
| Memory | Editor core 5.4 MB; rope overhead 1.16x at 100 MB |
| Rendering isolation | Enforced by architecture test |
| Correctness | 277 tests passing |
| Architecture invariants | Enforced by CI, not by convention |
| Large-file I/O | **Blocks the interactive path** |
| IME | **Missing** |
| Scheduler | **Not implemented** |

The result is better than a uniformly fast editor would have been, because it
localizes the problem: the text core is not the bottleneck and does not need
work. File I/O is, and the reason it is a bottleneck is structural rather than
algorithmic — there is nowhere else for the work to run.

## 3. Why the scheduler comes first

The scheduler is not a performance enhancement. It is the infrastructure
required to remove a **known violation of the responsiveness architecture**:
baseline §40 forbids long operations on the interactive thread, and Stage 1
knowingly ships two.

```text
Scheduler first:
    enables async I/O
    enables future search / Git / LSP
    fixes current 100 MB UI blocking
    establishes resource and backpressure rules

IME second:
    fixes the largest remaining input correctness gap
    does not require the rest of the IDE foundation
```

Every later subsystem — search, Git, language services, indexing — is a
background producer that needs admission, cancellation, fairness and
backpressure on its first day. Building them before the scheduler would mean
either building each one's concurrency by hand (which baseline §41 forbids) or
retrofitting admission afterwards (which is how the rule becomes aspirational).

## 4. Why IME comes immediately after

IME is a correctness and accessibility gap, not a performance one: without it,
users of CJK and other composition-based input methods cannot type into the
editor at all. That is a larger *functional* defect than a 617 ms save.

It is sequenced second because:

* it does not depend on the scheduler (composition is interactive-thread work,
  amendment §13.5), so it cannot be blocked by scheduler work;
* it does not depend on any other Foundation subsystem;
* it *does* touch the render path, so doing it after event-driven rendering
  (item 5) avoids implementing preedit presentation twice.

The formal statement:

> Scheduler and async persistence are implemented before IME because they
> establish the background-execution infrastructure required by the next
> Foundation Stage and directly address the largest current interactive
> correctness/performance violation: 100 MB open/save blocking the frame loop.
> IME follows immediately because it is the largest remaining Stage 1
> input-compatibility gap.

## 5. Reading the order

Two framings appear in this memo and they agree:

* **By theme:** scheduler and async persistence first, IME second, everything
  else after.
* **By task:** the seven numbered items in section 1.

Items 1–4 are the async infrastructure theme. Item 5 is sequenced before IME for
the render-path reason in section 4, not because it outranks IME in importance.
Item 7 is last because it is a known, bounded inefficiency with no correctness
consequence: a very large paste is `O(n log n)` instead of `O(n)`.

## 6. Execution sequence

```text
 1. Write Foundation Amendment 001                      [done]
 2. Write ADR-0012 through ADR-0015                     [done]
 3. Review / approve those documents                    [done, approved]
 4. Implement Scheduler                                 [done]
 5. Convert open to a scheduler-managed operation       [done]
 6. Convert save to a scheduler-managed operation       <- next
 6. Implement revision-aware async save
 7. Benchmark 100 MB open/save again
 8. Implement event-driven rendering
 9. Benchmark idle CPU and caret behaviour
10. Implement IME
11. Run the complete Stage 1 + Stage 1.1 suite
```

Steps 7 and 9 are not optional checkpoints. The claim being made by this
milestone is a measurement claim, and it is only true if it is measured on the
same workloads that produced the numbers in section 2.

Only after step 11:

```text
file tree
filesystem watcher
search
syntax highlighting
language services
terminal
Git
```

## 7. Success criteria

The milestone succeeds if, at step 11:

```text
100 MB open   interactive thread never blocked
100 MB save   interactive thread never blocked
input latency unchanged or better than the Stage 1 baseline
idle CPU      within the <=2% contract, with caret blinking active
memory        no regression against Stage 1 workload measurements
Stage 1 DoD   still passes in full
```

Failing "input latency unchanged or better" would mean the scheduler bought
throughput at the cost of the property the whole architecture exists to protect,
and would be a reason to reconsider the design rather than to accept the number.

## 8. Risks

| Risk | Mitigation |
| --- | --- |
| The scheduler becomes a thread pool with extra steps | The state machine in amendment §3.2 forbids `Created → Running`; admission is a distinct, observable state with its own accounting |
| Async introduces races the Stage 1 tests cannot see | Documents stay owned by the interactive thread; tasks return results rather than mutating state (amendment §3.6) |
| Save completion clears dirty state incorrectly | Revision-aware completion (amendment §8) with an explicit test for the 40/41/42 case |
| Event-driven rendering drops frames it should have drawn | Every invalidation source enumerated (amendment §11); a missed invalidation is a visible bug and gets a regression test |
| Scope creep into Foundation features | This memo names the gate: nothing from section 6's final list starts before step 11 |
