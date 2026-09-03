# ADR-0017: Filesystem change notification

**Status:** Accepted (interim, explicitly temporary)
**Date:** 2026-09-01

## Question

How should LightSpeed learn that a file it has open changed outside the
editor? What shipped for item 5 (a 1.5-second poll) is a stopgap, not the
architecture -- this ADR exists specifically so that stopgap cannot quietly
become permanent by nobody ever revisiting it.

## Context

`EditorCore::refresh_external_state` already existed before this item: given a
document, it stats the file, compares the stamp against what was recorded at
open/save time, and reports `Unchanged` / `ExternallyChanged` / `Missing` /
`Conflict`. What it never had was anything calling it automatically. Item 5
added exactly one caller: a timer in the shell, riding the same
`about_to_wait` mechanism the caret's blink already used, checking every open
tab every 1.5 seconds.

```text
what shipped                          what this defers to
------------------------------------  --------------------------------------
timer                                 OS filesystem notification
  |                                     |
stat, stat, stat, stat (every 1.5s)   platform watcher (ReadDirectoryChangesW /
  |                                     inotify / FSEvents)
core.refresh_external_state             |
  |                                   event
RenderSnapshot's ExternalState          |
                                       scheduler / admission
                                         |
                                       core
                                         |
                                       RenderSnapshot
```

The poll is not wrong, exactly -- amendment section 3.6 and this project's
whole thread-ownership discipline would have to be stretched to justify a
*new* background thread for something a existing timer already reaches at
negligible cost (one `stat` per open tab, a few times a minute). But it is
also not what "LightSpeed" is supposed to mean: change detection bounded by a
polling interval instead of an actual OS signal, silently accepted as
"good enough" is exactly the kind of drift this codebase's ADR discipline
exists to catch before it calcifies.

## Alternatives

**1. Keep polling, shrink the interval.** Does not change the shape of the
answer, only its latency and its constant cost. Rejected as not actually
addressing the concern -- a faster poll is still a poll.

**2. A native filesystem watcher per platform** (`ReadDirectoryChangesW` on
Windows, `inotify` on Linux, `FSEvents` on macOS), run as a background task
under the scheduler, publishing change events the same way a document load or
a git-status task publishes its result today.

**3. A third-party cross-platform watcher crate** (e.g. `notify`), trading a
new dependency for not hand-rolling three platform backends.

## Decision

**Interim, effective now: the 1.5-second poll stays.** It is cheap, it is
already shipped, and item 5's actual behavior (a document flips to
`ExternallyChanged` / `Missing` / `Conflict` and the status bar reports it)
does not change regardless of which underlying mechanism drives it -- a
correctness-preserving implementation swap, not a user-facing feature this
document is asking for permission to skip.

**Target architecture: alternative 2**, matching how every other background
capability in this codebase already works -- a platform-specific watcher
(most likely `ReadDirectoryChangesW` first, Windows being the current focus)
runs as an admitted, bounded task rather than a bespoke thread, and its
events reach `EditorCore` through the same completion path git status and
workspace search already use. This keeps the watcher inside the existing
admission/fairness/accounting story instead of being a fourth, differently
governed background mechanism.

Alternative 3 is not rejected outright, but a hand-rolled watcher under the
scheduler is preferred *first*: it keeps every background capability
answering to the same admission and accounting model this project has spent
several ADRs establishing, rather than importing a crate whose own threading
model would need to be reconciled with it.

## Why the poll is explicitly temporary, not "the architecture"

Nothing about the poll is being defended as correct long-term design. It was
the cheapest thing that could be built inside this milestone's time budget
that still produces the right user-visible behavior, and cheap-but-correct is
a legitimate interim state as long as it is written down as interim. This ADR
is that writing-down. A future session picking up ADR-driven work should read
"Accepted (interim, explicitly temporary)" at the top of this document as a
standing task, not as a closed decision.

## Consequences

* `app/src/app.rs`'s `poll_external_changes` and `EXTERNAL_WATCH_INTERVAL`
  carry a doc comment pointing at this ADR.
* No new dependency, no new thread, no allow-list change for the interim.
* `EditorCore::refresh_external_state`'s public shape does not need to change
  when the watcher lands: it already takes a `DocumentId` and returns a state,
  which is exactly what an event-driven caller needs too.

## Reconsideration criteria

* A workspace large enough, or a workflow interactive enough with an external
  tool (a formatter, a code generator, `git checkout`), that 1.5 seconds of
  staleness is a real problem rather than a theoretical one.
* Once a scheduler-admitted watcher task exists for Windows, whether it is
  worth the additional platform-specific code for Linux/macOS parity or
  whether the poll remains acceptable there.
