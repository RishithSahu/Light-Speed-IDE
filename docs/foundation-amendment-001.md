# Foundation Amendment 001 — Async Interactive Core

**Status:** Accepted (reviewed and approved 2026-08-25)
**Date:** 2026-08-25
**Amends:** `docs/foundation-spec.md` (LightSpeed Foundation Specification)
**Related:** ADR-0012, ADR-0013, ADR-0014, ADR-0015, `docs/prioritization-001.md`

---

## 0. Standing and precedence

This amendment formally extends the Foundation Specification. The Foundation
Specification remains the baseline document and is not edited; this amendment is
read alongside it.

> **Where this amendment conflicts with the original Foundation Specification,
> this amendment supersedes the affected sections.**

Sections of the baseline that this amendment touches are listed in section 1.
Every other section of the baseline stands unchanged.

## 0.1 Why this amendment exists

Stage 1 shipped and was measured. The text core met every interactive contract
with three orders of magnitude of margin, and two facts came out of the
measurement rather than out of opinion:

```text
100 MB open   ~354 ms   blocking the interactive thread
100 MB save   ~617 ms   blocking the interactive thread
```

The baseline forbids exactly this. Section 40 states that the interactive thread
must not perform long operations; section 19 of the original brief forbids
expensive synchronous work on the interactive path. Stage 1 complied with the
letter of section 58 (no scheduler in Stage 1) at the cost of a documented
violation of section 40.

This amendment closes that gap by admitting background execution to the
architecture under an explicit contract, rather than by adding threads
opportunistically wherever something is slow.

## 1. Amended baseline sections

| Baseline section | Effect of this amendment |
| --- | --- |
| §24 Open Document Contract | `open_document` becomes a scheduler-admitted request returning a `TaskId`; document construction is published as an event (§7 below) |
| §25 Document State Separation | `PersistenceState::SaveSucceeded` carries the revision that was saved; clean/dirty is decided by transaction-token equality, while content revision identifies what async work observed (§8, §8.1 below) |
| §26–28 RenderSnapshot, render pipeline, invalidation | The render loop becomes an explicit invalidation-driven state machine (§11 below) |
| §29 Persistence | Saving streams through a `ByteSink` and runs under scheduler admission; atomicity guarantees are unchanged (§9, §10 below) |
| §30 Interface Contracts / `EditorCore` | `open_document` and `save` signatures change to request/completion form |
| §33 Scheduler Contract | Expanded from four function signatures to a full contract: task model, admission, priority, fairness, cancellation, backpressure, accounting, completion publication (§3 below) |
| §40 Thread Ownership | Scheduler workers now exist; ownership boundaries restated (§3.6 below) |
| §41 Scheduler Admission Rule | Restated as a formal MUST invariant with enforcement (§3.4, §3.5 below) |
| §42 Scheduler Enforcement | Concrete CI check list (§3.5 below) |
| §43 Backpressure | Per-producer contracts made explicit and testable (§4 below) |
| §44 Fair Scheduling | Formula and base-priority policy table (§5 below) |
| §45 Resource Accounting | Required fields per task (§6 below) |
| §49 Startup Contract | Startup becomes staged; the contract applies to "usable editor", which is redefined (§14 below, ADR-0012) |
| §58 / §71 Stage 1 plan and Definition of Done | Stage 1 DoD stands unchanged; Stage 1.1 is defined as an extension (§15 below) |
| — (new) | IME contract (§13 below) |

## 2. Scope of the milestone

This amendment defines **Stage 1.1 — Async Interactive Core**:

```text
scheduler
async document open
async document save
revision-aware save completion
streaming persistence
backpressure and resource accounting
event-driven rendering
IME
```

Implementation order and its justification are in `docs/prioritization-001.md`.

Explicitly **not** in Stage 1.1: file tree, filesystem watcher, search, syntax
highlighting, language services, terminal, Git. Those remain Foundation Stage
work and must be built on the contracts below rather than beside them.

---

## 3. Scheduler contract

The scheduler is not a thread pool. It is the admission and accounting authority
for all non-interactive execution:

```text
Scheduler
├── Task
├── admission
├── priority
├── fairness
├── cancellation
├── backpressure
├── resource accounting
└── completion publication
```

### 3.1 Data model

```rust
struct TaskSpec {
    subsystem: SubsystemId,
    priority: Priority,
    resource_class: ResourceClass,
    estimated_cost: CostEstimate,
    deadline: Option<Instant>,
    cancellation: CancellationToken,
    workspace: Option<WorkspaceRef>,
}
```

*(Added 2026-08-25: `workspace` was required by the section 6 accounting record
but had no source. It is an opaque identifier so the scheduler stays ignorant of
workspace semantics.)*

```rust
struct TaskHandle {
    id: TaskId,
    state: TaskState,
}
```

`SubsystemId` names the owner (`document_io`, `search`, `git`, `language`,
`indexing`, …) and is the key for resource accounting and for per-subsystem
budgets. `ResourceClass` declares what the task will contend for (`Cpu`, `Io`,
`Memory`, `Process`) so admission can reason about saturation rather than about
task count alone. `CostEstimate` is the submitter's honest guess (bytes to read,
files to scan, documents to parse); it is used for admission and is compared
against the measured cost afterwards, which is what makes estimates improve.

### 3.2 State machine

```text
Created
  ↓
Submitted
  ↓
Queued
  ↓
Admitted
  ↓
Running
 ├── Completed
 ├── Failed
 └── Cancelled
```

**No task may transition directly from `Created` to `Running`.** Every task
passes through `Submitted`, `Queued` and `Admitted` in order. A task may be
cancelled from any state before `Completed`; cancellation is normal control
flow, not an error (baseline §39).

`Admitted` is a distinct state, not an implementation detail of `Queued`: it is
the point at which the scheduler has decided that resources exist for this task
*now*, and it is the point at which `queue_wait` stops accruing.

### 3.3 Interface

```rust
fn submit(spec: TaskSpec, work: TaskBody) -> TaskId;
fn cancel(task: TaskId);
fn pause(task: TaskId);
fn resume(task: TaskId);
fn state(task: TaskId) -> Option<TaskState>;
```

Completion is **published**, never awaited on the interactive thread. A finished
task emits an event (§37 of the baseline) carrying its `TaskId`, its outcome and,
where applicable, the `content_revision` the work was performed against.

### 3.4 Admission rule (invariant)

> **Every non-interactive operation MUST be admitted by the Scheduler before
> execution begins.**

This replaces the baseline's "must pass through Scheduler admission" with an
unambiguous MUST. An operation is *interactive* only if it is bounded,
synchronous, and measured against a declared interactive budget. Everything else
is non-interactive by definition, including operations that are usually fast:
opening a 2 KB file is a scheduled task in exactly the same way opening a 100 MB
file is, because the code path must not depend on the size of the input.

### 3.5 Enforcement (invariant)

Production code outside the scheduler crate (and the process-management module
that spawns external processes) may not create worker executors. CI rejects
occurrences of:

```text
thread::spawn
thread::Builder
std::thread::scope
rayon::
tokio::spawn / tokio::runtime
futures executors / custom worker creation
```

outside the approved modules. The allow-list is a literal list of paths in the
architecture test, so adding a module to it is a reviewable change rather than
an invisible one.

The Stage 1 architecture test
(`tests/tests/architecture.rs::no_subsystem_creates_its_own_workers`) already
enforces this with an empty allow-list. Stage 1.1 grows that list by exactly the
scheduler and process modules — nothing else.

### 3.5.1 Queue overload policy

The event queue drops its oldest entry when full, because losing a notification
costs a notification. **A task queue must not do that**: dropping a queued task
silently discards submitted work, and for a save that means discarding a user's
document.

When the task queue is at capacity, submission is **rejected** with a typed
error and the task is never created. The submitter learns immediately and
decides what to do; nothing is lost silently. This is *producer throttling*,
which baseline section 43 lists among the allowed backpressure mechanisms.

```text
event queue full  ->  drop oldest, count the drop
task queue full   ->  reject the submission, return a typed error
```

Rejections are counted and exposed alongside the other scheduler counters.

### 3.6 Thread ownership (restates baseline §40)

```text
Interactive thread     input, commands, editor mutations, snapshot construction,
                       render submission, IME composition
Scheduler workers      all admitted tasks
External processes     terminal, future language servers, external tools
```

The editor core's document state remains owned by the interactive thread. A task
never mutates a `Document`; it produces a **result** that the interactive thread
applies. This is what keeps the Stage 1 guarantee that a frame can never observe
a half-applied edit.

---

## 4. Backpressure contract

**No queue in LightSpeed may be unbounded.** This is an architectural invariant
and must be covered by a test, in the same way the Stage 1 event queue is.

Each producer declares its mechanism:

**Search**

```text
new query
   ↓
cancel previous query
```

Results from a cancelled query are discarded on arrival, not merged.

**Filesystem events**

```text
event storm
   ↓
coalesce by path
   ↓
latest state wins
```

**Terminal output**

```text
output stream
   ↓
bounded buffer
   ↓
drop/archive old terminal history
```

Dropped history is counted and the count is visible, in the same way the Stage 1
event queue counts drops.

**Language services**

```text
result for revision N
   ↓
current revision != N
   ↓
discard
```

This is why every published completion carries the revision it was computed
against (§3.3).

**Editor events** — the Stage 1 bounded ring is unchanged: capacity 1024, drop
oldest, count drops.

---

## 5. Fair scheduling contract

Priority alone starves the bottom of the queue. Effective priority is:

```text
effective_priority = base_priority + aging + deadline_pressure
```

`deadline_pressure` is zero when `TaskSpec::deadline` is `None`; otherwise it
rises as the deadline approaches, within a configured horizon.

*(Corrected 2026-08-25: an earlier revision of this section wrote only
`base_priority + aging`, omitting the third term. ADR-0006 and baseline section
44 have always carried `deadline_pressure`, and `TaskSpec` carries `deadline`.
This is a documentation correction, not a design change.)*

All three terms are subject to resource-budget enforcement: an aged-up
background task may run sooner, but it may never consume resources that
interactive latency depends on. Aging changes *order*, not *entitlement*.

Base priorities are **configurable policy values, not hardcoded system truths**:

```text
USER INPUT       1000     interactive; never scheduled
RENDER            900     interactive; never scheduled
DOCUMENT IO       800     the user is waiting on this file
LSP               700
SEARCH            500
GIT               300
INDEXING          200
```

`DOCUMENT IO` is the Stage 1.1 addition: `document_io` is named as a subsystem
in section 3.1 but had no row here. It sits above every other schedulable
subsystem because a person is waiting for the file they asked for, and below
`RENDER` because rendering is interactive work that never enters the queue at
all.

They live in the configuration subsystem (baseline §10) and are expected to be
tuned against the adversarial workloads in baseline §53 — particularly A1
(typing while search runs) and A3 (typing while indexing runs), which are where
a fairness mistake shows up as a latency regression.

---

## 6. Resource-accounting contract

Every scheduled task records:

```text
task_id
subsystem
workspace
queue_wait
wall_time
CPU_time
bytes_read
bytes_written
peak_memory where measurable
```

These records feed:

```text
performance dashboard (the F12 overlay)
benchmark reports
future performance-regression engine
```

Stage 1 already reports named scenarios with P50/P95/P99/max and RSS. Task
records use the same metric infrastructure (`ls-perf`), so a scheduled task's
cost appears in the same report as an interactive operation's, and the
comparison between `estimated_cost` and measured cost is available for tuning
admission.

---

## 7. Async document open

Baseline §24 defined `open_document()` as a synchronous operation that returns a
fully materialized document. That is superseded.

```rust
fn request_open_document(path: PathBuf) -> TaskId;
```

```text
request
 ↓
scheduler
 ↓
read / decode / detect encoding / detect line endings
 ↓
construct document
 ↓
publish DocumentLoaded
```

The interactive thread returns immediately. For a small file the task may
complete within the same frame, so the operation still *looks* synchronous. For
a large file:

```text
UI remains interactive
↓
tab shows Loading
↓
document arrives
```

The tab is created immediately in a `Loading` presentation state so the user has
somewhere to look and something to cancel. Cancelling a load cancels the task
and removes the tab.

The responsibilities in baseline §24 are unchanged — canonicalize, detect binary,
detect encoding, detect line endings, construct the buffer, revision 0,
initialize history — and so are its prohibitions: no processes, no Git, no
language analysis, no project scan, no search, no rendering. Only the
*synchrony* changes.

Document identity (baseline §24: two references to one file resolve to one
document) now also covers in-flight loads: a second request for a path that is
already loading joins the existing task rather than starting a second one.

### 7.1 Path identity policy

The path is canonicalized **before anything else**, on the interactive thread,
and the canonical key is what identity is keyed on:

```text
raw path
   -> std::fs::canonicalize   (resolves `.`, `..`, symlinks, 8.3 short names)
   -> strip the Windows `\\?\` verbatim prefix for display
   -> case-fold on Windows for the identity key
```

So `src/main.rs`, `src/./main.rs` and (on Windows) `SRC/MAIN.RS` are one
document. Canonicalization requires the file to exist, which is why a missing
or unreadable path fails *before* a task is created rather than becoming a task
that is doomed to fail. This is one `stat`, and it is the only filesystem call
on the interactive path.

### 7.2 Join semantics

`by_path` maps a canonical key to a `DocumentId` from the moment a load is
requested, not from the moment it finishes. A request therefore resolves to one
of three outcomes:

```text
key not present        -> new tab, new task            joined = false
key present, loading   -> attach to the running task   joined = true
key present, loaded    -> activate the existing tab    already_open = true
```

Joining increments a counter on the pending load and emits
`DocumentLoadJoined`. **N requests for one path produce exactly one read**, and
the count of joined requests is visible in the load activity so the behaviour
can be checked in a running editor rather than only in a test.

A request for a document that is already open never re-reads the file: the copy
in memory, including unsaved edits, wins.

### 7.3 Cancellation semantics

A load can be cancelled at any point before it settles, and cancelling is not a
failure:

```text
cancel -> token set -> worker returns at its next check -> completion published
       -> the interactive thread removes the tab and frees the path key
```

The task polls its token between read chunks (1 MiB) and between phases, so
cancelling a 100 MB load stops within roughly one chunk rather than at the end
of the file. Closing a loading tab cancels its load; there is no unsaved work to
protect, because there is no document yet.

The tab disappears when the cancellation **completes**, not when it is
requested, because the tab is editor state and only the interactive thread
changes editor state.

### 7.4 Event-loop ownership

```text
worker  publishes a completion, then calls a waker
waker   posts EventLoop user event   <- the only thing a worker may do to the shell
loop    receives the user event, calls pump_completions()
core    applies results: builds documents, removes tabs, emits events
```

`pump_completions` is the single place a task result becomes editor state, and
it runs on the event-loop thread. A worker never holds a reference to a
`Document`; it returns a value.

The rope is built **on the worker**, not at completion. Building a 100 MB rope
takes about 110 ms, and doing it while applying the completion would put that
cost straight back on the interactive thread - trading a frozen load for a
frozen frame. This is what baseline §24's "construct TextBuffer" belonging to
the load means in practice.

### 7.5 Rejection

When the admission queue is full the request is refused with a typed
`OpenDocumentError::Rejected`, no tab is created, and nothing is silently
dropped (§3.5.1). The caller decides whether to retry.

## 8. Revision-aware save completion

A save is performed against a specific revision, and finishing a save says
nothing about revisions that came after it.

```text
revision 40
   ↓ user presses Save
save task captures:
    document_id = X
    revision    = 40
    content     = snapshot(40)
   ↓ user keeps typing
revision 41
revision 42
   ↓ save completes
SaveCompleted(document = X, revision = 40)
```

The interactive thread then compares:

```text
saved_revision   = 40
current_revision = 42
   ↓
ContentState = Dirty
```

A completed save **must not** clear dirty state unless the saved revision is
still the current revision. This is the concurrency contract that makes async
saving safe, and it replaces any notion of locking the document for the duration
of a write.

The same comparison drives the disk stamp: a save that completes against a stale
revision still records that the file on disk now holds revision 40, which is what
external-change detection needs to avoid reporting a conflict against our own
write.

### 8.1 Content revision and transaction token are different mechanisms

`content_revision` identifies the exact document content observed by
asynchronous operations. `transaction token` determines whether the current
editing history differs from the last successfully persisted transaction. A save
is stale with respect to content revision, while clean/dirty status is
determined by transaction-token equality.

```text
transaction token   ->  logical dirty / clean state
content revision    ->  exact content version observed by asynchronous work
```

`content_revision` is therefore used for async-result staleness, the
`SaveCompleted` payload, persistence event identity, disk and persistence
revision metadata, and future diagnostics and language-analysis versioning.

One consequence is deliberate. Given:

```text
save of revision 40 starts
edit  -> revision 41
undo  -> revision 42, content identical to revision 40
```

the document may become `Clean` again, because its transaction token has
returned to the saved token even though its revision number has moved on. This
preserves the Stage 1 behaviour and is intentional: revisions never decrease
(baseline section 22), so revision equality alone cannot express "the user
undid their way back to what is on disk".

### 8.2 Which token a completion compares (implementation finding)

Section 8.1 says clean/dirty is decided by transaction-token equality. It did
not say *which* token the save is compared against, and the obvious reading is
wrong.

A save must compare against the token that was current **when the snapshot was
taken**, not the one read when the completion is applied. Reading it at
completion time compares the document against itself, so a document edited
during its own save would be declared clean and the edits made during the write
would be silently treated as persisted.

```text
snapshot taken      token T1, revision 40      <- captured here
user types          token T2, revision 41
save completes      wrote the content of revision 40
                    saved_token := T1          <- captured value, not T2
                    T1 != T2  ->  still Dirty
```

`SaveSnapshot` therefore carries `token` alongside `revision`, and the
completion sets `saved_token` from the captured value.

### 8.3 Where the undo boundary is forced (implementation finding)

A save forces a history boundary so that undoing back to the saved content is
reachable in one step. That boundary is forced **when the save is requested**,
not when it completes: a boundary placed at completion time falls in the middle
of whatever the user happened to be typing, which is arbitrary and not
reproducible. `mark_saving` forces it; `mark_saved_at` does not.

## 9. Async save

Not this:

```text
serialize 100 MB
↓
write 100 MB
↓
block UI
```

This:

```text
Document revision N
        ↓
immutable persistence snapshot
        ↓
Scheduler
        ↓
stream encoded chunks
        ↓
temporary file
        ↓
flush
        ↓
fsync
        ↓
atomic replace
        ↓
SaveCompleted(N)
```

The persistence snapshot is an `Arc` clone of the rope root — O(1), because the
Stage 1 buffer is already copy-on-write. The document remains fully editable
while the snapshot is being written.

The durability sequence from baseline §29 (temporary file → write → flush →
fsync → atomic replace) is **unchanged**. Only its execution context changes.

Concurrent saves of the same document are serialized per document: a save
requested while one is in flight is queued behind it, and a third request
supersedes the queued one rather than adding to a growing chain.

## 10. Streaming persistence

The rope stores ≤1 KB leaves with ~1.16x overhead at 100 MB. Saving must
exploit that rather than defeat it by flattening the document into another
contiguous buffer.

```rust
trait ByteSink {
    fn write(&mut self, bytes: &[u8]) -> Result<(), PersistenceError>;
}
```

```text
Rope leaves
    ↓
encoding transform (UTF-8 / UTF-8 BOM / UTF-16 LE / UTF-16 BE)
    ↓
line-ending transform (LF / CRLF / CR)
    ↓
ByteSink
    ↓
temporary file
```

**A save must never materialize the whole document as a second contiguous
allocation.** Peak memory during a save is therefore the document plus a small
constant, not twice the document.

Chunk-wise transformation is already proven correct in Stage 1: chunk boundaries
never split a character, and the line-ending transform only rewrites `\n`, so
chunk-by-chunk output is byte-identical to whole-text output. That property has
a test and must keep having one.

---

## 11. Event-driven rendering

The render loop becomes an explicit state machine:

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

A frame is produced only when something invalidates:

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

No invalidation, no frame. This is not "60 FPS forever with a wait", it is a
loop that genuinely has nothing to do when the user has nothing to say.

Invalidation remains regional (baseline §28): an event names the region it
dirties, and whole-document invalidation requires a correctness reason.

## 12. Caret blinking

With an event-driven loop, blinking is affordable:

```text
CaretVisible
    ↓ 500 ms
CaretHidden
    ↓ 500 ms
CaretVisible
```

Each transition invalidates **only the caret region**. The cost is therefore:

```text
2 tiny redraw events per second
```

rather than a continuously spinning frame loop. Blinking pauses while typing
(the caret is visible whenever an edit or movement just happened), which is both
conventional and cheaper.

The Stage 1 decision to omit blinking was a consequence of the render loop's
shape, not a preference. Once §11 lands, the reason disappears.

---

## 13. IME contract (new)

### 13.1 Data model

```rust
struct ImeState {
    enabled: bool,
    preedit: Option<Preedit>,
    cursor: Option<usize>,
    selection: Option<Range>,
}
```

### 13.2 State machine

```text
Disabled
   ↓ focus editor
Enabled
   ↓ composition starts
Composing
   ├── update → Composing
   ├── commit → Enabled
   └── cancel → Enabled
```

### 13.3 Events

```text
ImeEnabled
ImePreeditChanged
ImeCommitted
ImeCancelled
ImeDisabled
```

### 13.4 Error model

```text
ImeError
```

must distinguish:

```text
unsupported platform behavior
window rejected IME configuration
invalid composition coordinates
```

These are genuinely different failures: the first is a platform capability gap,
the second is a recoverable configuration problem with the window, the third is
a bug in our own coordinate reporting. Collapsing them into one error makes the
third invisible.

### 13.5 Thread ownership

IME events arrive on the interactive/UI thread because they directly affect
input state. **Composition does not involve the scheduler.** An IME task would
add latency to the most latency-sensitive path in the editor for no benefit.

### 13.6 Editor semantics

Preedit text is **temporary presentation state**. It is carried in the render
snapshot and drawn with composition styling, and it is **not** inserted into the
committed `TextBuffer`.

On commit, the composed text enters the buffer as **one logical `Edit`
transaction** and therefore as one undo step. A three-keystroke composition that
commits one CJK character is one undo, not three.

Cancelling a composition leaves the buffer, the revision and the undo history
untouched — a cancelled composition never happened as far as the document is
concerned.

This must be tested explicitly: preedit does not change `content_revision`;
commit produces exactly one transaction; cancel is a no-op on the document;
undo after commit removes the whole committed string.

---

## 14. Startup contract (amends baseline §49)

Startup becomes staged (ADR-0012). The baseline's separate measurements —
process startup, window creation, first frame, usable editor — remain required,
and "usable editor" is redefined as:

> the window is presented, the editor accepts input, and any document named on
> the command line is either loaded or visibly loading.

GPU device readiness is explicitly **not** part of "usable". The target
(≤500 ms P95) and failure threshold (>1 s P95) are unchanged.

Adapter/device caching is **not** decided by this amendment. It is a hypothesis
with a validation plan; see ADR-0012.

---

## 15. Definition of Done

### 15.1 Stage 1 (unchanged, already achieved)

```text
core editor
Unicode semantics
line endings and encoding
cursor and selection
clipboard
undo/redo with coalescing
document revisions
atomic save
render snapshots
tabs
architecture tests
correctness tests
benchmarks
performance contracts measured
```

Stage 1 remains valid and its Definition of Done is not reopened. Its two
documented gaps — IME, and large-file I/O on the interactive thread — are closed
by Stage 1.1 rather than by amending Stage 1.

### 15.2 Stage 1.1 — Async Interactive Core

```text
[ ] scheduler with the state machine in §3.2
[ ] admission invariant enforced by CI (§3.5)
[ ] async document open (§7)
[ ] async document save (§9)
[ ] revision-aware save completion (§8)
[ ] streaming persistence with no second contiguous allocation (§10)
[ ] backpressure declared and tested for every producer (§4)
[ ] fair scheduling with aging and resource budgets (§5)
[ ] resource accounting per task (§6)
[ ] event-driven render loop (§11)
[ ] caret blinking with region invalidation (§12)
[ ] IME (§13)
[ ] large-paste optimization
```

**Stage 1.1 is complete only when the Stage 1 Definition of Done still passes
and the Stage 1.1 contracts pass.**

### 15.3 The chain

```text
Stage 1
   ↓ extends
Stage 1.1
   ↓ extends
Foundation
```

There is one active chain. No parallel definitions of done exist.

### 15.4 Required evidence

Stage 1.1 cannot be declared complete on a passing test suite alone. It requires:

```text
100 MB open      interactive thread never blocked; measured input latency during load
100 MB save      interactive thread never blocked; measured input latency during save
idle CPU         with caret blinking active, against the ≤2% contract
input latency    unchanged or better versus the Stage 1 baseline
memory           no regression against the Stage 1 workload measurements
IME              composition, commit, cancel and undo behaviour tested
```

The Stage 1 numbers in `docs/milestone-1-report.md` are the baseline for
"unchanged or better".

---

## 16. Open questions

Recorded rather than hidden:

1. **Admission under memory pressure.** `ResourceClass::Memory` is declared but
   the policy for refusing or deferring a task when memory is tight is not
   specified. Needs a real workload (indexing) to design against.
2. **Cancellation granularity for I/O.** Whether a cancelled 100 MB read stops
   at the next chunk boundary or at the next syscall is a platform-dependent
   detail that affects worst-case cancellation latency.
3. **Per-subsystem budgets.** §5 enforces resource budgets, but whether budgets
   are per-subsystem, per-workspace or global is deferred until more than one
   background subsystem exists.
4. **Save coalescing window.** §9 supersedes a queued save with a newer request;
   whether a short debounce should precede that is a tuning question.
