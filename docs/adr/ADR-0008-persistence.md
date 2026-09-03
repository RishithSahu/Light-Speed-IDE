# ADR-0008: Persistence

**Status:** Accepted (Stage 1)
**Date:** 2026-08-25

## Context

Specification section 29 requires saving to be:

```text
encode -> temporary file -> write -> flush -> fsync where required -> atomic replace
```

with the original intact until the replacement succeeds, and platform-specific
replacement isolated behind the workspace. Section 25 requires persistence state
(`Idle`, `Saving`, `SaveSucceeded`, `SaveFailed`) to be tracked separately from
content state and external state. Section 65 requires failed and interrupted
saves to be tested.

## Decision

`ls_platform::fsops::write_file_atomic_with` implements the sequence once:

1. create a temporary file **in the target's own directory**, so the final step
   stays within one volume and therefore stays atomic;
2. stream the encoded document into it through a `BufWriter`;
3. `flush`, then `sync_all` — the bytes are on the device before anything is
   visible under the real name;
4. replace: `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)` on
   Windows, `rename` elsewhere;
5. on any failure, delete the temporary file and return a typed error; the
   original file has not been touched.

The core never writes a file itself. An architecture test scans `crates/core`
for `fs::write(` and `File::create(` and fails the build if either appears.

`Document::save` is a state machine: `mark_saving` → write → `mark_saved` (which
records the new disk stamp and clears dirty) or `mark_save_failed`.

## Alternatives

**Write in place.** A crash or a full disk mid-write leaves a truncated file —
losing the old contents *and* the new ones. Unacceptable for an editor.

**Temporary file in the system temp directory.** Would usually be a different
volume, which turns the atomic rename into a copy-and-delete with a window where
neither version is complete.

**Skip `fsync`.** Faster, and the usual argument is that the OS will get around
to it. But the failure mode is losing a save that the editor already reported as
succeeded, which is exactly the kind of lie this design exists to prevent. The
measured cost is 2.8-4.2 ms for a small file, which is affordable.

## Reasoning

Streaming rather than buffering the whole document matters at scale: a 100 MB
save writes through a `BufWriter` chunk by chunk instead of materializing a
second 100 MB buffer, so peak memory during a save stays close to the document's
own footprint.

Line-ending conversion happens during the same stream (ADR-0009), and because
only `\n` is rewritten and rope chunks never split a character, converting chunk
by chunk gives byte-identical output to converting the whole text at once — a
property with its own test.

## Consequences

* Saving is synchronous on the interactive thread (ADR-0003): 2.8 ms P95 for
  1 KB, 7.7 ms for 1 MB, 49 ms for 10 MB, 617 ms for 100 MB. Large saves block the
  frame loop, which is a documented Stage 1 limitation.
* File permissions and alternate data streams are not preserved through the
  replace — `MoveFileExW` keeps the source file's attributes, not the
  destination's. `ReplaceFileW` would preserve more and is the natural upgrade
  when it matters.
* Saving twice in a row produces identical bytes and leaves no temporary files;
  both are asserted in the regression suite.

## Benchmark evidence

`document.save` P95: 2.80 ms (1 KB), 4.21 ms (64 KB), 7.74 ms (1 MB), 49.34 ms
(10 MB), 616.77 ms (100 MB), 38.10 ms (10 MB single line).

## Reconsideration criteria

* Move saving behind scheduler admission when the scheduler exists, so a 100 MB
  save cannot block a frame.
* Switch to `ReplaceFileW` on Windows if preserving destination ACLs, creation
  time or alternate data streams turns out to matter.
