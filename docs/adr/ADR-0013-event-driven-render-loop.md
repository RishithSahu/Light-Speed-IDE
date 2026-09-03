# ADR-0013: Event-driven rendering

**Status:** Accepted (Stage 1.1)
**Date:** 2026-08-25
**Amends:** baseline §26-28 via `docs/foundation-amendment-001.md` §11-12

## Question

Should rendering run continuously, or only when something has invalidated?

## Context

Stage 1 already redraws on demand: the event loop uses `ControlFlow::Wait` and a
frame is produced when the shell calls `request_redraw`. That is the right
instinct, but it is implemented as a convention rather than as a model — the
invalidation sources are wherever someone remembered to call `request_redraw`,
and there is no explicit state for "a frame is needed".

The consequence is visible in one deliberate omission: **Stage 1 has no caret
blink.** It was left out because a blinking caret under a convention-based
redraw scheme is easiest to implement as a continuously running frame loop, and
a continuously running loop contradicts the ≤2% idle CPU contract (baseline
§51). The absence of blinking is therefore a symptom of the render loop's shape,
not a design preference.

## Alternatives

**1. Continuous frame loop (60 FPS forever).** Trivial to reason about, standard
in games, and wrong for an editor: it burns CPU and battery to redraw an
unchanged screen, and it makes the idle CPU contract unreachable by
construction.

**2. Convention-based on-demand redraw (Stage 1 behaviour).** Cheap when idle,
but the set of things that trigger a frame is implicit. A missing
`request_redraw` is a silent bug — the screen is simply stale — and there is no
place to hang caret timing.

**3. Explicit invalidation-driven state machine.** A frame is produced when, and
only when, a named invalidation source fires. Costs an explicit model and the
discipline of enumerating the sources.

## Decision

**Adopt alternative 3.**

```text
Idle
 ↓ event
Dirty
 ↓
Render
 ↓
Presented
 ↓
Idle
```

A frame exists only if one of these invalidated:

```text
input
cursor change
selection change
scroll
window resize
document update
diagnostic update
caret timer
```

The list is exhaustive and lives in the code as an enumeration, so adding a new
source is an explicit change. Invalidation stays regional (baseline §28):
whole-document invalidation requires a correctness reason.

**Caret blinking** becomes affordable:

```text
CaretVisible
    ↓ 500 ms
CaretHidden
    ↓ 500 ms
CaretVisible
```

Each transition invalidates only the caret region, so blinking costs **two small
redraw events per second**, not a spinning loop. Blinking pauses while the user
is typing (the caret is solid immediately after any edit or movement), which is
both conventional and cheaper than blinking through a burst of keystrokes.

## Reasoning

The difference between alternatives 2 and 3 is not performance — Stage 1 already
idles at no frames — it is *auditability*. With an explicit model:

* a missed invalidation is a testable defect, because the sources are named;
* caret timing has an obvious home, so the feature that was dropped comes back
  without contradicting the CPU contract;
* the future Foundation subsystems that will want to invalidate (diagnostics
  arriving, Git status changing, search results decorating a line) have a
  contract to satisfy rather than a `request_redraw` call to copy.

## Consequences

* The shell gains an explicit render-state enum and a timer source. The timer is
  the only wake-up that exists without user input, and it stops when the window
  loses focus.
* Frames become countable: "frames drawn per keystroke" and "frames drawn while
  idle" are metrics, and the second should be zero except for caret transitions.
* A dropped invalidation shows as a stale screen. That is the risk the model
  introduces, and it is mitigated by naming the sources and testing them.

## Validation

Measure before and after:

```text
idle CPU              (contract: <=2% average, with blinking active)
battery impact        (frames per minute while idle)
input latency         (input -> state, input -> frame; must not regress)
caret responsiveness  (time from edit to solid caret)
```

The Stage 1 numbers are the baseline: input→state and input→frame must be
unchanged or better, and idle CPU with blinking must remain inside the contract.
If blinking cannot be delivered within the idle CPU budget, blinking is what
gets dropped — not the budget.

## Reconsideration criteria

* If frames-while-idle is ever non-zero for a reason other than the caret timer,
  an invalidation source is firing spuriously; find it rather than raising the
  budget.
* If a future subsystem needs animation (smooth scrolling, a progress
  indicator), it declares a bounded animation source rather than reintroducing a
  continuous loop.
