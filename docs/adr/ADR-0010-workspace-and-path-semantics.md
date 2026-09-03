# ADR-0010: Workspace and path semantics

**Status:** Accepted (Stage 1)
**Date:** 2026-08-25

## Context

Specification section 7 requires canonical identity to be distinct from display
path, with Windows case behaviour, drive letters and UNC paths handled
correctly, and no hardcoded separators in higher-level logic. Section 24
requires two canonical references to the same file to resolve to one document.
Section 31 defines the workspace contract: `read_file`, `write_file_atomic`,
`enumerate_children` — the last of which is explicitly lazy, one level only.

## Decision

`ls_platform::paths::CanonicalPath` carries two things:

* the **path** for display, with the Windows verbatim prefix removed
  (`\\?\C:\src` → `C:\src`, `\\?\UNC\server\share` → `\\server\share`);
* an **identity key**, case-folded on Windows, used for equality and hashing.

`EditorCore` keys open documents by that identity key, so opening `src\Main.rs`,
`SRC\MAIN.RS` and `src\.\Main.rs` all resolve to one document with one buffer
and one undo history.

`Workspace::enumerate_children` reads exactly one directory level and returns
sorted `FileEntry` values. Recursive traversal is deliberately absent; it
belongs to a scheduler-managed background task in the Foundation Stage, and an
architecture test asserts no recursive walker exists in the workspace.

## Alternatives

**Compare paths as strings.** Would open the same file twice under different
capitalization — two buffers, two undo stacks, and whichever saves last wins.
This is a real data-loss bug, not a cosmetic one.

**Case-sensitive comparison on Windows.** Technically more correct for NTFS
directories with per-directory case sensitivity enabled, but those are rare and
the failure mode of getting it wrong (two documents for one file) is far worse
than the failure mode of folding (one document for two genuinely distinct
files, which requires deliberately creating `Main.rs` and `main.rs` side by
side).

**Keep the `\\?\` prefix everywhere.** Correct for the filesystem, unreadable
for humans, and it leaks into window titles and status bars.

**Store paths as `String`.** Loses the platform's own path semantics and invites
separator assumptions.

## Reasoning

Canonicalization resolves symlinks, relative components and 8.3 short names, so
identity is a property of the file rather than of how it was addressed.
Splitting display from identity means the display half can be as friendly as we
like without weakening the half that prevents duplicate documents.

Save As targets do not exist yet, so `CanonicalPath::unverified` normalizes them
with `std::path::absolute` without touching the filesystem — the same identity
rules, without requiring the file to exist first.

## Consequences

* Two references to one file share a document (integration test), and a path
  outside a workspace root is reported as outside rather than silently
  relativized (`relative_to` returns `None`, with a test for the
  `/proj` vs `/project` prefix trap).
* Opening a file requires it to exist and be canonicalizable; a broken symlink
  produces a typed open error rather than an empty buffer.
* Case folding uses `to_lowercase`, which is Unicode-aware but not identical to
  Windows' own case table for rare scripts. Sufficient for filesystem identity
  in practice; revisit if a real path ever collides.

## Reconsideration criteria

* If per-directory case sensitivity on NTFS becomes common, query the actual
  directory flag rather than assuming case-insensitivity.
* When the file tree lands, `enumerate_children` becomes the leaf of a lazy
  expansion model; it must stay one level even then.
