# ADR-0009: Encoding and line endings

**Status:** Accepted (Stage 1)
**Date:** 2026-08-25

## Context

Specification section 19 requires binary detection, encoding detection and
decoding for UTF-8, UTF-8 with BOM and UTF-16 LE/BE, with unknown encodings
producing an explicit failure rather than silent corruption. Section 18 requires
LF, CRLF and CR to be supported, normalized internally, detected on open,
preserved on save, and reported when mixed.

## Decision

**Encoding.** Detection is by byte-order mark first, then validation:

```text
EF BB BF -> UTF-8 with BOM
FF FE    -> UTF-16 LE        (FF FE 00 00 is not special-cased; UTF-32 is unsupported)
FE FF    -> UTF-16 BE
otherwise -> valid UTF-8, or an error
```

Invalid UTF-8 is an `EncodingError`, never a lossy conversion. The document
remembers its encoding and writes it back unchanged, BOM included.

**Binary detection.** A NUL byte in the first 8 KB means binary. Binary files are
refused with a typed error and never opened as text.

**Line endings.** The buffer stores `\n` only. On open, the raw text is scanned,
each style is counted, the majority becomes the document's `LineEnding`, and a
`mixed` flag records that more than one style was present. On save, the document's
own style is written back. A file with no line breaks at all takes the platform
default (CRLF on Windows).

## Alternatives

**Guess encodings statistically (chardet-style).** More permissive, and wrong
in the way that matters: a mis-guess corrupts the file on save. An explicit
failure lets the user convert the file deliberately.

**Normalize line endings to LF on save.** Would quietly rewrite every line of a
CRLF file the first time it is touched, producing a diff of the whole file.

**Keep CRLF in the buffer.** Every offset calculation, every cursor movement and
every grapheme query would have to know that two characters can be one line
break. Normalizing once at the boundary keeps that complexity out of the core.

**Round-trip a lossy decode.** Rejected: silent corruption is the failure mode
section 19 explicitly forbids.

## Reasoning

Normalizing at the edges gives the core exactly one representation to reason
about while giving the file exactly the bytes it had. Both directions are tested
as round trips: a CRLF file edited and saved stays CRLF; a UTF-16 LE file with a
BOM comes back byte-identical; a file with no trailing newline does not grow one.

Detecting mixed line endings without rewriting them respects the specification's
"report, do not silently normalize" rule — the status bar shows `CRLF (mixed)`.

## Consequences

* UTF-16 documents are decoded to UTF-8 in memory and re-encoded on save, so a
  UTF-16 file costs roughly half its on-disk size in the buffer (ASCII-heavy
  content) — the reverse of the usual overhead direction.
* Encodings beyond these four (Windows-1252, Shift-JIS, GB18030 and friends) are
  rejected rather than mangled. Adding them is a matter of a decoding table
  behind the same `Encoding` enum.
* Files with a NUL in the first 8 KB but text after it are treated as binary;
  this is the standard heuristic and its false-positive rate is acceptable.

## Benchmark evidence

The Unicode workload (1 MB, mixed scripts, emoji, combining marks, CJK, RTL)
opens in 5.04 ms P95 and types at 2.0-2.3 µs P95 — the multi-byte path costs
somewhat more per keystroke than the ASCII path, which is the cost of
character-boundary arithmetic, and remains three orders of magnitude under
budget.

## Reconsideration criteria

* Add legacy single-byte encodings if real files demand it; the enum and the
  save path already have the shape for it.
* Revisit the 8 KB binary window if a real text format is misdetected.
