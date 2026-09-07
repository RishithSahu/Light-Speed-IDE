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
use crate::icons;
use crate::layout::{FontMetrics, Layout, Rect};
use crate::menu::{self, MenuGeometry, MenuState};
use crate::quads::{Quad, QuadRenderer};
use crate::tabs::TabGeometry;
use crate::text::{Region, RichText, TextEngine, TextRegionPlacement};
use crate::theme::{Color, Theme};
use ls_core::{DecorationKind, RenderSnapshot};
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
    /// Status bar halves. Rich text, because Lapce's status bar is icons and
    /// text on one baseline: a branch glyph then a branch name, an error
    /// glyph then a count.
    pub status_left: &'a RichText,
    pub status_right: &'a RichText,
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
    /// Recently opened files, appended below File's static items. Empty when
    /// nothing has been opened yet.
    pub recent_files: &'a [menu::RecentRow],
    /// Whether the caret is in its visible half of the blink cycle.
    pub caret_visible: bool,
    /// A confirmation strip, when the editor is waiting on an answer.
    pub prompt: Option<&'a str>,
    /// The docked sidebar's rows, already composed with their chevrons,
    /// file-type icons and per-row colors.
    pub sidebar: Option<&'a RichText>,
    /// Which sidebar row (by index into `sidebar`, header included) is
    /// selected, for the highlight quad.
    pub sidebar_selected_row: Option<usize>,
    /// Which sidebar row the pointer is over, for a lighter hover highlight.
    pub sidebar_hovered_row: Option<usize>,
    /// How far the sidebar's row list is scrolled, in logical pixels -- the
    /// same amount subtracted from every row's drawn position (text and
    /// highlight quads alike) so a click still resolves against what the
    /// pointer is actually over.
    pub sidebar_scroll_y: f32,
    /// The command palette's own content (query row + filtered commands),
    /// and its floating panel rectangle, when it is open.
    pub palette: Option<&'a RichText>,
    pub palette_panel: Option<Rect>,
    pub palette_selected: Option<usize>,
    pub palette_hovered: Option<usize>,
    /// The activity bar's icon column, one icon per cell.
    pub activity: &'a RichText,
    pub activity_active: Option<usize>,
    pub activity_hovered: Option<usize>,
    /// Rows for the docked bottom panel's content, when it is shown.
    pub bottom_panel: Option<&'a [String]>,
    /// The bottom panel's own icon rail.
    pub bottom_panel_rail: &'a RichText,
    /// The header's three clusters: menu button, command field, actions.
    pub title_left: &'a RichText,
    pub title_center: &'a RichText,
    pub title_right: &'a RichText,
    /// The breadcrumb trail under the tab bar.
    pub breadcrumb: &'a RichText,
    /// The tab row's leading and trailing icon clusters.
    pub tab_nav: &'a RichText,
    pub tab_actions: &'a RichText,
    /// The dependency view, when it has taken over the editor area.
    pub dependency: Option<DependencyFrame<'a>>,
    /// The settings screen, when it has.
    pub settings: Option<SettingsFrame<'a>>,
}

/// Everything the settings screen needs drawn, already measured.
pub struct SettingsFrame<'a> {
    pub screen: &'a crate::settings_ui::Screen,
    /// The rows of the list, in the order `settings_ui::rows` produced them.
    pub rows: &'a [crate::settings_ui::Row],
    /// The settings those rows describe, so a control knows its own kind.
    pub visible: &'a [&'static ls_core::settings::SettingDescriptor],
    /// The merged settings, for reading current values.
    pub values: &'a ls_core::settings::Settings,
    /// What is in the search box, and whether it has the keyboard.
    pub query: &'a str,
    pub query_focused: bool,
    /// The section picked on the left, if any.
    pub section: Option<usize>,
    /// The field being typed into, and its draft.
    pub editing: Option<(&'static str, &'a str)>,
    /// How far the list is scrolled, in rows.
    pub scroll_rows: usize,
    /// Which file is being edited.
    pub workspace_scope: bool,
}

/// The dependency view's contribution to a frame: a fitted graph, where the
/// reader has panned and zoomed to, and the message to show instead when
/// there is no graph yet. The scene's coordinates start at the graph's own
/// top-left; the view is what puts it on screen.
pub struct DependencyFrame<'a> {
    pub scene: Option<&'a crate::depgraph::Scene>,
    /// Where the reader has panned and zoomed to.
    pub view: crate::depgraph::View,
    /// The node under the pointer, drawn with a ring and its edges picked
    /// out.
    pub traced: Option<usize>,
    pub placeholder: &'a str,
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
    tab_rich: RichText,
    overlay_text: String,

    bottom_panel_text: String,
    /// Where the dependency view's label grid is drawn, worked out while its
    /// text is built and used when it is placed.
    dependency_origin: (f32, f32),
    settings_rich: RichText,
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
            tab_rich: RichText::new(),
            overlay_text: String::new(),

            bottom_panel_text: String::new(),
            dependency_origin: (0.0, 0.0),
            settings_rich: RichText::new(),
            dropdown_text: String::new(),
            dropdown_disabled_text: String::new(),
        })
    }

    /// Changes the font the whole window is drawn with, reporting whether
    /// anything moved.
    pub fn set_font(&mut self, family: &str, font_size: f32, line_height_ratio: f32) -> bool {
        self.text.set_font(family, font_size, line_height_ratio)
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
            sidebar_panel: layout.sidebar_visible.then_some(layout.sidebar),
            sidebar_selected_row: frame.sidebar_selected_row,
            sidebar_hovered_row: frame.sidebar_hovered_row,
            sidebar_scroll_y: frame.sidebar_scroll_y,
            palette_panel: frame.palette_panel,
            palette_selected: frame.palette_selected,
            palette_hovered: frame.palette_hovered,

            activity_active: frame.activity_active,
            activity_hovered: frame.activity_hovered,
            bottom_panel: layout.bottom_panel_visible.then_some(layout.bottom_panel),
            bottom_panel_rail: layout.bottom_panel_visible.then_some(layout.bottom_panel_rail),
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
        // Byte ranges within `editor_text`, for syntax highlighting: one
        // shaped buffer holds the whole visible slice, so a token's position
        // has to be tracked as the lines that come before it are appended.
        let mut syntax_spans: Vec<crate::text::Span> = Vec::new();

        match frame.snapshot {
            Some(snapshot) => {
                for (row, line) in snapshot.lines.iter().enumerate() {
                    if row > 0 {
                        self.editor_text.push('\n');
                        self.gutter_text.push('\n');
                    }
                    let line_start = self.editor_text.len();
                    self.editor_text.push_str(&line.text);
                    for decoration in &snapshot.decorations {
                        if decoration.line != line.index {
                            continue;
                        }
                        let DecorationKind::SyntaxToken(kind) = decoration.kind else {
                            continue;
                        };
                        let start = line_start
                            + Self::byte_for_column(&line.text, decoration.start_column_chars);
                        let end = line_start
                            + Self::byte_for_column(&line.text, decoration.end_column_chars);
                        syntax_spans.push(crate::text::Span::text(
                            start,
                            end,
                            frame.theme.syntax_color(kind),
                        ));
                    }
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
        if syntax_spans.is_empty() {
            self.text.set_text(
                Region::Editor,
                &self.editor_text,
                text_width,
                layout.text.height.max(layout.metrics.line_height),
            );
        } else {
            let default_color =
                if frame.snapshot.is_some() { frame.theme.text } else { frame.theme.dim_text };
            self.text.set_rich_text(
                Region::Editor,
                &self.editor_text,
                text_width,
                layout.text.height.max(layout.metrics.line_height),
                default_color,
                &syntax_spans,
            );
        }
        // The dependency view's labels: one shaped buffer holding only the
        // grid rows the pane can show, so a graph taller than the window
        // does not cost a buffer taller than the window either.
        if let Some(dependency) = &frame.dependency {
            let grid = dependency.scene.map(|scene| {
                crate::depgraph::label_rows(
                    scene,
                    layout.text,
                    dependency.view,
                    crate::depgraph::GridMetrics {
                        digit_width: layout.metrics.digit_width,
                        line_height: layout.metrics.line_height,
                    },
                    dependency.traced,
                )
            });
            let text = match &grid {
                Some(grid) => grid.text.clone(),
                None => dependency.placeholder.to_string(),
            };
            self.dependency_origin =
                grid.map(|grid| grid.origin).unwrap_or((layout.text.x, layout.text.y));
            self.text.set_text(
                Region::DependencyGraph,
                &text,
                layout.text.width.max(1.0) + 4096.0,
                layout.text.height.max(layout.metrics.line_height),
            );
        }

        if let Some(settings) = &frame.settings {
            self.build_settings_text(frame, settings);
        }

        if layout.show_line_numbers {
            self.text.set_text(
                Region::Gutter,
                &self.gutter_text,
                layout.gutter.width.max(1.0),
                layout.gutter.height.max(layout.metrics.line_height),
            );
        }

        // Each tab is a file-type icon followed by a label exactly as wide as
        // the rest of its rectangle, so the row reads as separate tabs
        // without the text and the plates being measured two different ways.
        self.tab_rich.clear();
        for tab in &frame.tabs.tabs {
            let label_color = if tab.active { frame.theme.text } else { frame.theme.dim_text };
            // A bare space ahead of the icon (accounted for in
            // `tabs::ICON_LEAD`) so the glyph sits inset within its own tab
            // rather than flush against the border shared with the one
            // before it.
            self.tab_rich.plain(" ");
            self.tab_rich.icon(tab.icon, tab.icon_color);
            self.tab_rich.colored(&tab.label, label_color);
        }
        self.text.set_rich_text(
            Region::Tabs,
            &self.tab_rich.text,
            layout.tab_bar.width.max(1.0),
            layout.tab_bar.height,
            frame.theme.tab_text,
            &self.tab_rich.spans,
        );

        // The header's three clusters, each its own region because each is
        // positioned independently across the row rather than flowing from a
        // single origin. The icon-only ones (left button, right actions, tab
        // nav/actions) go through `set_icon_cluster` so each is sized and
        // centered against its own button rect rather than the shared,
        // much-smaller UI text metrics.
        let title_left_font = icons::cell_icon_font_size(layout.title_menu_button.height);
        self.text.set_icon_cluster(
            Region::TitleLeft,
            &frame.title_left.text,
            layout.title_menu_button.width.max(1.0),
            layout.title_menu_button.height.max(1.0),
            frame.theme.activity_icon_active,
            &frame.title_left.spans,
            title_left_font,
            layout.title_menu_button.height,
        );
        self.text.set_rich_text(
            Region::TitleCenter,
            &frame.title_center.text,
            layout.title_search.width.max(1.0),
            layout.title_search.height.max(layout.metrics.line_height),
            frame.theme.dim_text,
            &frame.title_center.spans,
        );
        let title_right_font = icons::cell_icon_font_size(layout.title_actions.height);
        self.text.set_icon_cluster(
            Region::TitleRight,
            &frame.title_right.text,
            layout.title_actions.width.max(1.0),
            layout.title_actions.height.max(1.0),
            frame.theme.activity_icon_active,
            &frame.title_right.spans,
            title_right_font,
            layout.title_actions.height,
        );
        self.text.set_rich_text(
            Region::Breadcrumb,
            &frame.breadcrumb.text,
            layout.breadcrumb.width.max(1.0),
            layout.breadcrumb.height.max(layout.metrics.line_height),
            frame.theme.dim_text,
            &frame.breadcrumb.spans,
        );
        let tab_nav_font = icons::cell_icon_font_size(layout.tab_nav.height);
        self.text.set_icon_cluster(
            Region::TabNav,
            &frame.tab_nav.text,
            layout.tab_nav.width.max(1.0),
            layout.tab_nav.height.max(1.0),
            frame.theme.activity_icon_inactive,
            &frame.tab_nav.spans,
            tab_nav_font,
            layout.tab_nav.height,
        );
        let tab_actions_font = icons::cell_icon_font_size(layout.tab_actions.height);
        self.text.set_icon_cluster(
            Region::TabActions,
            &frame.tab_actions.text,
            layout.tab_actions.width.max(1.0),
            layout.tab_actions.height.max(1.0),
            frame.theme.activity_icon_inactive,
            &frame.tab_actions.spans,
            tab_actions_font,
            layout.tab_actions.height,
        );

        self.dropdown_text.clear();
        self.dropdown_disabled_text.clear();
        if frame.menu.open.is_some() {
            let items = menu::all_items();
            let recent = frame.recent_files;
            let columns = frame
                .menu_geometry
                .dropdown
                .map(|panel| {
                    ((panel.width / layout.metrics.digit_width.max(1.0)) as usize).saturating_sub(2)
                })
                .unwrap_or(24);
            let total = items.len() + recent.len();
            for row in 0..total {
                if row > 0 {
                    self.dropdown_text.push('\n');
                    self.dropdown_disabled_text.push('\n');
                }
                let rendered = if row < items.len() {
                    menu::item_text(&items[row], columns.max(8))
                } else {
                    menu::recent_item_text(&recent[row - items.len()], columns.max(8))
                };
                // An item the registry would refuse is drawn dim, so the menu
                // never offers an action the editor cannot perform. Recent
                // files are never refused (see `Frame::menu_enabled`).
                if frame.menu_enabled.get(row).copied().unwrap_or(true) {
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

        let empty_palette = RichText::new();
        let palette = frame.palette.unwrap_or(&empty_palette);
        if let Some(panel) = frame.palette_panel {
            self.text.set_rich_text(
                Region::CommandPalette,
                &palette.text,
                panel.width.max(1.0),
                panel.height.max(layout.metrics.line_height),
                frame.theme.text,
                &palette.spans,
            );
        }

        self.text.set_text(
            Region::Prompt,
            frame.prompt.unwrap_or(""),
            layout.window.width.max(1.0),
            layout.metrics.line_height * 2.0,
        );

        self.text.set_rich_text(
            Region::Status,
            &frame.status_left.text,
            layout.status_bar.width.max(1.0),
            layout.status_bar.height,
            frame.status_color,
            &frame.status_left.spans,
        );
        self.text.set_rich_text(
            Region::StatusRight,
            &frame.status_right.text,
            layout.status_bar.width.max(1.0),
            layout.status_bar.height,
            frame.theme.dim_text,
            &frame.status_right.spans,
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

        let empty = RichText::new();
        let sidebar = frame.sidebar.unwrap_or(&empty);
        // cosmic-text's `shape_until_scroll` only lays out (and `layout_runs`
        // only ever returns) lines within one buffer-height's worth of its
        // internal scroll position -- so a buffer sized to the *panel's*
        // visible height, rather than its full content, silently never shapes
        // rows past that point at all. Scrolling by shifting `origin_y` at
        // draw time (see `compose::chrome`) can only move that already-fixed
        // window around; it can never reveal a row that was never shaped.
        // Sizing the buffer to the content's real height fixes that, and
        // `clip: panel` (in `compose::chrome`) still crops the drawn result
        // to the panel's actual visible rectangle.
        let sidebar_lines = sidebar.text.matches('\n').count() + 1;
        let sidebar_content_height =
            (sidebar_lines as f32 * layout.metrics.line_height).max(layout.sidebar.height);
        self.text.set_rich_text(
            Region::Sidebar,
            &sidebar.text,
            layout.sidebar.width.max(1.0),
            sidebar_content_height.max(layout.metrics.line_height),
            frame.theme.text,
            &sidebar.spans,
        );

        let activity_font = icons::cell_icon_font_size(layout.activity_bar.width);
        self.text.set_icon_cluster(
            Region::ActivityBar,
            &frame.activity.text,
            layout.activity_bar.width.max(1.0),
            layout.activity_bar.height.max(1.0),
            frame.theme.activity_icon_active,
            &frame.activity.spans,
            activity_font,
            layout.activity_bar.width,
        );

        self.bottom_panel_text.clear();
        if let Some(lines) = frame.bottom_panel {
            for (index, line) in lines.iter().enumerate() {
                if index > 0 {
                    self.bottom_panel_text.push('\n');
                }
                self.bottom_panel_text.push_str(line);
            }
        }
        self.text.set_text(
            Region::BottomPanel,
            &self.bottom_panel_text,
            layout.bottom_panel.width.max(1.0),
            layout.bottom_panel.height.max(layout.metrics.line_height),
        );

        let rail_font = icons::cell_icon_font_size(layout.bottom_panel_rail.width);
        self.text.set_icon_cluster(
            Region::BottomPanelRail,
            &frame.bottom_panel_rail.text,
            layout.bottom_panel_rail.width.max(1.0),
            layout.bottom_panel_rail.height.max(1.0),
            frame.theme.activity_icon_active,
            &frame.bottom_panel_rail.spans,
            rail_font,
            layout.bottom_panel_rail.width,
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
    /// Shapes the settings screen's three regions.
    ///
    /// The list is one rich-text buffer rather than a widget per row: it is
    /// a column of lines, which is what a buffer already is, and colouring
    /// by role is what the sidebar and the palette already do.
    fn build_settings_text(&mut self, frame: &Frame<'_>, settings: &SettingsFrame<'_>) {
        use crate::settings_ui::Row;
        let theme = frame.theme;
        let metrics = frame.layout.metrics;

        // The search box.
        self.settings_rich.clear();
        self.settings_rich.icon(icons::Icon::Search, theme.dim_text);
        self.settings_rich.plain(" ");
        if settings.query.is_empty() {
            self.settings_rich.colored("Search settings", theme.dim_text);
        } else {
            self.settings_rich.colored(settings.query, theme.text);
        }
        if settings.query_focused {
            self.settings_rich.colored("\u{2588}", theme.cursor);
        }
        self.text.set_rich_text(
            Region::SettingsSearch,
            &self.settings_rich.text.clone(),
            settings.screen.search.width.max(1.0),
            settings.screen.search.height.max(metrics.line_height),
            theme.text,
            &self.settings_rich.spans.clone(),
        );

        // The section list, with the two scopes above it.
        self.settings_rich.clear();
        let (user_colour, workspace_colour) = if settings.workspace_scope {
            (theme.dim_text, theme.text)
        } else {
            (theme.text, theme.dim_text)
        };
        self.settings_rich.colored("User", user_colour);
        self.settings_rich.plain("  ");
        self.settings_rich.colored("Workspace", workspace_colour);
        self.settings_rich.newline();
        self.settings_rich.newline();
        for (at, section) in ls_core::settings::SECTIONS.iter().enumerate() {
            let picked = settings.section == Some(at);
            let colour = if picked { theme.text } else { theme.dim_text };
            self.settings_rich.colored(section, colour);
            self.settings_rich.newline();
        }
        self.text.set_rich_text(
            Region::SettingsCategories,
            &self.settings_rich.text.clone(),
            settings.screen.categories.width.max(1.0),
            settings.screen.categories.height.max(metrics.line_height),
            theme.dim_text,
            &self.settings_rich.spans.clone(),
        );

        // The list itself, from the row the scroll starts at.
        self.settings_rich.clear();
        for row in settings.rows.iter().skip(settings.scroll_rows) {
            match row {
                Row::Blank => {}
                Row::Heading(text) => {
                    self.settings_rich.colored(text, theme.sidebar_folder);
                }
                Row::Title(text) => {
                    self.settings_rich.colored(text, theme.text);
                }
                Row::Description(text) => {
                    self.settings_rich.plain("  ");
                    self.settings_rich.colored(text, theme.dim_text);
                }
                Row::Value(text) => {
                    // Indented past the control, which is drawn over the
                    // start of this line.
                    self.settings_rich.plain("     ");
                    self.settings_rich.colored(text, theme.text);
                }
            }
            self.settings_rich.newline();
        }
        self.text.set_rich_text(
            Region::SettingsList,
            &self.settings_rich.text.clone(),
            settings.screen.list.width.max(1.0) + 4096.0,
            settings.screen.list.height.max(metrics.line_height),
            theme.text,
            &self.settings_rich.spans.clone(),
        );
    }

    /// Draws the settings screen: its surfaces, its controls, and its text.
    fn add_settings_layer(
        &mut self,
        frame: &Frame<'_>,
        settings: &SettingsFrame<'_>,
        draw: &mut DrawList,
    ) {
        use ls_core::settings::SettingKind;
        let layout = frame.layout;
        let theme = frame.theme;
        let screen = settings.screen;
        let line = layout.metrics.line_height;
        let digit = layout.metrics.digit_width;

        // The search box sits on its own surface so it reads as a field.
        draw.push_quad(Layer::Base, Quad::new(screen.search, theme.overlay_background));

        // The section picked on the left gets the sidebar's own selection.
        if let Some(at) = settings.section {
            if let Some(row) = screen.category_rows.get(at) {
                // Two rows below the scope line and its blank.
                let shifted = Rect::new(row.x, row.y + line * 2.0, row.width, row.height);
                draw.push_quad(Layer::Base, Quad::new(shifted, theme.sidebar_selected));
            }
        }

        for (placement, setting) in screen.placements.iter().zip(settings.visible.iter()) {
            // Nothing outside the list may be drawn: the pane has no scissor
            // of its own, so a control scrolled past the top would otherwise
            // paint over the search box.
            let control = placement.control;
            if control.bottom() < screen.list.y || control.y > screen.list.bottom() {
                continue;
            }
            match setting.kind {
                SettingKind::Bool => {
                    let on = settings.values.bool(setting.key);
                    draw.push_quad(Layer::Base, Quad::new(control, theme.overlay_border));
                    if on {
                        let inset = control.width * 0.22;
                        draw.push_quad(
                            Layer::Base,
                            Quad::new(
                                Rect::new(
                                    control.x + inset,
                                    control.y + inset,
                                    control.width - inset * 2.0,
                                    control.height - inset * 2.0,
                                ),
                                theme.cursor,
                            ),
                        );
                    }
                }
                SettingKind::Choice(options) => {
                    let current = settings.values.text(setting.key);
                    for (option, rect) in options.iter().zip(placement.options.iter()) {
                        let picked = *option == current;
                        let colour =
                            if picked { theme.cursor } else { theme.overlay_background };
                        draw.push_quad(Layer::Base, Quad::new(*rect, colour));
                    }
                }
                _ => {
                    let editing = settings
                        .editing
                        .is_some_and(|(key, _)| key == setting.key);
                    let colour =
                        if editing { theme.overlay_border } else { theme.overlay_background };
                    draw.push_quad(Layer::Base, Quad::new(control, colour));
                }
            }
            if let Some(reset) = placement.reset {
                draw.push_quad(Layer::Base, Quad::new(reset, theme.overlay_background));
            }
        }

        draw.push_text(
            Layer::Base,
            TextRegionPlacement {
                region: Region::SettingsSearch,
                origin_x: screen.search.x + digit,
                origin_y: screen.search.y + (screen.search.height - line) / 2.0,
                clip: screen.search,
                color: theme.text,
            },
        );
        draw.push_text(
            Layer::Base,
            TextRegionPlacement {
                region: Region::SettingsCategories,
                origin_x: screen.categories.x,
                origin_y: screen.categories.y,
                clip: screen.categories,
                color: theme.dim_text,
            },
        );
        draw.push_text(
            Layer::Base,
            TextRegionPlacement {
                region: Region::SettingsList,
                origin_x: screen.list.x,
                origin_y: screen.list.y,
                clip: screen.list,
                color: theme.text,
            },
        );
    }

    fn add_editor_layer(&mut self, frame: &Frame<'_>, draw: &mut DrawList) {
        let layout = frame.layout;
        let theme = frame.theme;
        let line_height = layout.metrics.line_height;
        let text_top = layout.text.y - frame.scroll_fraction;
        let origin_x = layout.text.x - frame.horizontal_offset;

        if frame.dependency.is_none() && frame.settings.is_none() {
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
        }
        if layout.show_line_numbers
            && frame.snapshot.is_some()
            && frame.dependency.is_none()
            && frame.settings.is_none()
        {
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

        // The settings screen stands in for the document, so none of the
        // caret, selection or decoration work below applies to it either.
        if let Some(settings) = &frame.settings {
            self.add_settings_layer(frame, settings, draw);
            return;
        }

        // The dependency view stands in for the document, so none of the
        // caret, selection or decoration work below applies to it.
        if let Some(dependency) = &frame.dependency {
            if let Some(scene) = dependency.scene {
                draw.base_quads.extend(crate::depgraph::pane_quads(
                    scene,
                    layout.text,
                    dependency.view,
                    theme,
                    dependency.traced,
                ));
            }
            // The label grid is pinned to the graph rather than to the pane,
            // so it is drawn at the origin `label_rows` chose: that origin
            // carries the pan's remainder within one cell, which is what
            // keeps the names from shivering as the reader drags.
            draw.push_text(
                Layer::Base,
                TextRegionPlacement {
                    region: Region::DependencyGraph,
                    origin_x: self.dependency_origin.0,
                    origin_y: self.dependency_origin.1,
                    clip: layout.text,
                    color: theme.text,
                },
            );
            return;
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

        // Find matches. Drawn before the selection loop below, so the current
        // match's ordinary selection highlight composites on top of it and
        // reads as "the one you're on" without a second highlight mechanism.
        for decoration in &snapshot.decorations {
            if decoration.kind != DecorationKind::SearchMatch {
                continue;
            }
            let Some(row) = decoration.line.get().checked_sub(first_line) else { continue };
            let Some(line) = snapshot.lines.get(row) else { continue };
            let top = layout.row_top(row, frame.scroll_fraction);
            if top + line_height <= layout.text.y || top >= layout.text.bottom() {
                continue;
            }
            let start_byte = Self::byte_for_column(&line.text, decoration.start_column_chars);
            let end_byte = Self::byte_for_column(&line.text, decoration.end_column_chars);
            for (x, width) in self.text.highlight_spans(Region::Editor, row, start_byte, end_byte) {
                if width <= 0.0 {
                    continue;
                }
                draw.push_quad(
                    Layer::Base,
                    Quad::new(Rect::new(origin_x + x, top, width, line_height), theme.search_match),
                );
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
