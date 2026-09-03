//! GPU renderer (specification sections 9.3, 26-28, 67).
//!
//! The renderer draws one immutable [`RenderSnapshot`] plus a little chrome. It
//! never reads or mutates editor state: everything it needs arrives in the
//! [`Frame`] it is handed, which is why the editor core can be tested without a
//! window and why a frame can never observe a half-applied edit.
//!
//! Stage 1 draws text, cursor, selection, line numbers and scrolling. No syntax
//! highlighting, diagnostics, Git decorations or minimap.

use crate::compose::{self, DrawList, Layer};
use crate::layout::{FontMetrics, Layout, Rect};
use crate::menu::{self, MenuGeometry, MenuState};
use crate::quads::{Quad, QuadRenderer};
use crate::tabs::TabGeometry;
use crate::text::{Region, TextEngine, TextRegionPlacement};
use crate::theme::{Color, Theme};
use ls_core::RenderSnapshot;
use std::sync::Arc;
use winit::window::Window;

/// Backends tried first. Windows gets DX12, which is the native API and
/// enumerates in a few milliseconds; other platforms have no equivalent
/// shortcut, so they search everything.
const PREFERRED_BACKENDS: wgpu::Backends =
    if cfg!(windows) { wgpu::Backends::DX12 } else { wgpu::Backends::all() };

/// Requests an adapter, treating "none available" as a value rather than an
/// error so the caller can retry on another backend.
async fn request_adapter(
    instance: &wgpu::Instance,
    surface: &wgpu::Surface<'static>,
) -> Option<wgpu::Adapter> {
    instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(surface),
            force_fallback_adapter: false,
            ..Default::default()
        })
        .await
        .ok()
}

/// Why a frame could not be presented.
#[derive(Debug)]
pub enum FrameError {
    /// The surface was lost and could not be recreated.
    SurfaceLost,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::SurfaceLost => f.write_str("the drawing surface was lost"),
        }
    }
}

/// Everything needed to draw one frame.
pub struct Frame<'a> {
    pub layout: Layout,
    pub theme: &'a Theme,
    pub snapshot: Option<&'a RenderSnapshot>,
    /// The tab bar's rectangles, the same ones the click handler tests
    /// against. The renderer is given geometry, never asked to invent it.
    pub tabs: &'a TabGeometry,
    pub status_left: &'a str,
    pub status_right: &'a str,
    /// Color for the status line, chosen by severity.
    pub status_color: Color,
    /// Pre-formatted performance overlay rows.
    pub overlay: Option<&'a [String]>,
    /// Pixels scrolled within the first visible line.
    pub scroll_fraction: f32,
    /// Pixels the view is scrolled to the right.
    pub horizontal_offset: f32,
    /// Shown in place of a document when no tab is open.
    pub placeholder: &'a str,
    /// Menu bar state and its computed geometry.
    pub menu: MenuState,
    pub menu_geometry: &'a MenuGeometry,
    /// Per-item enablement for the open menu, straight from the command
    /// registry. Empty when no menu is open.
    pub menu_enabled: &'a [bool],
    /// Whether the caret is in its visible half of the blink cycle.
    pub caret_visible: bool,
    /// A confirmation strip, when the editor is waiting on an answer.
    pub prompt: Option<&'a str>,
}

pub struct Renderer {
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    quads: QuadRenderer,
    text: TextEngine,
    window: Arc<Window>,
    /// Scratch buffers reused every frame so drawing allocates nothing steady-state.
    editor_text: String,
    gutter_text: String,
    tab_text: String,
    overlay_text: String,
    menu_text: String,
    dropdown_text: String,
    dropdown_disabled_text: String,
}

impl Renderer {
    pub async fn new(
        window: Arc<Window>,
        display_handle: winit::event_loop::OwnedDisplayHandle,
        font_family: &str,
        font_size: f32,
        line_height_ratio: f32,
    ) -> Result<Self, String> {
        // Startup is a contract (specification section 49), so each phase is
        // timed separately: knowing the total is 900 ms is useless without
        // knowing which phase spent it.
        let phase = std::time::Instant::now();
        let size = window.inner_size();

        // Enumerating every backend is the single largest cost in startup: on
        // this machine loading the Vulkan loader and the vendor ICD takes over
        // half a second, against a 500 ms budget for the whole launch. Ask the
        // platform's native backend first and only widen the search if it finds
        // nothing, which keeps the slow path available without paying for it.
        let mut descriptor =
            wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display_handle.clone()));
        descriptor.backends = PREFERRED_BACKENDS;
        let mut instance = wgpu::Instance::new(descriptor);
        let mut surface = instance
            .create_surface(window.clone())
            .map_err(|error| format!("could not create a surface: {error}"))?;
        let mut adapter = request_adapter(&instance, &surface).await;

        if adapter.is_none() && PREFERRED_BACKENDS != wgpu::Backends::all() {
            ls_log::debug!(
                "renderer",
                "backend_fallback",
                "no adapter on the preferred backend; searching all backends"
            );
            let mut descriptor =
                wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display_handle));
            descriptor.backends = wgpu::Backends::all();
            instance = wgpu::Instance::new(descriptor);
            surface = instance
                .create_surface(window.clone())
                .map_err(|error| format!("could not create a surface: {error}"))?;
            adapter = request_adapter(&instance, &surface).await;
        }
        let adapter = adapter.ok_or_else(|| "no suitable GPU adapter".to_string())?;
        let adapter_millis = phase.elapsed().as_secs_f64() * 1000.0;

        let phase = std::time::Instant::now();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("lightspeed.device"),
                ..Default::default()
            })
            .await
            .map_err(|error| format!("could not open the GPU device: {error}"))?;
        let device_millis = phase.elapsed().as_secs_f64() * 1000.0;

        let format = wgpu::TextureFormat::Bgra8UnormSrgb;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            // Vsync: the editor redraws on demand, so there is nothing to gain
            // from spinning faster than the display.
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        let phase = std::time::Instant::now();
        let quads = QuadRenderer::new(&device, format);
        let pipeline_millis = phase.elapsed().as_secs_f64() * 1000.0;

        let phase = std::time::Instant::now();
        let text =
            TextEngine::new(&device, &queue, format, font_family, font_size, line_height_ratio);

        let fonts_millis = phase.elapsed().as_secs_f64() * 1000.0;

        let adapter_info = adapter.get_info();
        ls_log::info!(
            "renderer",
            "gpu_selected",
            fields: [
                ls_log::Field::str("adapter", &adapter_info.name),
                ls_log::Field::str("backend", adapter_info.backend.to_str()),
                ls_log::Field::float("adapter_ms", adapter_millis),
                ls_log::Field::float("device_ms", device_millis),
                ls_log::Field::float("pipelines_ms", pipeline_millis),
                ls_log::Field::float("fonts_ms", fonts_millis),
            ],
            "GPU renderer ready"
        );
        ls_perf::record(
            "startup.gpu_adapter",
            std::time::Duration::from_secs_f64(adapter_millis / 1000.0),
        );
        ls_perf::record(
            "startup.gpu_device",
            std::time::Duration::from_secs_f64(device_millis / 1000.0),
        );
        ls_perf::record(
            "startup.font_system",
            std::time::Duration::from_secs_f64(fonts_millis / 1000.0),
        );

        Ok(Renderer {
            instance,
            surface,
            device,
            queue,
            config,
            quads,
            text,
            window,
            editor_text: String::new(),
            gutter_text: String::new(),
            tab_text: String::new(),
            overlay_text: String::new(),
            menu_text: String::new(),
            dropdown_text: String::new(),
            dropdown_disabled_text: String::new(),
        })
    }

    pub fn metrics(&self) -> FontMetrics {
        self.text.metrics()
    }

    pub fn size(&self) -> (f32, f32) {
        (self.config.width as f32, self.config.height as f32)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Byte offset within a visible row for a character column.
    fn byte_for_column(text: &str, column: usize) -> usize {
        text.char_indices().nth(column).map(|(byte, _)| byte).unwrap_or(text.len())
    }

    /// Maps a point in the text area back to a document position.
    pub fn position_at_point(
        &self,
        frame_layout: &Layout,
        snapshot: &RenderSnapshot,
        x: f32,
        y: f32,
        scroll_fraction: f32,
        horizontal_offset: f32,
    ) -> (usize, usize) {
        let line_height = frame_layout.metrics.line_height;
        let relative_y = (y - frame_layout.text.y + scroll_fraction).max(0.0);
        let row = (relative_y / line_height).floor() as usize;
        let row = row.min(snapshot.lines.len().saturating_sub(1));
        let line_index = snapshot.viewport.first_line.get() + row;

        let relative_x = x - frame_layout.text.x + horizontal_offset;
        let byte = self.text.byte_at_x(Region::Editor, row, relative_x.max(0.0));
        let column = snapshot
            .lines
            .get(row)
            .map(|line| line.text[..byte.min(line.text.len())].chars().count())
            .unwrap_or(0);
        (line_index.min(snapshot.total_lines.saturating_sub(1)), column)
    }

    /// Draws one frame.
    pub fn render(&mut self, frame: &Frame<'_>) -> Result<(), FrameError> {
        let layout = frame.layout;
        let theme = frame.theme;

        self.build_text(frame);
        let mut draw = compose::chrome(&compose::Chrome {
            layout,
            theme,
            tabs: frame.tabs,
            menu: frame.menu,
            menu_geometry: frame.menu_geometry,
            menu_enabled: frame.menu_enabled,
            status_color: frame.status_color,
            prompt: frame.prompt,
            overlay_panel: self.overlay_panel(frame),
        });
        self.add_editor_layer(frame, &mut draw);

        // Acquire the swapchain image, recovering from the recoverable states.
        let surface_frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(())
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Suboptimal(_) => {
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self
                    .instance
                    .create_surface(self.window.clone())
                    .map_err(|_| FrameError::SurfaceLost)?;
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => return Err(FrameError::SurfaceLost),
        };

        self.quads.prepare(
            &self.device,
            &self.queue,
            layout.window.width,
            layout.window.height,
            &draw.base_quads,
            &draw.overlay_quads,
        );

        // Both layers are shaped and uploaded before the pass begins, so a
        // glyph added for the overlay cannot disturb the base layer's atlas
        // coordinates after they have been recorded.
        let resolution =
            glyphon::Resolution { width: self.config.width, height: self.config.height };
        self.prepare_text(resolution, Layer::Base, &draw.base_text);
        self.prepare_text(resolution, Layer::Overlay, &draw.overlay_text);

        let view = surface_frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("lightspeed.frame"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lightspeed.pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: theme.background.linear[0] as f64,
                            g: theme.background.linear[1] as f64,
                            b: theme.background.linear[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            // The order that makes an overlay actually cover what is under it.
            // Reversing any two of these four steps reintroduces the bug where
            // the document's glyphs are drawn on top of the menu.
            self.quads.render_base(&mut pass);
            self.draw_text(&mut pass, Layer::Base);
            self.quads.render_overlay(&mut pass);
            self.draw_text(&mut pass, Layer::Overlay);
        }

        self.queue.submit(Some(encoder.finish()));
        self.queue.present(surface_frame);
        self.text.trim();
        Ok(())
    }

    /// Builds the strings for every text region and shapes the ones that changed.
    fn build_text(&mut self, frame: &Frame<'_>) {
        let layout = frame.layout;

        self.editor_text.clear();
        self.gutter_text.clear();

        match frame.snapshot {
            Some(snapshot) => {
                for (row, line) in snapshot.lines.iter().enumerate() {
                    if row > 0 {
                        self.editor_text.push('\n');
                        self.gutter_text.push('\n');
                    }
                    self.editor_text.push_str(&line.text);
                    if layout.show_line_numbers {
                        use std::fmt::Write;
                        let _ = write!(self.gutter_text, "{:>1$}", line.index.get() + 1, 3);
                    }
                }
            }
            None => self.editor_text.push_str(frame.placeholder),
        }

        // A shaped buffer must be at least as wide as the longest line, or
        // cosmic-text will report clipped runs and the caret will land wrong.
        let text_width = layout.text.width.max(1.0) + frame.horizontal_offset + 4096.0;
        self.text.set_text(
            Region::Editor,
            &self.editor_text,
            text_width,
            layout.text.height.max(layout.metrics.line_height),
        );
        if layout.show_line_numbers {
            self.text.set_text(
                Region::Gutter,
                &self.gutter_text,
                layout.gutter.width.max(1.0),
                layout.gutter.height.max(layout.metrics.line_height),
            );
        }

        // Each label is exactly as wide as the rectangle it is drawn in, so
        // the row reads as separate tabs without the text and the plates being
        // measured two different ways.
        self.tab_text.clear();
        for tab in &frame.tabs.tabs {
            self.tab_text.push_str(&tab.label);
        }
        self.text.set_text(
            Region::Tabs,
            &self.tab_text,
            layout.tab_bar.width.max(1.0),
            layout.tab_bar.height,
        );

        // Menu titles are drawn as one row; their rectangles come from the
        // same geometry the click handler uses, so text and hit boxes cannot
        // disagree.
        self.menu_text.clear();
        for (index, entry) in menu::MENUS.iter().enumerate() {
            if index > 0 {
                self.menu_text.push_str("  ");
            }
            self.menu_text.push_str(entry.title);
        }
        self.text.set_text(
            Region::Menu,
            &self.menu_text,
            layout.menu_bar.width.max(1.0),
            layout.menu_bar.height,
        );

        self.dropdown_text.clear();
        self.dropdown_disabled_text.clear();
        if let Some(open) = frame.menu.open {
            let items = menu::MENUS[open].items;
            let columns = frame
                .menu_geometry
                .dropdown
                .map(|panel| {
                    ((panel.width / layout.metrics.digit_width.max(1.0)) as usize).saturating_sub(2)
                })
                .unwrap_or(24);
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    self.dropdown_text.push('\n');
                    self.dropdown_disabled_text.push('\n');
                }
                let rendered = menu::item_text(item, columns.max(8));
                // An item the registry would refuse is drawn dim, so the menu
                // never offers an action the editor cannot perform.
                if frame.menu_enabled.get(index).copied().unwrap_or(true) {
                    self.dropdown_text.push_str(&rendered);
                } else {
                    self.dropdown_disabled_text.push_str(&rendered);
                }
            }
        }
        let dropdown_size = frame
            .menu_geometry
            .dropdown
            .map(|panel| (panel.width, panel.height))
            .unwrap_or((1.0, 1.0));
        self.text.set_text(
            Region::MenuDropdown,
            &self.dropdown_text,
            dropdown_size.0.max(1.0),
            dropdown_size.1.max(layout.metrics.line_height),
        );
        self.text.set_text(
            Region::MenuDropdownDisabled,
            &self.dropdown_disabled_text,
            dropdown_size.0.max(1.0),
            dropdown_size.1.max(layout.metrics.line_height),
        );

        self.text.set_text(
            Region::Prompt,
            frame.prompt.unwrap_or(""),
            layout.window.width.max(1.0),
            layout.metrics.line_height * 2.0,
        );

        self.text.set_text(
            Region::Status,
            frame.status_left,
            layout.status_bar.width.max(1.0),
            layout.status_bar.height,
        );
        self.text.set_text(
            Region::StatusRight,
            frame.status_right,
            layout.status_bar.width.max(1.0),
            layout.status_bar.height,
        );

        self.overlay_text.clear();
        if let Some(lines) = frame.overlay {
            for (index, line) in lines.iter().enumerate() {
                if index > 0 {
                    self.overlay_text.push('\n');
                }
                self.overlay_text.push_str(line);
            }
        }
        let overlay_height =
            (frame.overlay.map(|l| l.len()).unwrap_or(0) as f32 + 1.0) * layout.metrics.line_height;
        self.text.set_text(
            Region::Overlay,
            &self.overlay_text,
            self.overlay_width(frame),
            overlay_height.max(layout.metrics.line_height),
        );
    }

    fn overlay_width(&self, frame: &Frame<'_>) -> f32 {
        let columns = frame
            .overlay
            .map(|lines| lines.iter().map(|line| line.chars().count()).max().unwrap_or(0))
            .unwrap_or(0);
        (columns as f32 + 2.0) * frame.layout.metrics.digit_width
    }

    /// Shapes and uploads one layer, reporting a failure rather than aborting
    /// the frame: a missing overlay label is better than a black window.
    fn prepare_text(
        &mut self,
        resolution: glyphon::Resolution,
        layer: Layer,
        areas: &[TextRegionPlacement],
    ) {
        if let Err(error) = self.text.prepare(&self.device, &self.queue, resolution, layer, areas) {
            ls_log::warn!("renderer", "text_prepare_failed", "text prepare failed: {error:?}");
        }
    }

    /// Draws a prepared layer.
    fn draw_text(&self, pass: &mut wgpu::RenderPass<'_>, layer: Layer) {
        if let Err(error) = self.text.draw(pass, layer) {
            ls_log::warn!("renderer", "text_render_failed", "text render failed: {error:?}");
        }
    }

    /// Where the performance panel sits, if it is shown.
    fn overlay_panel(&self, frame: &Frame<'_>) -> Option<Rect> {
        let lines = frame.overlay?;
        let layout = frame.layout;
        let width = self.overlay_width(frame);
        let height = (lines.len() as f32 + 0.5) * layout.metrics.line_height;
        Some(Rect::new(
            layout.text.right() - width - 16.0 * layout.scale,
            layout.text.y + 12.0 * layout.scale,
            width,
            height,
        ))
    }

    /// Adds the parts of the frame that depend on where glyphs actually landed.
    ///
    /// Everything here is font-dependent -- a selection follows the shaped run,
    /// a caret sits at a measured advance -- which is why it cannot live in the
    /// composer with the rest of the chrome. It all belongs to [`Layer::Base`]:
    /// the editor is what an overlay covers, never the other way round.
    fn add_editor_layer(&mut self, frame: &Frame<'_>, draw: &mut DrawList) {
        let layout = frame.layout;
        let theme = frame.theme;
        let line_height = layout.metrics.line_height;
        let text_top = layout.text.y - frame.scroll_fraction;
        let origin_x = layout.text.x - frame.horizontal_offset;

        draw.push_text(
            Layer::Base,
            TextRegionPlacement {
                region: Region::Editor,
                origin_x,
                origin_y: text_top,
                clip: layout.text,
                color: if frame.snapshot.is_some() { theme.text } else { theme.dim_text },
            },
        );
        if layout.show_line_numbers && frame.snapshot.is_some() {
            draw.push_text(
                Layer::Base,
                TextRegionPlacement {
                    region: Region::Gutter,
                    origin_x: layout.gutter.x + 8.0 * layout.scale,
                    origin_y: text_top,
                    clip: layout.gutter,
                    color: theme.gutter_text,
                },
            );
        }

        // The right half of the status line is right-aligned against its own
        // measured width, so it needs the shaped row the composer cannot see.
        if layout.show_status_bar {
            let right_width = self.text.row_width(Region::StatusRight, 0);
            if let Some(placement) =
                draw.base_text.iter_mut().find(|placement| placement.region == Region::StatusRight)
            {
                placement.origin_x =
                    (layout.status_bar.right() - right_width - 10.0 * layout.scale)
                        .max(layout.status_bar.x);
            }
        }

        let Some(snapshot) = frame.snapshot else { return };
        let first_line = snapshot.viewport.first_line.get();

        // Current-line highlight, but only when nothing is selected: with a
        // selection on screen it is noise.
        if snapshot.selections.is_empty() {
            if let Some(cursor) = snapshot.cursors.first() {
                if let Some(row) = cursor.line.get().checked_sub(first_line) {
                    let top = layout.row_top(row, frame.scroll_fraction);
                    if top + line_height > layout.text.y && top < layout.text.bottom() {
                        draw.push_quad(
                            Layer::Base,
                            Quad::new(
                                Rect::new(layout.text.x, top, layout.text.width, line_height),
                                theme.current_line,
                            ),
                        );
                    }
                }
            }
        }

        // Selection spans, taken from the shaped run so they follow the glyphs.
        for span in &snapshot.selections {
            let Some(row) = span.line.get().checked_sub(first_line) else { continue };
            let Some(line) = snapshot.lines.get(row) else { continue };
            let top = layout.row_top(row, frame.scroll_fraction);
            if top + line_height <= layout.text.y || top >= layout.text.bottom() {
                continue;
            }
            let start_byte = Self::byte_for_column(&line.text, span.start_column_chars);
            let end_byte = Self::byte_for_column(&line.text, span.end_column_chars);
            let spans = self.text.highlight_spans(Region::Editor, row, start_byte, end_byte);
            for (x, width) in spans {
                if width <= 0.0 {
                    continue;
                }
                draw.push_quad(
                    Layer::Base,
                    Quad::new(Rect::new(origin_x + x, top, width, line_height), theme.selection),
                );
            }
            if span.includes_line_break {
                // Show the selected line break as a half-width block after the
                // end of the line, the way every editor does.
                let x = origin_x + self.text.row_width(Region::Editor, row);
                draw.push_quad(
                    Layer::Base,
                    Quad::new(
                        Rect::new(x, top, layout.metrics.digit_width * 0.5, line_height),
                        theme.selection,
                    ),
                );
            }
        }

        // Caret.
        if frame.caret_visible {
            if let Some(cursor) = snapshot.cursors.iter().find(|c| c.primary) {
                if let Some(row) = cursor.line.get().checked_sub(first_line) {
                    if let Some(line) = snapshot.lines.get(row) {
                        let top = layout.row_top(row, frame.scroll_fraction);
                        if top + line_height > layout.text.y && top < layout.text.bottom() {
                            let byte = Self::byte_for_column(&line.text, cursor.column_chars);
                            let x = origin_x + self.text.caret_x(Region::Editor, row, byte);
                            let width = (2.0 * layout.scale).round().max(2.0);
                            if x >= layout.text.x - width && x <= layout.text.right() {
                                draw.push_quad(
                                    Layer::Base,
                                    Quad::new(Rect::new(x, top, width, line_height), theme.cursor),
                                );
                            }
                        }
                    }
                }
            }
        }

        // Vertical scrollbar.
        if snapshot.total_lines > snapshot.viewport.visible_lines {
            let track = layout.scrollbar;
            draw.push_quad(Layer::Base, Quad::new(track, theme.tab_bar));
            let visible = snapshot.viewport.visible_lines as f32;
            let total = snapshot.total_lines as f32;
            let thumb_height = (track.height * (visible / total)).max(24.0 * layout.scale);
            let max_first = (total - visible).max(1.0);
            let progress = (first_line as f32 / max_first).clamp(0.0, 1.0);
            let thumb_top = track.y + (track.height - thumb_height) * progress;
            draw.push_quad(
                Layer::Base,
                Quad::new(
                    Rect::new(track.x + 2.0, thumb_top, track.width - 4.0, thumb_height),
                    theme.scrollbar,
                ),
            );
        }
    }

    /// Regions re-shaped during the last frame, for the overlay.
    pub fn take_reshaped_count(&mut self) -> usize {
        self.text.take_reshaped_count()
    }

    pub fn quad_count(&self) -> u32 {
        self.quads.instance_count()
    }
}
