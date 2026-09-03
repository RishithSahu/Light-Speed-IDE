# ADR-0012: Staged application startup

**Status:** Accepted (Stage 1.1)
**Date:** 2026-08-25
**Amends:** baseline §49 via `docs/foundation-amendment-001.md` §14

## Question

How do we make the application usable before expensive GPU initialization
finishes?

## Context

Stage 1 measured startup per phase across five cold launches on Windows 11,
i5-12450HX, RTX 3050 Laptop, release build:

| Phase | All backends | Native backend first |
| --- | ---: | ---: |
| Window creation | 75-118 ms | 75-118 ms |
| GPU adapter enumeration | 510-562 ms | 277-306 ms |
| GPU device creation | 48-64 ms | 94-223 ms |
| Pipeline creation | 1-2 ms | 7-23 ms |
| Font system | 36-39 ms | 56-74 ms |
| **Total to usable editor** | **943-1057 ms** | **542-740 ms** |

Preferring the platform's native backend removed ~40% of startup and brought the
editor inside the 1 s failure threshold. It did not reach the 500 ms target, and
the remaining cost is not ours: adapter enumeration and device creation are
driver work that happens before any LightSpeed code runs.

The structural problem is that Stage 1 startup is a straight line — window, then
GPU, then first frame — so the user waits for the GPU even though nothing they
can see yet requires it.

## Alternatives

**1. Synchronous GPU startup (Stage 1 behaviour).** Simple, one code path, and
the window appears at 100 ms but shows nothing for another 500 ms. Rejected: it
makes the startup contract depend entirely on driver latency we do not control.

**2. Staged startup.** Present the window and accept input as soon as the window
exists; bring the GPU up behind it; promote to GPU rendering when ready.
Requires the shell to tolerate a period with no renderer.

**3. CPU temporary renderer.** Stage 2 plus a software rasterizer that draws the
first frames, so the editor is not merely responsive but *visible* immediately.
Costs a second rendering path — a real one, that has to lay out text — for a
window of a few hundred milliseconds.

**4. Adapter cache.** Persist the previously successful backend/adapter identity
and reuse it next launch to skip discovery. Attractive, unmeasured, and carries
correctness risk when hardware or drivers change.

## Decision

**Adopt staged startup (alternative 2).**

```text
process start
    ↓
configuration + logging          (measured)
    ↓
window created, input accepted   (measured)  <- "usable editor" starts here
    ↓
GPU adapter + device + pipelines (measured, off the critical path)
    ↓
first GPU frame presented        (measured)
```

"Usable editor" is redefined (amendment §14) as: the window is presented, the
editor accepts input, and any document named on the command line is either
loaded or visibly loading. GPU readiness is explicitly not part of it.

Input that arrives before the renderer exists is applied to the editor core
normally — the core has never needed a GPU — and is reflected in the first frame
that is drawn.

**Alternative 3 (CPU temporary renderer) is deferred**, not rejected. It becomes
worthwhile only if staged startup leaves a visually blank window long enough to
be objectionable; that is a measurement, taken after staging lands.

**Alternative 4 (adapter caching) is not decided by this ADR.** See below.

## Adapter caching: hypothesis, not decision

> **Hypothesis.** Persisting the previously successful backend/adapter identity
> may reduce startup discovery time on the same machine.

It is recorded as a hypothesis because the benefit is unmeasured and the failure
modes are real: a cached adapter can be stale after a driver update, a GPU
change, or an external display being docked or undocked, and recovering from a
stale entry may cost more than discovery would have.

**Validation plan.** Measure across:

```text
cold launch without cached adapter
cold launch with cached adapter
cold launch after driver update
cold launch after device change
cold launch after docking / undocking
```

**Metrics.**

```text
adapter discovery time
device creation time
total startup time
failure / recovery rate
```

**Promotion criteria.** If caching produces a material reduction in total
startup (target: ≥100 ms P95) *and* the failure/recovery rate across the stale
cases is indistinguishable from the uncached baseline, promote it into this ADR
as an accepted decision. If it does not materially help, do not add the
complexity.

## Consequences

* The shell gains a state where `renderer: None` is normal rather than an error,
  and the redraw path must handle it. This is a small amount of complexity in
  exactly one place.
* Startup phases stay individually instrumented; the phase table above is the
  regression baseline.
* The 500 ms target becomes reachable without depending on driver behaviour,
  because the measured quantity no longer includes the driver.
* A window that is responsive but not yet painted is a new state to design for.
  It must not look broken; the first paint should not flash.

## Reconsideration criteria

* If the gap between window creation and first frame exceeds ~300 ms in
  practice, evaluate alternative 3 (CPU temporary renderer) with measurements.
* If GPU initialization ever fails on a target machine, staged startup makes a
  software fallback a policy choice rather than a rewrite — revisit then.
* Revisit adapter caching when the validation plan above has been run.
