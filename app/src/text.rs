//! Text shaping and rasterization (ADR-0002).
//!
//! cosmic-text does segmentation, shaping, bidi and font fallback; glyphon
//! rasterizes into a GPU atlas and draws it with wgpu. The shell keeps one
//! shaped buffer per region of the window and only re-shapes a region when its
//! text actually changes, so scrolling or moving the caret does not re-shape the
//! visible document.

use crate::layout::{FontMetrics, Rect};
use crate::theme::Color;
use glyphon::{
    Attrs, Buffer, Cache, Cursor, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Wrap,
};

use crate::compose::Layer;

/// The regions of the window that hold text.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Region {
    /// The document itself.
    Editor,
    /// Line numbers.
    Gutter,
    /// Tab titles.
    Tabs,
    /// The left half of the status line.
    Status,
    /// The right half of the status line, drawn right-aligned.
    StatusRight,
    /// The performance overlay.
    Overlay,
    /// The docked explorer / search / git-status sidebar.
    Sidebar,
    /// Menu bar titles.
    Menu,
    /// The open dropdown's items that can be run now.
    MenuDropdown,
    /// The open dropdown's items that cannot apply right now.
    MenuDropdownDisabled,
    /// A confirmation strip.
    Prompt,
}

const REGION_COUNT: usize = 11;

fn region_index(region: Region) -> usize {
    match region {
        Region::Editor => 0,
        Region::Gutter => 1,
        Region::Tabs => 2,
        Region::Status => 3,
        Region::StatusRight => 4,
        Region::Overlay => 5,
        Region::Menu => 6,
        Region::MenuDropdown => 7,
        Region::MenuDropdownDisabled => 8,
        Region::Prompt => 9,
        Region::Sidebar => 10,
    }
}

struct ShapedRegion {
    buffer: Buffer,
    /// The text currently shaped, so an unchanged region costs nothing.
    text: String,
    width: f32,
    height: f32,
    /// The color spans last shaped via `set_rich_text`, so an unchanged rich
    /// region also costs nothing -- without this, a region with no text to
    /// compare (unlike `set_text`'s fast path) would reshape every single
    /// frame even while showing exactly the same thing. Always empty for a
    /// region only ever drawn with `set_text`.
    spans: Vec<(usize, usize, Color)>,
    /// The default color last used by `set_rich_text`, compared alongside
    /// `spans` -- there is no live theme switch today, so this never
    /// actually changes frame to frame, but the fast path would be wrong to
    /// assume that rather than check it.
    default_color: Color,
}

/// Owns the font system and every shaped buffer.
pub struct TextEngine {
    font_system: FontSystem,
    swash: SwashCache,
    atlas: TextAtlas,
    /// One renderer per composition layer. glyphon prepares a whole frame's
    /// worth of areas at once, so drawing the editor and then an overlay on top
    /// of it needs two of them rather than two calls to one.
    renderer: TextRenderer,
    overlay_renderer: TextRenderer,
    viewport: Viewport,
    regions: Vec<ShapedRegion>,
    metrics: FontMetrics,
    family: String,
    /// Number of regions re-shaped in the most recent frame.
    reshaped: usize,
}

impl TextEngine {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        font_family: &str,
        font_size: f32,
        line_height_ratio: f32,
    ) -> Self {
        let mut font_system = FontSystem::new();
        let swash = SwashCache::new();
        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        let mut atlas = TextAtlas::new(device, queue, &cache, format);
        let renderer =
            TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);
        let overlay_renderer =
            TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);

        let line_height = (font_size * line_height_ratio).round().max(1.0);
        let text_metrics = Metrics::new(font_size, line_height);

        let mut regions = Vec::with_capacity(REGION_COUNT);
        for _ in 0..REGION_COUNT {
            let mut buffer = Buffer::new(&mut font_system, text_metrics);
            // The editor lays out its own lines; wrapping would break the
            // one-row-per-document-line mapping that hit testing relies on.
            buffer.set_wrap(Wrap::None);
            regions.push(ShapedRegion {
                buffer,
                text: String::new(),
                width: 0.0,
                height: 0.0,
                spans: Vec::new(),
                default_color: Color::rgb(0, 0, 0),
            });
        }

        let mut engine = TextEngine {
            font_system,
            swash,
            atlas,
            renderer,
            overlay_renderer,
            viewport,
            regions,
            metrics: FontMetrics { font_size, line_height, digit_width: font_size * 0.6 },
            family: font_family.to_string(),
            reshaped: 0,
        };
        engine.metrics.digit_width = engine.measure_digit_width();
        engine
    }

    pub fn metrics(&self) -> FontMetrics {
        self.metrics
    }

    /// Advance width of one digit in the chosen monospace face, measured rather
    /// than assumed.
    fn measure_digit_width(&mut self) -> f32 {
        const SAMPLE: &str = "0000000000";
        let index = region_index(Region::Status);
        let family = self.family.clone();
        let attrs = Attrs::new().family(Family::Name(&family));
        let region = &mut self.regions[index];
        region.buffer.set_size(Some(4096.0), Some(64.0));
        region.buffer.set_text(SAMPLE, &attrs, Shaping::Advanced, None);
        region.buffer.shape_until_scroll(&mut self.font_system, false);
        let width = region
            .buffer
            .layout_runs()
            .next()
            .map(|run| run.line_w / SAMPLE.chars().count() as f32)
            .unwrap_or(self.metrics.font_size * 0.6);
        // Leave the region dirty so the real status text is shaped next frame.
        region.text.clear();
        width.max(1.0)
    }

    /// Shapes `text` into `region` if it differs from what is already there.
    pub fn set_text(&mut self, region: Region, text: &str, width: f32, height: f32) {
        let index = region_index(region);
        let unchanged = self.regions[index].text == text
            && (self.regions[index].width - width).abs() < 0.5
            && (self.regions[index].height - height).abs() < 0.5
            // A region last drawn with `set_rich_text` must not short-circuit
            // here just because the text and size happen to match -- it is
            // still carrying colored spans that plain `set_text` needs to
            // clear away.
            && self.regions[index].spans.is_empty();
        if unchanged {
            return;
        }

        let attrs = Attrs::new().family(Family::Name(&self.family));
        let entry = &mut self.regions[index];
        entry.buffer.set_size(Some(width.max(1.0)), Some(height.max(1.0)));
        entry.buffer.set_text(text, &attrs, Shaping::Advanced, None);
        entry.buffer.shape_until_scroll(&mut self.font_system, false);
        entry.text.clear();
        entry.text.push_str(text);
        entry.width = width;
        entry.height = height;
        entry.spans.clear();
        self.reshaped += 1;
    }

    /// Like [`Self::set_text`], but `spans` colors byte ranges of `text`
    /// differently from the surrounding default color -- syntax highlighting's
    /// hook into text shaping. Ranges must be sorted by start and
    /// non-overlapping (the [`Decoration`](ls_core::Decoration) list a
    /// [`RenderSnapshot`](ls_core::RenderSnapshot) produces already is, since
    /// it is built by scanning lines in order).
    pub fn set_rich_text(
        &mut self,
        region: Region,
        text: &str,
        width: f32,
        height: f32,
        default_color: Color,
        spans: &[(usize, usize, Color)],
    ) {
        let index = region_index(region);
        // Unlike `set_text`, this used to always reshape -- there was no
        // "nothing changed" fast path at all, so even a region showing
        // exactly the same colored text every frame (a hidden or idle
        // sidebar, say) paid a full reshape every frame. Measured
        // regression: render.frame p95 blew past its 16ms budget from an
        // empty sidebar alone. Comparing the cached spans alongside the text
        // closes that gap the same way `set_text` already does for plain
        // regions.
        let unchanged = self.regions[index].text == text
            && (self.regions[index].width - width).abs() < 0.5
            && (self.regions[index].height - height).abs() < 0.5
            && self.regions[index].spans == spans
            && self.regions[index].default_color == default_color;
        if unchanged {
            return;
        }
        let base = Attrs::new().family(Family::Name(&self.family));
        let mut runs: Vec<(&str, Attrs)> = Vec::with_capacity(spans.len() * 2 + 1);
        let mut cursor = 0usize;
        for &(start, end, color) in spans {
            if start < cursor || end <= start || end > text.len() {
                continue;
            }
            if start > cursor {
                runs.push((&text[cursor..start], base.clone()));
            }
            runs.push((&text[start..end], base.clone().color(color.to_glyphon())));
            cursor = end;
        }
        if cursor < text.len() {
            runs.push((&text[cursor..], base.clone()));
        }

        let entry = &mut self.regions[index];
        entry.buffer.set_size(Some(width.max(1.0)), Some(height.max(1.0)));
        entry.buffer.set_rich_text(
            runs,
            &base.color(default_color.to_glyphon()),
            Shaping::Advanced,
            None,
        );
        entry.buffer.shape_until_scroll(&mut self.font_system, false);
        entry.text.clear();
        entry.text.push_str(text);
        entry.width = width;
        entry.height = height;
        entry.spans.clear();
        entry.spans.extend_from_slice(spans);
        entry.default_color = default_color;
        self.reshaped += 1;
    }

    pub fn take_reshaped_count(&mut self) -> usize {
        std::mem::take(&mut self.reshaped)
    }

    fn buffer(&self, region: Region) -> &Buffer {
        &self.regions[region_index(region)].buffer
    }

    /// X offset of a byte position within a shaped row, relative to the row's
    /// left edge. Uses the real glyph positions, so it is correct for
    /// proportional fallback faces and wide characters alike.
    pub fn caret_x(&self, region: Region, row: usize, byte: usize) -> f32 {
        let buffer = self.buffer(region);
        for run in buffer.layout_runs() {
            if run.line_i != row {
                continue;
            }
            let mut last_end = 0.0;
            for glyph in run.glyphs {
                if byte <= glyph.start {
                    return glyph.x;
                }
                if byte < glyph.end {
                    // Inside a cluster: interpolate across it.
                    let span = (glyph.end - glyph.start).max(1) as f32;
                    let progress = (byte - glyph.start) as f32 / span;
                    return glyph.x + glyph.w * progress;
                }
                last_end = glyph.x + glyph.w;
            }
            return if run.glyphs.is_empty() { 0.0 } else { last_end };
        }
        0.0
    }

    /// Highlight spans `(x, width)` for a byte range within a shaped row.
    pub fn highlight_spans(
        &self,
        region: Region,
        row: usize,
        start_byte: usize,
        end_byte: usize,
    ) -> Vec<(f32, f32)> {
        let buffer = self.buffer(region);
        for run in buffer.layout_runs() {
            if run.line_i == row {
                return run
                    .highlight(Cursor::new(row, start_byte), Cursor::new(row, end_byte))
                    .collect();
            }
        }
        Vec::new()
    }

    /// Byte offset in `row` nearest to `x` (relative to the row's left edge).
    pub fn byte_at_x(&self, region: Region, row: usize, x: f32) -> usize {
        let buffer = self.buffer(region);
        for run in buffer.layout_runs() {
            if run.line_i != row {
                continue;
            }
            for glyph in run.glyphs {
                if x < glyph.x + glyph.w / 2.0 {
                    return glyph.start;
                }
                if x < glyph.x + glyph.w {
                    return glyph.end;
                }
            }
            return run.text.len();
        }
        0
    }

    /// Width of a shaped row in pixels.
    pub fn row_width(&self, region: Region, row: usize) -> f32 {
        self.buffer(region)
            .layout_runs()
            .find(|run| run.line_i == row)
            .map(|run| run.line_w)
            .unwrap_or(0.0)
    }

    /// Shapes and uploads one composition layer's regions.
    ///
    /// Every layer must be prepared before any of them is drawn. Preparing a
    /// layer can grow the glyph atlas, and a layer already recorded into the
    /// pass would then sample the new texture with the coordinates it was given
    /// for the old one -- so an overlay with an unseen glyph in it could
    /// scramble the editor's text beneath it.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        resolution: Resolution,
        layer: Layer,
        areas: &[TextRegionPlacement],
    ) -> Result<(), glyphon::RenderError> {
        self.viewport.update(queue, resolution);

        let text_areas: Vec<TextArea<'_>> = areas
            .iter()
            .map(|placement| TextArea {
                buffer: &self.regions[region_index(placement.region)].buffer,
                left: placement.origin_x,
                top: placement.origin_y,
                scale: 1.0,
                bounds: TextBounds {
                    left: placement.clip.x.max(0.0) as i32,
                    top: placement.clip.y.max(0.0) as i32,
                    right: placement.clip.right().max(0.0) as i32,
                    bottom: placement.clip.bottom().max(0.0) as i32,
                },
                default_color: placement.color.to_glyphon(),
                custom_glyphs: &[],
            })
            .collect();

        let renderer = match layer {
            Layer::Base => &mut self.renderer,
            Layer::Overlay => &mut self.overlay_renderer,
        };
        match renderer.prepare(
            device,
            queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            text_areas,
            &mut self.swash,
        ) {
            Ok(()) => Ok(()),
            Err(glyphon::PrepareError::AtlasFull) => Err(glyphon::RenderError::RemovedFromAtlas),
        }
    }

    /// Draws a layer that was already prepared.
    pub fn draw(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        layer: Layer,
    ) -> Result<(), glyphon::RenderError> {
        let renderer = match layer {
            Layer::Base => &self.renderer,
            Layer::Overlay => &self.overlay_renderer,
        };
        renderer.render(&self.atlas, &self.viewport, pass)
    }

    /// Releases atlas space that this frame did not use.
    pub fn trim(&mut self) {
        self.atlas.trim();
    }
}

/// Where a shaped region is drawn, and how it is clipped.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TextRegionPlacement {
    pub region: Region,
    pub origin_x: f32,
    pub origin_y: f32,
    pub clip: Rect,
    pub color: Color,
}
