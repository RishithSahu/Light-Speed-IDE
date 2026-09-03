# ADR-0014: IME architecture

**Status:** Accepted (Stage 1.1)
**Date:** 2026-08-25
**Adds:** a new contract via `docs/foundation-amendment-001.md` §13

## Question

How should native IME composition integrate with the editor's transactional edit
model?

## Context

Stage 1 handles `WindowEvent::KeyboardInput` and inserts the text the platform
produced. Composition events are not handled at all, so input methods that
compose — Chinese, Japanese, Korean, Vietnamese, and many others — cannot type
into the editor. Paste works; typing does not. This is the largest functional
gap in the Stage 1 shell and it affects a large fraction of the world's
developers.

The design pressure is that composition is *provisional*. Text under composition
is not yet what the user meant; it changes as they refine it, and it may be
abandoned entirely. The editor core, meanwhile, is built on the premise that
every content change is a numbered revision with an undo entry.

Naively inserting preedit text into the buffer would mean:

* a revision per keystroke of a composition that has not been committed;
* undo history full of partial compositions;
* language services (Foundation Stage) analysing text the user never wrote;
* a dirty document produced by a composition the user then cancelled.

## Alternatives

**1. Insert preedit into the buffer and replace it on each update.** Simplest to
draw, because preedit is just text. Rejected for all four reasons above; it
corrupts revision semantics to save presentation work.

**2. Insert preedit into the buffer but suppress revisions/undo for it.** Keeps
drawing simple and avoids polluting undo, but creates a buffer state that is not
a revision — every invariant that says "the buffer is at revision N" gains an
exception, and every future consumer has to know about it.

**3. Keep preedit entirely out of the buffer, carry it in presentation state.**
Composition lives in `ImeState`, travels in the render snapshot, and is drawn
with composition styling. The buffer sees exactly one edit, at commit.

## Decision

**Adopt alternative 3.**

### Data model

```rust
struct ImeState {
    enabled: bool,
    preedit: Option<Preedit>,
    cursor: Option<usize>,
    selection: Option<Range>,
}
```

### State machine

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

### Events

```text
ImeEnabled
ImePreeditChanged
ImeCommitted
ImeCancelled
ImeDisabled
```

### Error model

`ImeError` distinguishes:

```text
unsupported platform behavior          capability gap; degrade, do not fail
window rejected IME configuration      recoverable; retry or report
invalid composition coordinates        our bug; log and correct
```

Collapsing these into one error would hide the third behind the first.

### Thread ownership

IME events arrive on the interactive/UI thread and are handled there.
**Composition does not involve the scheduler** (amendment §13.5): it is the most
latency-sensitive path in the editor, and admission would add queueing to a
keystroke.

### Editor semantics

* Preedit is **temporary presentation state** and never enters the
  `TextBuffer`.
* Preedit does not change `content_revision` and produces no undo entry.
* On commit, the composed text enters as **one logical `Edit` transaction** —
  one revision, one undo step. Committing a single CJK character built from
  three keystrokes is one undo, not three.
* Cancel is a no-op on the document: buffer, revision and history are untouched.
* A composition that starts with a selection active replaces that selection at
  commit, as a single transaction.

## Consequences

* `RenderSnapshot` carries preedit (text, cursor within it, and the underline or
  highlight ranges the platform reports), so the renderer can draw composition
  styling without consulting editor state.
* The shell must report the caret's screen rectangle back to the platform so the
  IME can position its candidate window. Getting that wrong is the "invalid
  composition coordinates" error above.
* Word movement, selection and undo see committed text only, which is what makes
  their existing tests remain valid.
* The interactive thread gains a small amount of state that must be reset on
  focus loss, document switch and tab close.

## Testing (required)

Explicit tests, per amendment §13.6:

```text
preedit does not change content_revision
preedit does not create an undo entry
commit produces exactly one transaction
undo after commit removes the whole committed string
cancel leaves buffer, revision and history untouched
commit over an active selection replaces it in one transaction
focus loss during composition does not leave orphaned preedit state
```

These are core-level tests and must run headless, like the rest of the editor
core's suite. Platform-level composition sequences are exercised separately.

## Reconsideration criteria

* If a platform's IME cannot be driven without inserting provisional text into
  the document, revisit alternative 2 — but with the exception documented as a
  platform quirk rather than as the general model.
* Windows-specific behaviour beyond winit's IME events (full TSF integration)
  may become necessary for some input methods; that is a platform-layer change
  behind this same contract, not a change to the contract.
