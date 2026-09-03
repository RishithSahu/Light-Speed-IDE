# Stage 1.1 implementation map

**Status:** Working plan — derived from the approved contracts, not itself a contract
**Date:** 2026-08-25
**Sources:** `foundation-amendment-001.md`, `prioritization-001.md`,
ADR-0003 → ADR-0007, ADR-0012 → ADR-0015

This maps the approved contracts onto the code that exists today. It changes no
architectural meaning; where the code and a contract do not line up cleanly,
that is called out in section 9 rather than resolved silently.

---

## 0. What the code looks like today

| Fact | Consequence for Stage 1.1 |
| --- | --- |
| `EditorCore::open_document(&Path) -> Result<DocumentId, OpenDocumentError>` is synchronous and does read + decode + detect + construct inline | Becomes a request; the body moves into a task |
| `EditorCore::save/save_as/save_to` are synchronous; `save_to` borrows `&self.documents[&id]` to stream chunks | The borrow cannot cross to a worker; a persistence snapshot replaces it |
| `encoding::encode_to(writer, chunks, encoding, line_ending)` already streams chunk-by-chunk into a `Write` | §10's "no second contiguous allocation" is **already satisfied**; the work is formalizing `ByteSink` and moving execution, not rewriting the encoder |
| `workspace::write_file_atomic_with` already does temp → write → flush → fsync → atomic replace | Durability sequence is unchanged; only its execution context moves |
| Dirty state is **token**-based: `saved_token: TransactionId` vs `history.state_token()` | See section 9, question 1 |
| `by_path: HashMap<String, DocumentId>` gives one-document-per-file | Extends to cover in-flight loads |
| `document(id) -> Option<&Document>` already returns `Option` | A tab whose document has not arrived yet fits the existing shape |
| 17 scattered `request_redraw()` calls in `app.rs` | Exactly the convention-based invalidation ADR-0013 replaces |
| `EventLoop::new()` — no user event type | Must become `with_user_event()` so a completing task can wake the loop |
| `ControlFlow::Wait` already set | Correct foundation; caret timing needs `WaitUntil` |
| No IME handling, no `set_ime_allowed` | Whole path is new |
| Architecture test allow-list for worker creation is **empty** | Grows by exactly the scheduler module |
| `TextBuffer` contains no `Rc`/`RefCell`/raw pointers | `Send + Sync`, so a snapshot can cross to a worker — verified |

---

## 1. Scheduler — IMPLEMENTED

**Affected contract:** amendment §3 (all), §5, §6; ADR-0004, ADR-0006; baseline
§33, §40–§45.

> **Done 2026-08-25.** `crates/scheduler` (1,948 lines incl. tests), 52 unit
> tests, 2 new architecture tests, 5 benchmark scenarios. Two contract gaps
> found and closed in the amendment first: `TaskSpec.workspace` (§3.1) and the
> task-queue overload policy (§3.5.1). Details in the item 1 report.

**Planned code change**

New crate `crates/scheduler` (`ls-scheduler`), depending only on `ls-log` and
`ls-perf` — it must not know about documents, or the dependency graph inverts.

```text
crates/scheduler/src/
  lib.rs          Scheduler: submit / cancel / pause / resume / state
  task.rs         TaskId, TaskSpec, TaskHandle, TaskState, SubsystemId,
                  Priority, ResourceClass, CostEstimate
  cancel.rs       CancellationToken
  queue.rs        bounded admission queue, effective-priority ordering
  worker.rs       the ONLY place workers are created
  accounting.rs   TaskRecord + ls-perf integration
```

* `TaskState` is a real enum with a `transition` method that rejects illegal
  moves; `Created → Running` is unrepresentable, not merely unused.
* `Admitted` stops `queue_wait` accounting; `Running` starts `wall_time`.
* `TaskBody = Box<dyn FnOnce(&CancellationToken) -> TaskOutcome + Send>` keeps
  the scheduler generic; `ls-core` supplies closures.
* Completion is published through a **bounded** completion channel that the
  interactive thread drains — never awaited.
* Worker count: `available_parallelism` clamped to a small maximum, configurable.

**Fairness** — `effective_priority = base_priority + aging + deadline_pressure`.
The amendment's §5 text states the first two terms; ADR-0006 and the baseline
carry `deadline_pressure`, and `TaskSpec` already has `deadline`, so all three
are implemented (see section 9, question 2).

**Enforcement** — `tests/tests/architecture.rs::no_subsystem_creates_its_own_workers`
grows a literal allow-list of exactly `crates/scheduler/src/worker.rs`. Everything
else stays forbidden.

**Tests**

```text
new  scheduler: state machine rejects Created -> Running and every other illegal edge
new  scheduler: queue_wait stops at Admitted, wall_time starts at Running
new  scheduler: cancellation from Created / Queued / Admitted / Running
new  scheduler: cancelled task reports Cancelled, not Failed
new  scheduler: bounded queue rejects or sheds at capacity, never grows
new  scheduler: aging lifts a starved low-priority task above a stream of high ones
new  scheduler: interactive-adjacent priority ordering under load
new  scheduler: pause/resume
chg  architecture: worker allow-list contains exactly the scheduler module
```

---

## 2. Async document open — IMPLEMENTED

**Affected contract:** amendment §7; ADR-0015; baseline §24, §30.

> **Done 2026-08-25.** `crates/core/src/loading.rs` plus the request/pump path in
> `editor.rs`; the shell moved to `EventLoop::with_user_event()`. 17 integration
> tests, 4 new architecture tests, a development panel (`app/src/devpanel.rs`)
> and the A1 benchmark. Path identity, join, cancellation and event-loop
> ownership are documented in amendment §7.1-§7.5.

**Planned code change** — `crates/core/src/editor.rs`, `document.rs`

* `EditorCore::request_open_document(PathBuf) -> TaskId`.
* Split today's `open_document` body into:
  * interactive part (canonicalize, identity check, allocate `DocumentId`,
    register the tab) — stays on the interactive thread, bounded;
  * task part (read, decode, detect binary/encoding/line endings, build the
    buffer) — runs under admission, returns a `LoadedDocument` value.
* New `loading: HashMap<DocumentId, PendingOpen { task: TaskId, path: CanonicalPath }>`
  beside `documents`. `tabs()` includes loading ids; `document(id)` keeps
  returning `None` until the task lands. This is the minimal shape that satisfies
  "tab visibly Loading" without inventing a second tab model or a half-built
  `Document`.
* `by_path` is populated at **request** time, so a second request for the same
  canonical path joins the in-flight task and returns the same `TaskId`.
* `EventPayload::DocumentLoaded { document, path, bytes, lines }` and
  `DocumentLoadFailed { document, code }`; the existing `DocumentOpened` is kept
  for the moment a tab appears.
* `TabPresentation` gains `loading: bool`.
* Cancelling removes the tab and the `by_path` entry.
* Errors stay exactly the typed `OpenDocumentError` variants that exist now.

**Tests**

```text
new  core: request_open returns immediately; document arrives via event
new  core: two requests for one canonical path share one TaskId and one DocumentId
new  core: second request while loading does not create a second document
new  core: cancel during load removes the tab and frees the path
new  core: binary / not-found / permission errors arrive as typed failures
new  core: identity holds across `.`-relative and case-different spellings while loading
chg  integration: open_edit_save_reopen and friends move to the request + pump form
chg  app: open path uses the request API
```

---

## 3. Revision-aware save completion — IMPLEMENTED

**Affected contract:** amendment §8, §8.1, §9, §10; ADR-0015; baseline §25, §29.

### Inspection findings (2026-08-25)

| Question | Finding |
| --- | --- |
| Current owner | `EditorCore::save_to`, entirely on the interactive thread |
| Current blocking path | `mark_saving` -> borrow `&self.documents[&id]` -> `workspace.write_file_atomic_with` (temp, write, flush, fsync, replace) -> `mark_saved` |
| Why the borrow cannot cross | `save_to` streams `document.text().chunks()` straight out of the live document; a worker cannot hold that borrow, and the document must stay editable |
| Data that must cross | document id, `ContentRevision`, `TransactionId`, `CanonicalPath`, `Encoding`, `LineEnding`, and a `TextBuffer` snapshot (`Arc` clone, O(1)) |
| Data that must stay interactive-owned | the `Document` itself: text, cursor, selection, undo history, revision counter, `by_path`, tab order |
| Reusable as-is | `encoding::encode_to` (already streams chunk by chunk), `Workspace::write_file_atomic_with` -> `fsops::write_file_atomic_with` (already temp/flush/fsync/atomic-replace), `Workspace::stamp` |
| Needs a new entry point | `Document::mark_saved_at(path, stamp, saved_token)`: today's `mark_saved` reads `history.state_token()` *at completion*, which for an async save is the wrong token |
| Coalescing boundary | `mark_saved` calls `history.force_boundary()`. That is history mutation and must not happen at completion; it moves to the request, where the user's Save press is the boundary (baseline §23). `force_boundary` does not change `state_token`, so the captured token stays valid |
| Tests affected | 14 call sites of `save`/`save_as` across integration, regression, architecture and benchmarks. As with `open_document`, `save` stays as a blocking request+pump helper so they keep working; an architecture test keeps the shell off it |

### Planned shape

```text
request_save(id) -> capture snapshot -> submit -> PersistenceState::Saving
worker           -> encode -> temp -> flush -> fsync -> atomic replace -> SaveOutcome
pump             -> mark_saved_at(captured token) -> Clean iff token still current
```

Serialization per document: at most one in flight, at most one queued, a newer
queued save replaces the older one.

**Planned code change**

* `SaveRequest { document, revision, token, path, encoding, line_ending, snapshot: TextBuffer }`
  captured on the interactive thread (`snapshot` is an `Arc` clone — O(1)).
* Task writes; completion publishes `SaveCompleted { document, revision }`.
* On completion the interactive thread compares the captured **token** against
  the document's current token to decide clean/dirty, and reports the
  **revision** in the event and the disk stamp (section 9, question 1).
* Disk stamp is recorded regardless of staleness, so our own write is never
  reported as an external change.
* Per-document serialization: `in_flight_save: HashMap<DocumentId, TaskId>` plus
  at most one `queued_save`; a third request supersedes the queued one.
* `PersistenceState::SaveSucceeded` carries the saved revision.

**Tests**

```text
done core: the 40/41/42 case - save completes stale, document stays Dirty
done core: save completes current - document becomes Clean
done core: undo back to saved content after a stale save reports Clean (token semantics)
done core: disk stamp recorded even when the completion is stale
done core: save requested during an in-flight save queues; a third supersedes the queued one
done core: failed save leaves the original intact and the document dirty
done core: close/quit with a save in flight does not silently discard it
```

`tests/tests/async_save.rs`, 14 tests. Architecture enforcement is in
`tests/tests/architecture.rs`: the shell never calls a blocking save, saves are
admitted under `DOCUMENT IO` with a workspace and a byte count, the persistence
layer cannot mutate a `Document`, and only the persistence layer writes document
bytes.

### What shipped differed from the plan in two places

Both are recorded in the amendment (new sections 8.2 and 8.3) and in ADR-0015.

1. **The token is captured, not re-read.** `SaveSnapshot` carries the
   transaction token that was current when the snapshot was taken. Reading the
   token at completion time compares the document against itself, so a document
   edited during its own save would be wrongly reported clean.
2. **The undo boundary moved to the request.** `mark_saving` forces it, not
   `mark_saved_at`, so the boundary lands where the user pressed Save rather
   than wherever they happened to be typing when the write finished.

### Measured (benchmarks/results/stage-1.1-item3.txt)

| | 10 MB | 100 MB |
| --- | ---: | ---: |
| Synchronous save on the interactive thread, P95 | 46.0 ms | 7.63 s |
| Async request cost on that thread, P95 | 50.4 us | 99.6 us |
| Keystrokes served during the save | 23,777 | 420,069 |
| Keystroke P99 during the save | 3.0 us | 2.9 us |
| End-to-end save, P95 | 68.2 ms | 719.6 ms |
| RSS before -> during -> after | 29.5 -> 33.2 -> 33.2 MB | 225 -> 261 -> 261 MB |

---

## 4. Backpressure and resource accounting

**Affected contract:** amendment §4, §6; ADR-0005.

**Planned code change**

Stage 1.1 has exactly two producers — the scheduler's completion channel and the
existing event queue. The other four producers named in §4 (search, filesystem,
terminal, language) do not exist yet, so per §19 ("do not add speculative
abstractions") their **rules** are encoded as a reusable, tested policy where
they are genuinely shared, and nothing more:

* completion channel: bounded, drop-oldest with a counter, mirroring the event
  queue;
* `staleness`: a small helper for "result computed against revision N, current
  is M → discard", used by save completion today and by language services later;
* an architecture test that enumerates every queue type in the workspace and
  asserts each declares a bound.

`TaskRecord` carries the nine §6 fields, recorded through `ls-perf`
(`scheduler.queue_wait`, `scheduler.wall_time`, `task.<subsystem>.*`), with
`bytes_read`/`bytes_written` reported by the task body and `peak_memory` from
`ls_platform::process` where measurable. No second metrics system.

**Tests**

```text
new  scheduler: completion channel is bounded and counts drops
new  core: stale-result helper discards a result for a superseded revision
new  architecture: every queue type declares a bound (extends queues_are_bounded)
new  scheduler: accounting fields are populated for a completed task
new  scheduler: accounting overhead stays under a declared fraction of task time
```

---

## 5. Event-driven rendering + staged startup

**Affected contract:** amendment §11, §12, §14; ADR-0012, ADR-0013.

**Planned code change** — `app/src/app.rs`, `app/src/main.rs`

* `RenderState { Idle, Dirty, Render, Presented }` as an explicit enum.
* `Invalidation` source enum with exactly the eight approved sources; the 17
  scattered `request_redraw()` calls become `self.invalidate(Source::X)`.
* `EventLoop::<UserEvent>::with_user_event()` so a completing task wakes the
  loop; `UserEvent::TaskCompleted` is itself an invalidation source
  (`document update`).
* Caret timer via `ControlFlow::WaitUntil`; blink pauses on edit/movement and
  stops entirely when the window loses focus.
* Staged startup: window + input first, GPU after; `renderer: None` becomes a
  normal state on the redraw path. Separate metrics for `window visible`,
  `editor usable`, `GPU ready`, `first rendered frame`.
* Adapter caching is **not** implemented.

**Tests**

```text
new  app: invalidation source list is exhaustive (every enum variant maps to a trigger)
new  app: Idle -> Dirty -> Render -> Presented -> Idle transitions
new  app: no frame is produced without an invalidation
new  app: caret timer toggles visibility and invalidates only the caret region
new  app: typing makes the caret solid and defers the next blink
new  app: input applied before the renderer exists is reflected in the first frame
new  bench: idle CPU with caret on/off and overlay on/off (four combinations)
```

---

## 6. IME

**Affected contract:** amendment §13; ADR-0014.

**Planned code change**

* `crates/core/src/ime.rs`: `ImeState`, `Preedit`, the four-state machine, the
  five events, `ImeError` with its three distinct variants.
* Preedit never touches `TextBuffer`; it rides in `RenderSnapshot` as
  presentation state with its styling ranges.
* Commit applies **one** `Edit` transaction (`EditKind` gains an IME-commit kind
  so it never coalesces with adjacent typing).
* `app`: handle `WindowEvent::Ime`, call `set_ime_allowed`, report the caret
  rectangle with `set_ime_cursor_area`.
* Renderer draws preedit with composition underline styling.

**Tests** — split as the directive requires:

```text
new  core (deterministic): preedit does not change content_revision
new  core (deterministic): preedit creates no undo entry
new  core (deterministic): commit is exactly one transaction; undo removes it whole
new  core (deterministic): cancel is a no-op on buffer, revision and history
new  core (deterministic): commit over a selection replaces it in one transaction
new  core (deterministic): focus loss during composition clears preedit safely
new  platform integration (ignored by default): winit Ime event sequences
new  manual acceptance checklist: Japanese, Chinese, Korean IMEs - documented, not faked
```

---

## 7. Large-paste optimization

**Affected contract:** ADR-0001 consequences; amendment §15.2.

**Planned code change** — `crates/buffer/src/rope.rs`, `text_buffer.rs`

Build a balanced subtree from the pasted text once and splice it, instead of
chunk-by-chunk insertion, above a measured threshold. Only after items 1–6.

**Tests**

```text
new  buffer: bulk insert equals chunked insert byte-for-byte (property style)
new  buffer: validate() passes after bulk insert at start / middle / end
new  buffer: unicode boundaries and line indices survive bulk insert
new  core: one paste is one undo transaction regardless of path taken
new  bench: 1 MB / 10 MB / 100 MB paste against the Stage 1 baseline
```

---

## 8. Performance validation (§15 of the directive)

Extends `benchmarks/`, not a new framework. New adversarial workloads that
inject input **while** background work runs:

```text
A1  typing while 100 MB open      -> input latency P95 must hold
A2  typing while 100 MB save      -> input latency P95 must hold
A3  cursor movement while saving  -> input latency P95 must hold
A4  rendering while saving        -> frame time P95 must hold
A5  memory while 100 MB save      -> no doubling
A6  scheduler queue wait under saturation
A7  many queued low-priority tasks (starvation check)
A8  background completion during render
```

Baseline for "unchanged or better" is `docs/milestone-1-report.md`.

---

## 9. Questions before implementation

> **All resolved 2026-08-25.** Question 1: token semantics accepted; amendment
> section 8.1 and ADR-0015 updated. Question 2: three-term formula accepted;
> amendment section 5 corrected. Two further gaps found while implementing were
> closed the same way: `TaskSpec` gained `workspace` (required by the section 6
> accounting record), and the task queue's overload policy was specified as
> rejection rather than drop-oldest (amendment section 3.5.1).

**1. Dirty state: revision comparison vs transaction token.** *(RESOLVED: token)*

Amendment §8 says a save clears dirty only if "the saved revision is still the
current revision". The code decides clean/dirty by comparing a **transaction
token**, which differs in one case:

```text
save revision 40 starts
user types      -> revision 41
user undoes     -> revision 42, content identical to revision 40

revision rule:  42 != 40  -> Dirty
token rule:     token == saved token -> Clean
```

Stage 1 ships the token rule and has passing tests for it; the revision rule
would regress that behaviour, and Stage 1's DoD must stay green. Proposed
resolution: **use the token for the clean/dirty decision, and the revision for
the event payload, the disk stamp and staleness checks.** The §8 intent — a
stale save must never clear dirty — holds under both. If accepted, this is a
semantic clarification and amendment §8 gets one sentence added; if rejected, I
implement strict revision comparison and update the Stage 1 tests instead.

**2. `deadline_pressure` in the fairness formula.** *(RESOLVED: three terms)*

Amendment §5 writes `effective_priority = base_priority + aging`. ADR-0006, the
baseline §44 and the directive all write
`base_priority + aging + deadline_pressure`, and `TaskSpec` carries `deadline`.
I will implement all three terms (deadline_pressure is zero when `deadline` is
`None`) and, if you agree, amendment §5 gets the third term restored — it reads
as an omission in §5 rather than a decision to drop it.

**3. Loading tabs and `EditorCore::active()`.** *(informational)*

A tab can now be active while its document has not arrived. `document(id)`
already returns `Option`, so the shell's existing "no document" path handles it;
it will render a `Loading` state for that tab rather than the empty-editor
placeholder. No new tab model.

**4. Scheduler worker count.** *(informational)*

`available_parallelism` clamped to a small maximum, exposed as configuration so
the adversarial workloads can pin it. Not a contract change.
