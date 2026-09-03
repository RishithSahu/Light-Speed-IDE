# ADR-0002: UI and rendering stack

**Status:** Accepted (Stage 1)
**Date:** 2026-08-25

## Context

Specification section 4.1 fixes the application model: a native desktop
application built on `winit` + `wgpu`, with a dedicated text shaping and
rasterization layer that must be selected and documented in this ADR before text
rendering is finalized. It rules out HTML/CSS as the editor's rendering layer and
forbids depending on a browser engine.

The text layer must support Unicode, bidirectional text where applicable,
variable-width glyphs, font fallback, cursor positioning and selection
rendering. Frame budget is 8 ms P95 from input to presented frame.

## Decision

```text
winit 0.30      window, input, DPI
wgpu 30         GPU device, surface, render passes
glyphon 0.12    glyph atlas + wgpu text renderer
  cosmic-text   segmentation, shaping (rustybuzz), bidi, font fallback (fontdb)
```

Two pipelines draw everything:

1. **Quads** (`app/src/quads.rs`) — one instanced draw call for every rectangle
   in the window: panel backgrounds, caret, selection highlights, tab plates,
   scrollbar. ~40 instances for a typical frame.
2. **Text** (`app/src/text.rs`) — glyphon renders one shaped buffer per window
   region (editor, gutter, tabs, status left/right, overlay).

There is no widget toolkit. The shell computes rectangles from the window size
and the measured font metrics (`app/src/layout.rs`), and hit-tests them directly.

## Alternatives

**egui / eframe.** Fastest path to a working window, and its immediate-mode
model is pleasant. Rejected because it brings its own text stack and widget
system: the specification asks for a named, documented text layer, and egui's
`TextEdit` would either be bypassed entirely (leaving a large unused dependency)
or used, which would put document text inside the UI toolkit rather than in the
editor core.

**Tauri / any WebView.** Explicitly excluded by section 4.1.

**Direct2D/DirectWrite.** Excellent text quality and the natural Windows choice,
but Windows-only, and section 5.2 names macOS and Linux as future targets.

**Hand-written glyph rasterization on top of `ab_glyph`/`fontdue`.** Would mean
implementing shaping, bidi and font fallback by hand — exactly the work
cosmic-text has already done correctly.

## Reasoning

glyphon is the smallest thing that satisfies the section 4.1 requirement list
while sitting directly on wgpu: cosmic-text supplies segmentation, shaping,
bidi and fallback; glyphon supplies atlas management and a wgpu render pass. Its
own examples target winit 0.30 + wgpu 30, so the three versions are known-good
together rather than pinned by hope.

Using the real shaped layout for geometry — rather than assuming a monospace
advance — is what makes the caret land correctly next to an emoji or a CJK
glyph. `LayoutRun::highlight` produces selection spans from the same layout, so
selection and caret cannot disagree with what was drawn.

Redraws are on demand (`ControlFlow::Wait`). An idle editor renders no frames at
all, which is what keeps the idle CPU contract (section 51) reachable. The
performance overlay is the one exception: while visible it requests a redraw
every frame, and that cost is the price of watching live numbers.

## Consequences

* Shaping is cached per region and only redone when a region's text actually
  changes, so scrolling within already-shaped text and moving the caret cost no
  shaping at all.
* Each region has a single default color. Per-token colors (syntax highlighting)
  will need `set_rich_text` with per-span attributes — a Foundation Stage change
  to `app/src/text.rs`, not an architectural one.
* No IME support yet: composition events are not handled, so CJK input methods
  cannot be used to type into the editor (they can still paste). This is the
  largest known gap in the Stage 1 shell; the contract that closes it is
  ADR-0014.
* First-frame cost is dominated by GPU device creation and font enumeration,
  both of which are one-time. Taking them off the critical path is ADR-0012.

## Benchmark evidence

Release build, Windows 11, i5-12450HX, NVIDIA RTX 3050 Laptop. Five cold
launches, phases logged individually (`LIGHTSPEED_LOG=info`):

| Phase | Vulkan (all backends) | DX12 (native first) |
| --- | ---: | ---: |
| Window creation | 75-118 ms | 75-118 ms |
| GPU adapter enumeration | 510-562 ms | 277-306 ms |
| GPU device creation | 48-64 ms | 94-223 ms |
| Pipeline creation | 1-2 ms | 7-23 ms |
| Font system | 36-39 ms | 56-74 ms |
| **Process start to usable editor** | **943-1057 ms** | **542-740 ms** |

Enumerating every backend loads the Vulkan loader and the vendor ICD before the
editor gets control. Asking for the platform's native backend first (and falling
back to a full search only if that finds nothing) removed ~40% of startup. The
remaining dominant cost is DX12 adapter enumeration, which is driver work before
any LightSpeed code runs.

Against the section 49 contract: the 1 s failure threshold is met, the 500 ms
target is not yet. The remedy is staged startup: see ADR-0012 and the milestone
report.

## Reconsideration criteria

* If input-to-frame P95 exceeds 16 ms on ordinary editing, profile the split
  between shaping, quad upload and present before changing the stack.
* If IME support proves impractical on top of raw winit events, evaluate a
  platform-specific text input layer (Windows TSF) behind the platform boundary.
* If wgpu's Vulkan/DX12 backends prove unreliable on target hardware, evaluate
  the GL backend as a fallback rather than replacing the stack.
