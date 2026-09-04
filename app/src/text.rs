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
    /// The open dropdown's items that can be run now.
    MenuDropdown,
    /// The open dropdown's items that cannot apply right now.
    MenuDropdownDisabled,
    /// A confirmation strip.
    Prompt,
    /// The persistent icon-only activity bar at the far left.
    ActivityBar,
    /// The docked bottom panel (currently: the terminal).
    BottomPanel,
    /// The bottom panel's own icon rail, to the left of its content.
    BottomPanelRail,
    /// The title bar's left cluster: logo and the menu button.
    TitleLeft,
    /// The title bar's centered command/search field.
    TitleCenter,
    /// The title bar's right cluster: run and settings.
    TitleRight,
    /// The breadcrumb trail under the tab bar.
    Breadcrumb,
    /// The tab row's leading navigation cluster.
    TabNav,
    /// The tab row's trailing split/close cluster.
    TabActions,
}

const REGION_COUNT: usize = 19;

fn region_index(region: Region) -> usize {
    match region {
        Region::Editor => 0,
        Region::Gutter => 1,
        Region::Tabs => 2,
        Region::Status => 3,
        Region::StatusRight => 4,
        Region::Overlay => 5,
        Region::MenuDropdown => 6,
        Region::MenuDropdownDisabled => 7,
        Region::Prompt => 8,
        Region::Sidebar => 9,
        Region::ActivityBar => 10,
        Region::BottomPanel => 11,
        Region::BottomPanelRail => 12,
        Region::TitleLeft => 13,
        Region::TitleCenter => 14,
        Region::TitleRight => 15,
        Region::Breadcrumb => 16,
        Region::TabNav => 17,
        Region::TabActions => 18,
    }
}

/// One styled run inside a rich-text region: a byte range, the color it is
/// drawn in, and which font it is shaped with.
///
/// The font matters because the chrome mixes the two: a tab is an icon glyph
/// and then a filename, a status bar is a branch icon and then a branch name.
/// Both have to shape into one buffer to line up on one baseline, so the
/// choice of font belongs to the run, not to the region.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub color: Color,
    /// Shape this run with a bundled icon font (which one) rather than the UI
    /// font. There are two icon fonts in play -- Codicons for chrome, Material
    /// Design Icons for file types -- so this carries the family name rather
    /// than a bare bool.
    pub icon_family: Option<&'static str>,
}

impl Span {
    pub fn text(start: usize, end: usize, color: Color) -> Self {
        Span { start, end, color, icon_family: None }
    }

    pub fn icon(start: usize, end: usize, color: Color, family: &'static str) -> Self {
        Span { start, end, color, icon_family: Some(family) }
    }
}

/// A string being built alongside the spans that style it.
///
/// Every piece of Lapce-style chrome is a mix of icon glyphs and text on one
/// baseline -- a tab is an icon then a filename, a status bar is a branch icon
/// then a branch name then an error icon then a count. Tracking byte offsets
/// for that by hand at each call site is exactly the kind of thing that is
/// wrong once and then wrong everywhere, so it is done here instead.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RichText {
    pub text: String,
    pub spans: Vec<Span>,
}

impl RichText {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.spans.clear();
    }

    /// Appends an icon glyph, shaped with whichever bundled icon font it
    /// belongs to. Accepts anything that converts to [`crate::icons::Glyph`]
    /// -- a chrome [`crate::icons::Icon`] or a file-type
    /// [`crate::icons::FileIcon`] -- so call sites never need to know which
    /// font a particular glyph actually lives in.
    pub fn icon(&mut self, icon: impl Into<crate::icons::Glyph>, color: Color) -> &mut Self {
        let glyph = icon.into();
        let start = self.text.len();
        self.text.push(glyph.ch);
        self.spans.push(Span::icon(start, self.text.len(), color, glyph.family));
        self
    }

    /// Appends an icon-width blank: an icon glyph drawn fully transparent.
    ///
    /// Icons and monospace characters have different advances, so a row that
    /// omits its icon cannot pad with spaces and stay aligned with the rows
    /// that have one -- a file's name would sit at a different column than a
    /// folder's. An invisible glyph from the same font advances by exactly
    /// the same amount as a visible one, which is the only padding that keeps
    /// a tree's names in a straight line.
    pub fn icon_space(&mut self) -> &mut Self {
        self.icon(crate::icons::Icon::CircleFilled, Color::rgba(0, 0, 0, 0))
    }

    /// Appends text in a specific color.
    pub fn colored(&mut self, text: &str, color: Color) -> &mut Self {
        if text.is_empty() {
            return self;
        }
        let start = self.text.len();
        self.text.push_str(text);
        self.spans.push(Span::text(start, self.text.len(), color));
        self
    }

    /// Appends text in the region's default color, with no span of its own.
    pub fn plain(&mut self, text: &str) -> &mut Self {
        self.text.push_str(text);
        self
    }

    pub fn newline(&mut self) -> &mut Self {
        self.text.push('\n');
        self
    }
}

struct ShapedRegion {
    buffer: Buffer,
    /// The text currently shaped, so an unchanged region costs nothing.
    text: String,
    width: f32,
    height: f32,
    /// The spans last shaped via `set_rich_text`, so an unchanged rich
    /// region also costs nothing -- without this, a region with no text to
    /// compare (unlike `set_text`'s fast path) would reshape every single
    /// frame even while showing exactly the same thing. Always empty for a
    /// region only ever drawn with `set_text`.
    spans: Vec<Span>,
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
        // The icon font is bundled into the binary rather than looked up by
        // name: the chrome's icons are not optional decoration, and a shell
        // whose activity bar silently renders as blank boxes because a font
        // is not installed is not a shell that shipped.
        font_system.db_mut().load_font_data(crate::icons::ICON_FONT.to_vec());
        font_system.db_mut().load_font_data(crate::icons::MATERIAL_ICON_FONT.to_vec());
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
            metrics: FontMetrics {
                font_size,
                line_height,
                digit_width: font_size * 0.6,
                icon_width: font_size,
                material_icon_width: font_size,
            },
            family: font_family.to_string(),
            reshaped: 0,
        };
        engine.metrics.digit_width = engine.measure_digit_width();
        engine.metrics.icon_width = engine.measure_icon_width();
        engine.metrics.material_icon_width = engine.measure_material_icon_width();
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

    /// Advance width of one chrome icon glyph, measured the same way as the
    /// digit.
    ///
    /// Icons are square-ish and do not share the UI font's monospace advance,
    /// so anything that mixes the two in one run -- a tree row's chevron then
    /// a filename -- has to know this to compute a rectangle that matches
    /// what actually gets shaped. Guessing it is how a click lands on the
    /// wrong row.
    fn measure_icon_width(&mut self) -> f32 {
        self.measure_family_icon_width(crate::icons::Icon::Files.glyph(), crate::icons::ICON_FAMILY)
    }

    /// Advance width of one Material Design Icons file-type glyph, measured
    /// separately from [`Self::measure_icon_width`] since the two icon fonts
    /// do not share an advance -- a tab is a file-type glyph then its name,
    /// and a rectangle computed from the chrome font's width would drift from
    /// what actually gets shaped there.
    fn measure_material_icon_width(&mut self) -> f32 {
        self.measure_family_icon_width(
            crate::icons::FileIcon::Generic.glyph(),
            crate::icons::MATERIAL_ICON_FAMILY,
        )
    }

    fn measure_family_icon_width(&mut self, sample: char, family_name: &str) -> f32 {
        let sample = sample.to_string();
        let index = region_index(Region::Status);
        let attrs = Attrs::new().family(Family::Name(family_name));
        let region = &mut self.regions[index];
        region.buffer.set_size(Some(4096.0), Some(64.0));
        region.buffer.set_text(&sample, &attrs, Shaping::Advanced, None);
        region.buffer.shape_until_scroll(&mut self.font_system, false);
        let width = region
            .buffer
            .layout_runs()
            .next()
            .map(|run| run.line_w)
            .unwrap_or(self.metrics.font_size);
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
        spans: &[Span],
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
        for span in spans {
            let Span { start, end, color, icon_family } = *span;
            if start < cursor || end <= start || end > text.len() {
                continue;
            }
            if start > cursor {
                runs.push((&text[cursor..start], base.clone()));
            }
            let attrs = match icon_family {
                Some(family) => Attrs::new().family(Family::Name(family)),
                None => base.clone(),
            };
            runs.push((&text[start..end], attrs.color(color.to_glyphon())));
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

    /// Like [`Self::set_rich_text`], but for a region that is *only* icons
    /// filling a button- or cell-shaped space (the activity bar, a panel's
    /// icon rail, the title bar's buttons) rather than a left-aligned list.
    ///
    /// Two things `set_rich_text` does not do, both needed for an icon to
    /// actually read as an icon instead of a stray character in the corner
    /// of its box:
    /// - Shapes at `font_size`/`line_height` the caller chooses, rather than
    ///   the shared UI text size every other region uses. An icon sized like
    ///   a lowercase letter in a 50px activity-bar cell is the "too small"
    ///   half of the bug this fixes.
    /// - Centers every line horizontally within `width`, using cosmic-text's
    ///   own line alignment rather than a hand-measured offset that only
    ///   worked if the measurement matched what actually got shaped.
    ///
    /// Vertical centering needs no help from the caller: cosmic-text's own
    /// `LayoutRunIter` already centers each line's glyphs within its
    /// `line_height` using their measured ascent/descent (see
    /// `cosmic_text::buffer::LayoutRunIter::next`), so placing the region's
    /// `origin_y` at the cell's own top edge is enough -- an earlier version
    /// of this method's caller applied a second, hand-rolled centering
    /// nudge on top of that, which double-shifted every icon down.
    #[allow(clippy::too_many_arguments)]
    pub fn set_icon_cluster(
        &mut self,
        region: Region,
        text: &str,
        width: f32,
        height: f32,
        default_color: Color,
        spans: &[Span],
        font_size: f32,
        line_height: f32,
    ) {
        let index = region_index(region);
        let metrics = Metrics::new(font_size.max(1.0), line_height.max(1.0));
        let metrics_changed = self.regions[index].buffer.metrics() != metrics;
        let unchanged = !metrics_changed
            && self.regions[index].text == text
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
        for span in spans {
            let Span { start, end, color, icon_family } = *span;
            if start < cursor || end <= start || end > text.len() {
                continue;
            }
            if start > cursor {
                runs.push((&text[cursor..start], base.clone()));
            }
            let attrs = match icon_family {
                Some(family) => Attrs::new().family(Family::Name(family)),
                None => base.clone(),
            };
            runs.push((&text[start..end], attrs.color(color.to_glyphon())));
            cursor = end;
        }
        if cursor < text.len() {
            runs.push((&text[cursor..], base.clone()));
        }

        let entry = &mut self.regions[index];
        entry.buffer.set_metrics(metrics);
        entry.buffer.set_size(Some(width.max(1.0)), Some(height.max(1.0)));
        entry.buffer.set_rich_text(
            runs,
            &base.color(default_color.to_glyphon()),
            Shaping::Advanced,
            None,
        );
        entry.buffer.shape_until_scroll(&mut self.font_system, false);
        // Centering a line is a layout-only change (it does not reshape any
        // glyph), but it still needs a second pass over `shape_until_scroll`
        // for cosmic-text to actually recompute glyph positions from it.
        for line in entry.buffer.lines.iter_mut() {
            line.set_align(Some(glyphon::cosmic_text::Align::Center));
        }
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
