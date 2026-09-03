# ADR-0011: Clipboard

**Status:** Accepted (Stage 1)
**Date:** 2026-08-25

## Context

Specification section 9.4 puts OS clipboard interaction behind an interface that
the editor core talks to; section 20 limits Stage 1 to `read_text` and
`write_text`, with multi-format support out of scope; section 62 requires copy,
cut and paste through the platform abstraction.

## Decision

```rust
pub trait Clipboard: Send {
    fn read_text(&self) -> Result<String, PlatformError>;
    fn write_text(&self, text: &str) -> Result<(), PlatformError>;
}
```

Two implementations:

* `SystemClipboard` — Win32 `OpenClipboard`/`GetClipboardData`/`SetClipboardData`
  with `CF_UNICODETEXT`, wrapped in an RAII session guard so the clipboard is
  always closed, including on the error paths.
* `MemoryClipboard` — in-process, used by tests and as the fallback on platforms
  whose native clipboard is not implemented yet.

`EditorCore::with_clipboard` takes a `Box<dyn Clipboard>`, so the whole test
suite runs against the in-process one and never touches the machine's real
clipboard.

Pasted text is normalized through the same line-ending pipeline as a file
(ADR-0009), so pasting CRLF content into an LF document does not smuggle
carriage returns into the buffer.

## Alternatives

**Use the `arboard` crate.** Mature and cross-platform, and it would have been a
reasonable choice. Rejected because the Win32 text clipboard is about 60 lines
behind an interface we need anyway, and the dependency would sit in the same
platform layer that already exists for dialogs, atomic replace and process
statistics.

**Talk to the clipboard from the shell.** Would put an editor action outside the
command registry and make copy/cut/paste untestable headlessly.

**Retry forever when the clipboard is busy.** The clipboard is a single global
resource another process can hold. Retrying without a bound would let a
misbehaving application freeze the editor on Ctrl+C.

## Reasoning

The clipboard can genuinely fail — another process holds it, or the allocation
fails — so the interface returns `Result` and the shell reports the failure in
the status bar rather than pretending the copy worked. Opening is retried six
times at 1 ms, which covers the ordinary contention window and caps the worst
case at ~6 ms.

An empty clipboard, or one holding a non-text format, reads as an empty string
rather than an error: there is nothing wrong, there is just nothing to paste.

## Consequences

* Copy and cut are disabled (via the command registry's `enabled` predicate)
  when the selection is empty, so the clipboard is never cleared by accident.
* Only text is supported. Copying from LightSpeed into an application expecting
  rich text gives plain text, which is the documented Stage 1 behaviour.
* The system clipboard test tolerates a busy clipboard rather than failing CI
  for something outside the test's control.

## Reconsideration criteria

* Add formats (HTML, RTF, file lists) only when a feature needs them; the trait
  grows a method rather than changing shape.
* Implement the macOS and Linux backends behind the same trait when those
  platforms are targeted.
