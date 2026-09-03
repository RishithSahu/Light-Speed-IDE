# LightSpeed Foundation Specification

## 0. Purpose

This document defines the engineering contract for the first implementation of **LightSpeed IDE**.

LightSpeed is a native, lightweight code editor whose primary engineering property is:

> **Interactive editing remains responsive and predictable while background work is occurring.**

This document defines:

* implementation language;
* UI and rendering stack;
* supported platforms;
* system boundaries;
* data ownership;
* interface contracts;
* state machines;
* event semantics;
* error semantics;
* thread/process ownership;
* scheduling;
* backpressure;
* persistence;
* encoding and line-ending semantics;
* performance;
* memory/resource accounting;
* security;
* testing;
* benchmarking;
* Stage 1 implementation.

Future functionality must integrate through these contracts.

---

# 1. Product Definition

## 1.1 Current product

The current product is a lightweight native desktop code editor.

The foundation will eventually provide:

```text
project/workspace
file tree
file open/save
tabs
text editing
cursor
selection
undo/redo
clipboard
search
terminal
syntax highlighting
language diagnostics
basic Git
performance instrumentation
```

The editor's first engineering goal is not feature count.

It is:

```text
correctness
+
low latency
+
bounded memory
+
predictable background behavior
```

---

# 2. Long-Term Vision

After the foundation is stable, LightSpeed may add:

```text
Project Time Machine
10-million-file repository mode
Automatic performance-regression diagnosis
Global symbol/reference graph
Safe delete/change-impact analysis
Transactional toolchain/version switching
Environment compatibility analysis
Advanced Git visualization
AI
```

These are future systems.

They must not become implicit dependencies of the editor core.

---

# 3. Implementation Language

## 3.1 Primary language: Rust

The LightSpeed application core is implemented in:

```text
Rust
```

Rust is the foundation language for:

```text
editor core
TextBuffer
workspace management
filesystem operations
search
scheduler
background workers
process management
Git integration
performance instrumentation
resource accounting
future indexing infrastructure
```

The reason is architectural rather than stylistic:

* explicit ownership;
* safe concurrency;
* low-level memory control;
* predictable resource usage;
* native execution;
* straightforward process/thread management.

No Python or JavaScript runtime is required for the LightSpeed core.

Future integrations may use external processes or language-specific tooling, but the core editor remains Rust.

---

# 4. UI and Rendering Technology

## 4.1 Application model

LightSpeed is a native desktop application.

The UI/rendering stack for the foundation is:

```text
Rust
+
winit
+
wgpu
```

Use:

* `winit` for native window/event integration;
* `wgpu` for GPU-backed rendering;
* a dedicated text shaping/rasterization layer for text rendering.

The exact text shaping implementation must be selected and documented in ADR-0002 before text rendering is finalized.

Candidates may include a mature Rust text-stack based on:

```text
Unicode segmentation
text shaping
glyph rasterization
```

The implementation must support:

* Unicode;
* bidirectional text where applicable;
* variable-width glyphs;
* font fallback;
* cursor positioning;
* selection rendering.

Do not use HTML/CSS as the primary editor rendering layer.

The editor must not depend on a browser engine for its core UI.

---

# 5. Platform Targets

## 5.1 Foundation platform

The first supported platform is:

```text
Windows 11 x64
```

This is the primary development and benchmark platform.

The architecture must avoid deliberately Windows-specific assumptions where practical, because macOS/Linux support is a future goal.

## 5.2 Future platforms

Future target:

```text
Windows
macOS
Linux
```

However, cross-platform implementation is not a Stage 1 requirement.

---

# 6. Platform Abstraction Boundary

Platform-specific behavior must be isolated behind services for:

```text
filesystem
windowing
clipboard
process execution
file watching
path handling
atomic replacement
terminal
```

The rest of LightSpeed must not contain scattered:

```text
if Windows
if Linux
if macOS
```

branches.

Use platform abstractions.

---

# 7. Path Semantics

Internally, filesystem paths are represented as native filesystem paths rather than arbitrary strings.

Rules:

```text
canonical identity
≠
display path
```

A `Document` uses a canonical path for identity.

The UI may display a shorter workspace-relative path.

On Windows:

* path comparison must account for filesystem case behavior;
* drive letters must be handled correctly;
* UNC paths must be handled correctly;
* separators must not be hardcoded into higher-level logic.

Future platforms must be able to supply their own path semantics through the workspace layer.

---

# 8. System Boundaries

```text
                         LIGHTSPEED
                              │
       ┌──────────────────────┼──────────────────────┐
       │                      │                      │
       ▼                      ▼                      ▼
 Application              Editor Core            Renderer
       │                      │                      │
       ▼                      │                      │
 Command System              │                      │
       │                      │                      │
       └──────────────┬───────┴──────┬───────────────┘
                      │              │
                      ▼              ▼
                 Workspace        Clipboard
                      │
       ┌──────────────┼──────────────┐
       ▼              ▼              ▼
    Search           Git          Language
       │              │              │
       └──────────────┼──────────────┘
                      ▼
                Task Scheduler
                      │
            ┌─────────┴─────────┐
            ▼                   ▼
        Workers             Processes
```

Subsystems communicate only through public contracts.

Private state is never accessed directly across subsystem boundaries.

---

# 9. Core Ownership

## 9.1 Editor Core

Owns:

```text
Document
TextBuffer
Cursor
Selection
EditHistory
DocumentRevision
```

Only the Editor Core may mutate document content.

---

## 9.2 Workspace

Owns:

```text
workspace root
directory metadata
filesystem observation
file persistence
file metadata
```

Workspace does not own unsaved editor content.

---

## 9.3 Renderer

Owns:

```text
viewport
layout
render caches
GPU resources
frame scheduling
presentation state
```

Renderer cannot mutate editor state.

---

## 9.4 Clipboard

Owns OS clipboard interaction.

The Editor Core interacts through a clipboard interface.

Clipboard implementation is platform-specific.

---

# 10. Configuration System

Configuration is a proper subsystem.

## 10.1 Configuration model

```text
Configuration
├── schema_version
├── editor
├── appearance
├── keybindings
├── terminal
└── performance
```

Configuration has three conceptual layers:

```text
defaults
    ↓
user configuration
    ↓
workspace configuration
```

Higher-priority configuration overrides lower-priority configuration.

The configuration system produces an immutable effective configuration snapshot.

---

## 10.2 Configuration rules

Configuration loading must:

* validate types;
* validate supported values;
* reject malformed configuration clearly;
* provide defaults;
* avoid executing configuration code.

Configuration changes produce:

```text
ConfigurationChanged
```

Only affected subsystems need to react.

---

# 11. Logging System

Logging is a separate subsystem.

## 11.1 Log levels

```text
TRACE
DEBUG
INFO
WARN
ERROR
```

## 11.2 Structured record

```text
LogRecord
├── timestamp
├── level
├── subsystem
├── event
├── message
└── fields
```

## 11.3 Security rule

Logs must never automatically capture:

```text
environment secrets
tokens
passwords
terminal passwords
private keys
authentication cookies
```

File contents must not be logged by default.

A subsystem may explicitly log a safe diagnostic summary.

---

# 12. Command Registry

Commands are first-class application actions.

Example:

```text
OpenFile
Save
SaveAs
Undo
Redo
Copy
Cut
Paste
Find
FindInWorkspace
CloseTab
NextTab
ToggleTerminal
GitStatus
```

A command contains:

```text
Command
├── id
├── display_name
├── enabled_predicate
└── execute()
```

Commands may be triggered by:

```text
keyboard
mouse
menu
command palette
future automation
future AI
```

The command registry is the single routing mechanism.

UI components must not directly implement application-wide actions.

---

# 13. Data Model

## 13.1 Workspace

```text
Workspace
├── workspace_id
├── root_path
├── project_metadata
├── open_documents
└── repository_handle
```

---

# 14. Document

```text
Document
├── document_id
├── canonical_path
├── TextBuffer
├── encoding
├── line_ending
├── language
├── content_revision
├── undo_history
├── content_state
├── external_state
└── persistence_state
```

---

# 15. Text Position Semantics

The system explicitly distinguishes:

```text
ByteOffset
UnicodeScalarOffset
GraphemeIndex
LineIndex
DisplayColumn
```

Never assume:

```text
1 byte = 1 character
1 character = 1 display column
```

---

# 16. TextBuffer Representation

The TextBuffer must support:

```text
insert
delete
replace
range access
line access
offset conversion
```

The implementation must support incremental edits without rebuilding the entire document.

Candidate representations:

```text
piece table
rope
gap buffer
```

The final choice is benchmark-driven and recorded in ADR-0001.

---

# 17. Unicode Semantics

Cursor movement and selection use grapheme-aware behavior.

TextBuffer offsets remain independent from display columns.

Language services receive positions through an explicit conversion layer.

Unicode handling must be tested with:

```text
ASCII
accented characters
emoji
combining marks
CJK
right-to-left text
mixed-width text
```

---

# 18. Line Ending Semantics

Line endings are part of document metadata.

Supported:

```text
LF
CRLF
CR
```

The document stores its current line-ending policy:

```text
LineEnding
├── LF
├── CRLF
└── CR
```

When opening a file:

```text
bytes
 ↓
decode
 ↓
detect line endings
 ↓
normalize internally
 ↓
TextBuffer
```

The internal TextBuffer uses one canonical newline representation.

When saving:

```text
TextBuffer
 ↓
selected LineEnding
 ↓
encoded bytes
```

The original line-ending style is preserved unless explicitly changed by the user or configuration.

Mixed line endings must be detected.

Default behavior:

```text
preserve majority / detected style
```

and report mixed-line-ending state.

---

# 19. Encoding and Binary Handling

Loading:

```text
bytes
 ↓
binary detection
 ↓
encoding detection
 ↓
decode
 ↓
TextBuffer
```

Support initially:

```text
UTF-8
UTF-8 BOM
UTF-16 LE/BE
```

Unknown encodings must produce an explicit warning/failure rather than silent corruption.

Binary files are not normal editable text documents.

---

# 20. Clipboard

Stage 1 supports:

```text
copy
cut
paste
```

Clipboard contents are treated as text for normal editor operations.

Multi-format clipboard support is out of scope for the first Stage 1 implementation.

Clipboard API:

```text
read_text()
write_text(text)
```

The platform layer implements the actual OS clipboard interaction.

---

# 21. Multi-Cursor Policy

Multi-cursor editing is explicitly:

```text
Future / Stage 2+
```

However, the editing architecture must not prevent it.

The editor should represent selection/edit operations in a way that can later support:

```text
SelectionSet
```

rather than assuming a document can have only one cursor forever.

Do not implement multi-cursor in Stage 1.

---

# 22. Document Revision Semantics

Every successful content mutation increments:

```text
content_revision
```

Example:

```text
v0
 ↓ edit
v1
 ↓ edit
v2
 ↓ undo
v3
```

Undo does not move backward numerically.

The revision identifies the exact content version observed by asynchronous systems.

---

# 23. Undo Coalescing

Undo history must group adjacent compatible edits.

For example, typing:

```text
hello
```

should normally produce a single undo step rather than five separate undo operations.

A coalescing group continues while:

```text
same document
same typing mode
no selection interruption
no cursor discontinuity
no command boundary
within coalescing time window
```

A configurable initial coalescing window:

```text
500 ms
```

is the starting implementation target.

The exact value is benchmark/user-testing driven and must not be treated as an architectural invariant.

Operations that force a boundary include:

```text
cursor jump
selection change
paste
delete-selection
command execution
save
undo
redo
```

Undo coalescing affects edit history only.

It does not alter document revision semantics.

---

# 24. Open Document Contract

```text
open_document(path: Path) -> Result<DocumentHandle, OpenDocumentError>
```

Responsibilities:

```text
canonicalize path
obtain file bytes from Workspace
detect binary
detect encoding
detect line endings
construct TextBuffer
set content_revision = 0
initialize edit history
```

It must not:

```text
spawn processes
run Git
run language analysis
scan project
perform search
render UI
```

Two canonical references to the same file within one workspace must resolve to the same document identity.

---

# 25. Document State Separation

Document state is three independent dimensions.

## ContentState

```text
Clean
Dirty
```

## ExternalState

```text
Unchanged
ExternallyChanged
Missing
Conflict
```

## PersistenceState

```text
Idle
Saving
SaveSucceeded
SaveFailed
```

These states must not be collapsed into one enum.

---

# 26. RenderSnapshot

`RenderSnapshot` means:

> An immutable presentation snapshot for one rendering update.

It does not represent:

* the complete document;
* persistence;
* undo history;
* workspace state.

```text
RenderSnapshot
├── document_id
├── content_revision
├── viewport
├── visible text/layout
├── cursor presentation
├── selection presentation
├── diagnostics
└── decorations
```

Once published to the renderer:

```text
immutable
```

---

# 27. Render Pipeline

```text
Editor State
    ↓
Dirty Region Tracking
    ↓
Build RenderSnapshot
    ↓
Publish
    ↓
GPU Renderer
    ↓
Frame
```

The renderer never directly reads mutable editor state.

---

# 28. Rendering Invalidation

Examples:

```text
TextChanged
    → affected line/layout regions

CursorChanged
    → old/new cursor regions

SelectionChanged
    → affected selection regions

ViewportChanged
    → visible viewport

DiagnosticsChanged
    → diagnostic overlays
```

No operation invalidates the entire document unless required for correctness.

---

# 29. Persistence

Saving uses:

```text
encode
 ↓
temporary file
 ↓
write
 ↓
flush
 ↓
fsync where required
 ↓
atomic replace
```

The original file remains intact until replacement succeeds.

Platform-specific atomic replacement behavior is isolated behind Workspace.

---

# 30. Interface Contracts

## EditorCore

```rust
fn open_document(path: &Path)
    -> Result<DocumentHandle, OpenDocumentError>;

fn insert(
    document: DocumentId,
    position: Position,
    text: &str
) -> Result<EditResult, EditorError>;

fn delete(
    document: DocumentId,
    range: Range
) -> Result<EditResult, EditorError>;

fn undo(
    document: DocumentId
) -> Result<EditResult, EditorError>;

fn redo(
    document: DocumentId
) -> Result<EditResult, EditorError>;
```

Exact Rust types may be refined during Stage 1, but ownership and semantics must remain unchanged.

---

# 31. Workspace Contract

```rust
fn read_file(path: &Path)
    -> Result<FileBytes, WorkspaceError>;

fn write_file_atomic(
    path: &Path,
    contents: &[u8]
) -> Result<(), PersistenceError>;

fn enumerate_children(
    directory: &Path
) -> Result<Vec<FileEntry>, WorkspaceError>;
```

`enumerate_children()` is lazy and only enumerates one directory level.

Recursive traversal belongs to a scheduler-managed background task.

---

# 32. Search Contract

```rust
fn submit(
    request: SearchRequest
) -> TaskId;

fn cancel(
    task: TaskId
);
```

Every result includes:

```text
request_id
document_id where applicable
content_revision where applicable
```

---

# 33. Scheduler Contract

```rust
fn submit(task: TaskSpec) -> TaskId;

fn cancel(task: TaskId);

fn pause(task: TaskId);

fn resume(task: TaskId);
```

No subsystem may directly create background workers.

---

# 34. Language Contract

Stage 1 does not implement full language services.

The interface is reserved for:

```rust
fn detect_language(path: &Path) -> Language;

fn analyze(
    document: DocumentSnapshot
) -> TaskId;
```

Syntax highlighting belongs to the language layer, not the editor core.

---

# 35. Git Contract

The Git interface is future Stage 2 functionality.

The boundary is:

```rust
fn status() -> Result<GitStatus, GitError>;
fn branch() -> Result<Branch, GitError>;
fn diff() -> Result<Diff, GitError>;
fn log() -> Result<Vec<Commit>, GitError>;
```

Git does not own editor content.

---

# 36. Terminal Contract

```rust
fn spawn(
    command: CommandSpec
) -> Result<ProcessId, TerminalError>;

fn write(
    process: ProcessId,
    input: &[u8]
) -> Result<(), TerminalError>;

fn terminate(
    process: ProcessId
) -> Result<(), TerminalError>;
```

Terminal processes are external.

---

# 37. Event Model

Events are immutable facts.

Core events:

```text
WorkspaceOpened
WorkspaceClosed

DocumentOpened
DocumentEdited
DocumentSaved
DocumentSaveFailed
DocumentExternallyChanged
DocumentConflictDetected

CursorChanged
SelectionChanged
ViewportChanged

RenderSnapshotPublished

SearchSubmitted
SearchCompleted
SearchCancelled
SearchFailed

GitStatusChanged
GitOperationCompleted
GitOperationFailed

TaskQueued
TaskStarted
TaskCancelled
TaskCompleted
TaskFailed

PerformanceBudgetExceeded
ResourceBudgetExceeded
```

Events contain:

```text
event_id
timestamp
source
payload
```

Events must not secretly perform actions.

---

# 38. State Machines

## Document

```text
Closed
  ↓
Opening
  ↓
Open
  │
  ├── edit → Dirty
  ├── save → Saving
  ├── external change → ExternallyChanged
  └── close → Closed
```

Persistence, content and external state remain independent dimensions.

---

## Background Task

```text
Created
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

---

## Terminal

```text
Created
 ↓
Starting
 ↓
Running
 ├── Exited
 ├── Failed
 └── Terminated
```

---

# 39. Error Model

Typed errors:

```text
EditorError
WorkspaceError
EncodingError
PersistenceError
SearchError
GitError
LanguageServiceError
TerminalError
SchedulerError
ResourceError
SecurityError
```

Each error contains:

```text
code
message
subsystem
cause
recoverability
```

Recoverability:

```text
Recoverable
Retryable
UserActionRequired
FatalToSubsystem
```

Cancellation is normal control flow, not an error.

---

# 40. Thread Ownership

## Interactive thread

Owns:

```text
input
commands
bounded editor operations
render snapshot creation
```

It must not:

```text
perform recursive filesystem scanning
run Git commands
run search
spawn arbitrary workers
run external processes
perform long parsing
```

## Scheduler workers

Own background work.

## External processes

Own:

```text
terminal
future LSP
future AI
external tools
```

---

# 41. Scheduler Admission Rule

Absolute rule:

> **Every non-interactive operation must pass through Scheduler admission before execution.**

No direct:

```text
thread::spawn
rayon pool
tokio runtime
executor
```

creation is allowed outside Scheduler-owned infrastructure.

If an operation is small and synchronous enough to run interactively, it must still remain within an explicitly measured interactive budget.

---

# 42. Scheduler Enforcement

Architectural enforcement requires:

1. scheduler owns worker creation;
2. worker creation APIs are hidden from other modules where possible;
3. CI architecture tests scan for prohibited thread/executor creation outside approved modules;
4. code review treats scheduler bypass as an architecture violation.

This is a hard invariant.

---

# 43. Backpressure

Every producer/consumer path must be bounded.

Allowed:

```text
bounded queue
coalescing
cancellation
stale-result dropping
producer throttling
```

Examples:

```text
filesystem events → coalesce
search → cancel stale
terminal output → bounded history
LSP → stale-result rejection
```

---

# 44. Fair Scheduling

The Scheduler considers:

```text
base priority
queue age
resource budget
deadline pressure
```

Conceptually:

```text
effective_priority =
    base_priority
    + aging
    + deadline_pressure
```

Interactive work has highest practical priority.

Lower-priority background work must not be permanently starved while resources are available.

---

# 45. Resource Accounting

Every Scheduler task records:

```text
task_id
subsystem
workspace
queue_wait_time
wall_time
CPU_time
bytes_read
bytes_written
peak_memory where measurable
```

The same accounting API will later feed the automatic performance-regression system.

---

# 46. Failure Isolation

Failure in:

```text
Git
Search
Language server
Terminal
Future AI
Indexing
```

must not terminate the editor core.

The editor must be able to degrade gracefully.

---

# 47. Security Contracts

Opening a project does not automatically execute code.

Process execution occurs only through explicit process APIs.

Workspace-relative filesystem operations must validate paths.

Logs must not capture secrets.

Binary files must not be automatically converted to text.

Future plugins and AI processes must run behind explicit trust/process boundaries.

---

# 48. Performance Contracts

Use:

```text
Target
Failure Threshold
```

not “maximum acceptable P95.”

## Interactive

| Operation                 | Target P95 | Failure Threshold P95 |
| ------------------------- | ---------: | --------------------: |
| input → editor state      |     ≤ 2 ms |                > 5 ms |
| input → rendered response |     ≤ 8 ms |               > 16 ms |
| cursor movement           |     ≤ 4 ms |               > 10 ms |
| selection movement        |     ≤ 4 ms |               > 10 ms |
| loaded-tab switch         |     ≤ 2 ms |                > 5 ms |
| small-file open           |    ≤ 20 ms |               > 50 ms |

---

# 49. Startup Contract

Target:

```text
cold start → usable editor ≤ 500 ms P95
```

Failure threshold:

```text
> 1 second P95
```

Measure separately:

```text
process startup
window creation
first frame
usable editor
```

---

# 50. Workload-Linked Memory Contracts

## W1 — Empty editor

```text
no workspace
no files
no background tasks
```

Target:

```text
RSS ≤ 120 MB
```

Failure threshold:

```text
RSS > 160 MB
```

## W2 — Small project

```text
1,000 files
10 MB source
5 open documents
```

Target:

```text
RSS ≤ 180 MB
```

Failure threshold:

```text
RSS > 250 MB
```

## W3 — Medium project

```text
50,000 files
~250 MB source
20 open documents
```

Target:

```text
RSS ≤ 300 MB
```

Failure threshold:

```text
RSS > 450 MB
```

Larger workloads initially report measurements without an arbitrary hard threshold until benchmark data establishes an appropriate contract.

---

# 51. CPU Contract

Idle target:

```text
≤ 2% average CPU
```

The CPU contract is subordinate to interactive latency.

If background work causes the input/render failure threshold to be exceeded:

```text
background work must be throttled, deferred or cancelled
```

---

# 52. Benchmark Workloads

Workloads are version-controlled.

```text
workloads/
├── W1_empty
├── W2_small
├── W3_medium
├── W4_large
└── W5_extreme
```

Each workload defines:

```text
file count
total bytes
file size distribution
directory depth
language distribution
Git state
number of open documents
```

---

# 53. Adversarial Workloads

```text
A1 typing while search runs
A2 typing while Git analysis runs
A3 typing while indexing runs
A4 rapid search cancellation
A5 filesystem event storm
A6 huge file editing
A7 terminal output flood
A8 LSP failure
A9 save failure
A10 workspace repeatedly opened and closed
```

The architecture must preserve interactive contracts under these workloads.

---

# 54. Correctness Tests

```text
tests/
├── unit/
├── integration/
├── architecture/
└── regression/
```

Core tests cover:

```text
TextBuffer
Unicode
line endings
encoding
binary detection
cursor
selection
undo/redo
revision semantics
open_document()
persistence
atomic save
search
stale results
workspace
scheduler
events
state machines
```

---

# 55. Architecture Tests

CI must verify:

```text
renderer cannot mutate EditorCore
worker creation occurs only through Scheduler
non-interactive operations cannot execute outside Scheduler
RenderSnapshot is immutable
queues are bounded
workspace traversal is lazy
document revision increments correctly
undo does not decrement revision
line ending metadata is preserved
save uses atomic persistence layer
```

---

# 56. Benchmark Metrics

Every workload records:

```text
P50
P95
P99
max latency
RSS
CPU
I/O
queue depth
queue wait time
task runtime
cancellations
stale results
```

Benchmark results must include:

```text
application version
workload version
OS
CPU
RAM
GPU
build configuration
```

---

# 57. Architecture Decision Records

Create:

```text
docs/adr/
```

Required ADRs:

```text
ADR-0001 TextBuffer representation
ADR-0002 UI/rendering stack
ADR-0003 Thread ownership
ADR-0004 Scheduler
ADR-0005 Backpressure
ADR-0006 Fair scheduling
ADR-0007 RenderSnapshot
ADR-0008 Persistence
ADR-0009 Encoding and line endings
ADR-0010 Workspace/path semantics
ADR-0011 Clipboard
```

Every ADR contains:

```text
Status
Context
Decision
Alternatives
Reasoning
Consequences
Benchmark evidence
Reconsideration criteria
```

---

# 58. Stage 1 Implementation Plan

Stage 1 is strictly the editor core.

It contains:

```text
application shell
configuration
logging
command registry

TextBuffer
cursor
selection
clipboard
undo/redo
document revision
encoding
line endings

document open
document save
atomic persistence

viewport
RenderSnapshot
renderer
tabs
```

Stage 1 explicitly excludes:

```text
syntax highlighting
workspace-wide search
Git
terminal
LSP
background indexing
advanced scheduler
AI
Time Machine
global symbol graph
toolchain migration
```

These belong to the Foundation Stage or later.

---

# 59. Stage 1.1 — Application Shell

Implement:

```text
native window
application lifecycle
configuration loading
logging
command registry
```

Create:

```text
ADR-0002 UI/rendering stack
```

before finalizing renderer implementation.

---

# 60. Stage 1.2 — TextBuffer

Implement and benchmark candidate representations.

Requirements:

```text
insert
delete
replace
range lookup
line lookup
offset conversion
Unicode semantics
line-ending normalization
```

Select final representation using representative workloads.

Record the decision in ADR-0001.

---

# 61. Stage 1.3 — Cursor / Selection

Implement:

```text
character movement
line movement
word movement
selection
delete-selection
```

Test:

```text
ASCII
Unicode
emoji
combining marks
CJK
RTL
mixed-width text
```

---

# 62. Stage 1.4 — Clipboard

Implement:

```text
copy
cut
paste
```

Text clipboard only.

Use platform abstraction.

Multi-cursor editing is deferred.

---

# 63. Stage 1.5 — Undo/Redo

Implement:

```text
undo stack
redo stack
coalescing
revision increment
cursor restoration
selection restoration
```

Verify:

```text
normal edits
typing groups
paste
delete-selection
cursor jumps
undo
redo
```

---

# 64. Stage 1.6 — Document Open

Implement:

```text
canonical path
read bytes
binary detection
encoding detection
line-ending detection
TextBuffer creation
revision = 0
```

No indexing.

No syntax analysis.

No Git.

No subprocess.

---

# 65. Stage 1.7 — Document Save

Implement:

```text
encode
line-ending conversion
temporary file
flush
fsync where appropriate
atomic replacement
state transition
```

Test failed/interrupted saves.

---

# 66. Stage 1.8 — Viewport and RenderSnapshot

Implement:

```text
viewport
visible region
dirty regions
immutable RenderSnapshot
```

The renderer receives snapshots rather than reading mutable editor state.

---

# 67. Stage 1.9 — Basic Rendering

Render only:

```text
text
cursor
selection
line numbers
scrolling
```

No syntax highlighting.

No diagnostics.

No Git decorations.

No minimap.

No semantic code graph.

---

# 68. Stage 1.10 — Tabs

Implement:

```text
open tab
switch tab
close tab
dirty indicator
save
```

Tabs reference Documents rather than copying document contents.

---

# 69. Stage 1.11 — Stage 1 Tests

All Stage 1 correctness tests must pass.

Architecture tests must pass.

Persistence tests must pass.

Unicode/line-ending tests must pass.

---

# 70. Stage 1.12 — Stage 1 Benchmarks

Run:

```text
startup
typing
large-file editing
cursor movement
selection
undo/redo
scrolling
tab switching
open
save
Unicode-heavy editing
```

Measure:

```text
P50
P95
P99
RSS
CPU
```

---

# 71. Stage 1 Definition of Done

Stage 1 passes only when:

```text
[ ] native application starts
[ ] configuration works
[ ] logging works
[ ] command registry works
[ ] TextBuffer works
[ ] Unicode semantics pass
[ ] line-ending semantics pass
[ ] cursor works
[ ] selection works
[ ] clipboard works
[ ] undo/redo works
[ ] undo coalescing works
[ ] document revision semantics pass
[ ] open_document() contract passes
[ ] encoding detection works
[ ] binary handling works
[ ] atomic persistence works
[ ] save-state model works
[ ] viewport works
[ ] RenderSnapshot works
[ ] renderer works
[ ] tabs work
[ ] architecture tests pass
[ ] correctness tests pass
[ ] benchmark workloads execute
[ ] performance contracts are measured
```

Only after this is satisfied may the Foundation Stage begin.

---

# 72. Foundation Stage After Stage 1

Foundation Stage adds:

```text
workspace
file tree
filesystem watcher
search
terminal
syntax highlighting
language services
Git
Scheduler
background workers
backpressure
fair scheduling
resource accounting
failure isolation
```

The Foundation Stage may not weaken the Stage 1 performance contracts.

---

# 73. Future Feature Admission Contract

Before implementing any future subsystem, define:

```text
System boundary
Data model
Interface
State machine
Events
Errors
Thread/process ownership
Persistence
Backpressure
Resource budget
Performance contract
Security contract
Tests
Benchmarks
ADR
```

No future feature is allowed to bypass these requirements.

---

# 74. Final Engineering Principle

LightSpeed is not trying to be a feature clone of another IDE.

Its architectural thesis is:

> **Keep the interactive critical path small, deterministic and protected. Push expensive work out of it, control that work, measure it, and make every future subsystem respect the same boundaries.**

The foundation therefore depends on:

```text
explicit ownership
explicit interfaces
explicit state machines
explicit events
typed failures
transactional persistence
Unicode-correct text semantics
bounded queues
scheduler admission
fair scheduling
cancellation
resource accounting
immutable render snapshots
workload-defined benchmarks
enforced architecture invariants
```

The first objective remains deliberately narrow:

> **Build the smallest correct native editor core that is measurably fast and architecturally difficult to damage.**
