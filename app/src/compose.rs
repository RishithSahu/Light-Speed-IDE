//! Frame composition.
//!
//! The renderer draws in two passes, and the reason is a bug this module
//! exists to make impossible. There is one quad pass and one text pass, in that
//! order, so a panel quad emitted "last" still lands *underneath* every glyph
//! in the frame -- including the document text it is supposed to hide. An open
//! dropdown looked transparent for exactly that reason: its background was
//! painted, and then the editor's glyphs were painted over it.
//!
//! ```text
//! before                            after
//! -------------------------         -------------------------
//! all quads                         base quads
//! all text        <- document        base text
//!                    text drawn      overlay quads   <- hides everything under it
//!                    over the panel   overlay text
//! ```
//!
//! So composition is explicit: every rectangle and every text placement
//! declares which layer it belongs to, and the renderer draws
//! `base quads -> base text -> overlay quads -> overlay text`. An overlay
//! surface therefore covers the pixels beneath it, whatever they were.
//!
//! Nothing here touches the GPU, which is the second reason it is a separate
//! module: the composition can be asserted on directly in tests instead of
//! being inspected by eye.

use crate::layout::{Layout, Rect};
use crate::menu::{self, MenuGeometry, MenuState};
use crate::quads::Quad;
use crate::tabs::TabGeometry;
use crate::text::{Region, TextRegionPlacement};
use crate::theme::{Color, Theme};

/// Which pass a piece of the frame is drawn in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Layer {
    /// The editor and its chrome.
    Base,
    /// Surfaces that float above the editor and must hide it.
    Overlay,
}

/// One frame's rectangles and text placements, split by layer.
#[derive(Clone, Debug, Default)]
pub struct DrawList {
    pub base_quads: Vec<Quad>,
    pub overlay_quads: Vec<Quad>,
    pub base_text: Vec<TextRegionPlacement>,
    pub overlay_text: Vec<TextRegionPlacement>,
}

impl DrawList {
    pub fn push_quad(&mut self, layer: Layer, quad: Quad) {
        match layer {
            Layer::Base => self.base_quads.push(quad),
            Layer::Overlay => self.overlay_quads.push(quad),
        }
    }

    pub fn push_text(&mut self, layer: Layer, placement: TextRegionPlacement) {
        match layer {
            Layer::Base => self.base_text.push(placement),
            Layer::Overlay => self.overlay_text.push(placement),
        }
    }

    /// Which layer a region's text is drawn in, or `None` if it is not drawn.
    #[cfg(test)]
    pub fn layer_of(&self, region: Region) -> Option<Layer> {
        if self.overlay_text.iter().any(|placement| placement.region == region) {
            Some(Layer::Overlay)
        } else if self.base_text.iter().any(|placement| placement.region == region) {
            Some(Layer::Base)
        } else {
            None
        }
    }

    /// The topmost quad in `layer` that covers `rect` completely.
    ///
    /// Last wins, because within a layer quads are drawn in order: the surface
    /// the user actually sees over `rect` is the last one that covers it.
    #[cfg(test)]
    pub fn covering_quad(&self, layer: Layer, rect: Rect) -> Option<&Quad> {
        let quads = match layer {
            Layer::Base => &self.base_quads,
            Layer::Overlay => &self.overlay_quads,
        };
        quads.iter().rev().find(|quad| covers(quad.rect, rect))
    }
}

/// Whether `outer` fully contains `inner`, within a pixel of slack.
#[cfg(test)]
fn covers(outer: Rect, inner: Rect) -> bool {
    outer.x <= inner.x + 0.5
        && outer.y <= inner.y + 0.5
        && outer.right() >= inner.right() - 0.5
        && outer.bottom() >= inner.bottom() - 0.5
}

/// Everything the chrome needs to lay itself out.
///
/// Deliberately not a borrow of the shell: composition is a function of the
/// frame's description, not of application state.
pub struct Chrome<'a> {
    pub layout: Layout,
    pub theme: &'a Theme,
    pub tabs: &'a TabGeometry,
    pub menu: MenuState,
    pub menu_geometry: &'a MenuGeometry,
    /// Per-item enablement for the open menu, from the command registry.
    pub menu_enabled: &'a [bool],
    pub status_color: Color,
    /// The confirmation strip's text, when one is up.
    pub prompt: Option<&'a str>,
    /// The performance / loading panel, when it is shown.
    pub overlay_panel: Option<Rect>,
    /// The docked explorer / search / git-status sidebar, when it is shown.
    /// `None` here (rather than an empty row list) is what keeps a hidden
    /// sidebar from drawing so much as an empty panel.
    pub sidebar_panel: Option<Rect>,
    /// Which row of the sidebar (if any, and including its header) sits
    /// under the current selection, for the highlight quad.
    pub sidebar_selected_row: Option<usize>,
    /// Which row the pointer is over, for a lighter hover highlight. Never
    /// drawn on top of the selection highlight -- the selected row already
    /// reads as "the current one".
    pub sidebar_hovered_row: Option<usize>,
}

/// Builds the chrome for one frame.
///
/// The editor's own text, selection and caret are added by the renderer, which
/// is the only place that knows where a glyph actually landed. They all belong
/// to [`Layer::Base`].
pub fn chrome(input: &Chrome<'_>) -> DrawList {
    let mut list = DrawList::default();
    let layout = input.layout;
    let theme = input.theme;
    let scale = layout.scale;

    // --- base: the window's own furniture ------------------------------------
    list.push_quad(Layer::Base, Quad::new(layout.menu_bar, theme.menu_background));
    list.push_quad(Layer::Base, Quad::new(layout.tab_bar, theme.tab_bar));
    list.push_quad(Layer::Base, Quad::new(layout.gutter, theme.gutter_background));
    if layout.show_status_bar {
        list.push_quad(Layer::Base, Quad::new(layout.status_bar, theme.status_bar));
    }

    // The sidebar is docked chrome beside the editor, not a surface floating
    // over it, so it belongs in the base layer with the gutter and status
    // bar rather than the overlay layer.
    if let Some(panel) = input.sidebar_panel {
        list.push_quad(Layer::Base, Quad::new(panel, theme.sidebar_background));
        list.push_quad(
            Layer::Base,
            Quad::new(Rect::new(panel.right(), panel.y, scale, panel.height), theme.sidebar_border),
        );
        // A thin rule under the header (row 0) separates the panel's title
        // from its rows, the way a real title bar would.
        list.push_quad(
            Layer::Base,
            Quad::new(
                Rect::new(panel.x, panel.y + layout.metrics.line_height, panel.width, scale),
                theme.sidebar_border,
            ),
        );
        if let Some(row) = input.sidebar_hovered_row {
            if input.sidebar_selected_row != Some(row) {
                let row_rect = Rect::new(
                    panel.x,
                    panel.y + row as f32 * layout.metrics.line_height,
                    panel.width,
                    layout.metrics.line_height,
                );
                list.push_quad(Layer::Base, Quad::new(row_rect, theme.sidebar_hover));
            }
        }
        if let Some(row) = input.sidebar_selected_row {
            let row_rect = Rect::new(
                panel.x,
                panel.y + row as f32 * layout.metrics.line_height,
                panel.width,
                layout.metrics.line_height,
            );
            list.push_quad(Layer::Base, Quad::new(row_rect, theme.sidebar_selected));
            // A left accent bar on the selection, echoing the active-tab
            // indicator elsewhere in the chrome -- the same visual language
            // for "this is the current one".
            list.push_quad(
                Layer::Base,
                Quad::new(
                    Rect::new(panel.x, row_rect.y, 2.0 * scale, row_rect.height),
                    theme.cursor,
                ),
            );
        }
        list.push_text(
            Layer::Base,
            TextRegionPlacement {
                region: Region::Sidebar,
                origin_x: panel.x + 8.0 * scale,
                origin_y: panel.y,
                clip: panel,
                color: theme.text,
            },
        );
    }

    // Tab plates, from the same rectangles the click handler tests against.
    for tab in &input.tabs.tabs {
        let color = if tab.active { theme.tab_active } else { theme.tab_inactive };
        list.push_quad(Layer::Base, Quad::new(tab.full, color));
        if tab.active {
            list.push_quad(
                Layer::Base,
                Quad::new(
                    Rect::new(
                        tab.full.x,
                        tab.full.bottom() - 2.0 * scale,
                        tab.full.width,
                        2.0 * scale,
                    ),
                    theme.cursor,
                ),
            );
        }
        if tab.dirty {
            list.push_quad(
                Layer::Base,
                Quad::new(
                    Rect::new(tab.full.x, tab.full.y, 2.0 * scale, tab.full.height),
                    theme.dirty_marker,
                ),
            );
        }
    }

    list.push_text(
        Layer::Base,
        TextRegionPlacement {
            region: Region::Tabs,
            origin_x: layout.tab_bar.x,
            origin_y: layout.tab_bar.y + (layout.tab_bar.height - layout.metrics.line_height) / 2.0,
            clip: layout.tab_bar,
            color: theme.tab_text,
        },
    );

    // The menu bar itself is chrome, not an overlay: it is always there.
    if let Some(open) = input.menu.open {
        if let Some(title) = input.menu_geometry.titles.get(open) {
            list.push_quad(Layer::Base, Quad::new(*title, theme.menu_highlight));
        }
    }
    list.push_text(
        Layer::Base,
        TextRegionPlacement {
            region: Region::Menu,
            origin_x: input
                .menu_geometry
                .titles
                .first()
                .map(|rect| rect.x)
                .unwrap_or(layout.menu_bar.x),
            origin_y: layout.menu_bar.y
                + (layout.menu_bar.height - layout.metrics.line_height) / 2.0,
            clip: layout.menu_bar,
            color: theme.status_text,
        },
    );

    if layout.show_status_bar {
        let status_y =
            layout.status_bar.y + (layout.status_bar.height - layout.metrics.line_height) / 2.0;
        list.push_text(
            Layer::Base,
            TextRegionPlacement {
                region: Region::Status,
                origin_x: layout.status_bar.x + 12.0 * scale,
                origin_y: status_y,
                clip: layout.status_bar,
                color: input.status_color,
            },
        );
        list.push_text(
            Layer::Base,
            TextRegionPlacement {
                region: Region::StatusRight,
                origin_x: layout.status_bar.x + 12.0 * scale,
                origin_y: status_y,
                clip: layout.status_bar,
                color: theme.dim_text,
            },
        );
    }

    // --- overlay: surfaces that must hide the editor --------------------------
    if let Some(panel) = input.menu_geometry.dropdown {
        push_panel(&mut list, panel, scale, theme.menu_background, theme.menu_border);

        // The highlight follows the pointer, and only over an item the registry
        // would actually run.
        if let Some(row) = input.menu.hovered_item {
            let runnable = input.menu_enabled.get(row).copied().unwrap_or(true);
            if runnable {
                if let Some(rect) = input.menu_geometry.items.get(row) {
                    list.push_quad(Layer::Overlay, Quad::new(*rect, theme.menu_highlight));
                }
            }
        }

        let origin_x = panel.x + menu::TEXT_INSET * scale;
        let origin_y = panel.y + menu::ROW_INSET * scale;
        list.push_text(
            Layer::Overlay,
            TextRegionPlacement {
                region: Region::MenuDropdown,
                origin_x,
                origin_y,
                clip: panel,
                color: theme.text,
            },
        );
        list.push_text(
            Layer::Overlay,
            TextRegionPlacement {
                region: Region::MenuDropdownDisabled,
                origin_x,
                origin_y,
                clip: panel,
                color: theme.dim_text,
            },
        );
    }

    if input.prompt.is_some() {
        let strip = prompt_rect(&layout);
        push_panel(&mut list, strip, scale, theme.menu_background, theme.dirty_marker);
        list.push_text(
            Layer::Overlay,
            TextRegionPlacement {
                region: Region::Prompt,
                origin_x: strip.x + menu::TEXT_INSET * scale,
                origin_y: strip.y + menu::ROW_INSET * scale,
                clip: strip,
                color: theme.text,
            },
        );
    }

    if let Some(panel) = input.overlay_panel {
        push_panel(&mut list, panel, scale, theme.overlay_background, theme.overlay_border);
        list.push_text(
            Layer::Overlay,
            TextRegionPlacement {
                region: Region::Overlay,
                origin_x: panel.x + menu::TEXT_INSET * scale,
                origin_y: panel.y + menu::ROW_INSET * scale,
                clip: panel,
                color: theme.dim_text,
            },
        );
    }

    list
}

/// Where the confirmation strip sits.
pub fn prompt_rect(layout: &Layout) -> Rect {
    Rect::new(
        layout.text.x,
        layout.text.y + layout.metrics.line_height,
        layout.text.width,
        layout.metrics.line_height * 2.5,
    )
}

/// A bordered, filled surface: border first, fill on top, both in the overlay
/// layer so the pair is drawn after the editor's glyphs.
fn push_panel(list: &mut DrawList, rect: Rect, scale: f32, fill: Color, border: Color) {
    let width = 1.0 * scale;
    list.push_quad(
        Layer::Overlay,
        Quad::new(
            Rect::new(
                rect.x - width,
                rect.y - width,
                rect.width + width * 2.0,
                rect.height + width * 2.0,
            ),
            border,
        ),
    );
    list.push_quad(Layer::Overlay, Quad::new(rect, fill));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::FontMetrics;
    use crate::tabs;
    use ls_core::{DocumentId, TabPresentation};

    fn metrics() -> FontMetrics {
        FontMetrics { font_size: 14.0, line_height: 20.0, digit_width: 8.0 }
    }

    fn layout() -> Layout {
        Layout::with_chrome(
            1000.0,
            700.0,
            1.0,
            metrics(),
            4,
            true,
            true,
            false,
            crate::layout::SIDEBAR_WIDTH,
        )
    }

    fn presentations() -> Vec<TabPresentation> {
        vec![
            TabPresentation {
                id: DocumentId::new(1),
                title: "main.rs".into(),
                tooltip: None,
                dirty: false,
                active: true,
                loading: false,
            },
            TabPresentation {
                id: DocumentId::new(2),
                title: "lib.rs".into(),
                tooltip: None,
                dirty: true,
                active: false,
                loading: false,
            },
        ]
    }

    struct Fixture {
        theme: Theme,
        tabs: TabGeometry,
        menu_geometry: MenuGeometry,
        enabled: Vec<bool>,
    }

    fn fixture(menu: MenuState) -> Fixture {
        let layout = layout();
        let tabs = tabs::geometry(layout.tab_bar, &presentations(), 8.0, 1.0);
        let menu_geometry =
            menu::geometry(layout.menu_bar, menu, 8.0, layout.metrics.line_height, 1.0, &[]);
        let enabled =
            menu.open.map(|open| vec![true; menu::MENUS[open].items.len()]).unwrap_or_default();
        Fixture { theme: Theme::dark(), tabs, menu_geometry, enabled }
    }

    fn compose(menu: MenuState, fixture: &Fixture, prompt: Option<&str>) -> DrawList {
        chrome(&Chrome {
            layout: layout(),
            theme: &fixture.theme,
            tabs: &fixture.tabs,
            menu,
            menu_geometry: &fixture.menu_geometry,
            menu_enabled: &fixture.enabled,
            status_color: fixture.theme.status_text,
            prompt,
            overlay_panel: None,
            sidebar_panel: None,
            sidebar_selected_row: None,
            sidebar_hovered_row: None,
        })
    }

    #[test]
    fn an_open_dropdown_has_a_fully_opaque_background() {
        // The defect: the panel was painted, then the document's glyphs were
        // painted over it, so the menu looked transparent. Opacity alone is not
        // enough -- the fill also has to be in the layer that is drawn last.
        let menu = MenuState { open: Some(0), hovered_item: None };
        let fixture = fixture(menu);
        let list = compose(menu, &fixture, None);
        let panel = fixture.menu_geometry.dropdown.expect("the File menu is open");

        let fill = list
            .covering_quad(Layer::Overlay, panel)
            .expect("the dropdown's background must be an overlay surface");
        assert_eq!(
            fill.color.srgb[3], 255,
            "the dropdown background must be fully opaque, not blended with the editor"
        );
        assert_eq!(fill.color, fixture.theme.menu_background);
    }

    #[test]
    fn the_dropdown_background_covers_every_pixel_of_the_panel() {
        let menu = MenuState { open: Some(1), hovered_item: None };
        let fixture = fixture(menu);
        let list = compose(menu, &fixture, None);
        let panel = fixture.menu_geometry.dropdown.unwrap();

        let fill = list.covering_quad(Layer::Overlay, panel).expect("a covering fill");
        assert!(fill.rect.x <= panel.x && fill.rect.y <= panel.y);
        assert!(fill.rect.right() >= panel.right() && fill.rect.bottom() >= panel.bottom());

        // Every item rectangle is inside the painted area, so no row can render
        // against the document behind it.
        for item in &fixture.menu_geometry.items {
            assert!(covers(fill.rect, *item), "item row {item:?} is not covered by the panel");
        }
    }

    #[test]
    fn the_dropdown_is_drawn_after_the_editor_rather_than_under_it() {
        let menu = MenuState { open: Some(2), hovered_item: None };
        let fixture = fixture(menu);
        let list = compose(menu, &fixture, None);

        assert_eq!(list.layer_of(Region::MenuDropdown), Some(Layer::Overlay));
        assert_eq!(list.layer_of(Region::MenuDropdownDisabled), Some(Layer::Overlay));
        // The things it has to hide are in the earlier pass.
        assert_eq!(list.layer_of(Region::Tabs), Some(Layer::Base));
        assert_eq!(list.layer_of(Region::Status), Some(Layer::Base));

        // Within the overlay pass, quads are uploaded before text, so the fill
        // can never land on top of its own labels.
        assert!(!list.overlay_quads.is_empty());
        assert!(!list.overlay_text.is_empty());
    }

    #[test]
    fn a_closed_menu_contributes_no_overlay_surface() {
        let menu = MenuState::default();
        let fixture = fixture(menu);
        let list = compose(menu, &fixture, None);
        assert!(list.overlay_quads.is_empty(), "nothing floats over the editor");
        assert!(list.overlay_text.is_empty());
        assert_eq!(list.layer_of(Region::MenuDropdown), None);
    }

    #[test]
    fn every_menu_opens_an_opaque_surface() {
        for open in 0..menu::MENUS.len() {
            let menu = MenuState { open: Some(open), hovered_item: None };
            let fixture = fixture(menu);
            let list = compose(menu, &fixture, None);
            let panel = fixture.menu_geometry.dropdown.expect("a panel");
            let fill = list.covering_quad(Layer::Overlay, panel).unwrap_or_else(|| {
                panic!("menu {} has no opaque surface", menu::MENUS[open].title)
            });
            assert_eq!(fill.color.srgb[3], 255);
        }
    }

    #[test]
    fn the_hovered_row_is_highlighted_only_when_it_can_run() {
        let menu = MenuState { open: Some(1), hovered_item: Some(0) };
        let mut fixture = fixture(menu);

        let list = compose(menu, &fixture, None);
        let row = fixture.menu_geometry.items[0];
        assert!(
            list.overlay_quads.iter().any(|quad| quad.rect == row),
            "a runnable row is highlighted"
        );

        fixture.enabled[0] = false;
        let list = compose(menu, &fixture, None);
        assert!(
            !list.overlay_quads.iter().any(|quad| quad.rect == row),
            "a disabled row must not look clickable"
        );
    }

    #[test]
    fn the_confirmation_strip_is_an_opaque_overlay_too() {
        let menu = MenuState::default();
        let fixture = fixture(menu);
        let list = compose(menu, &fixture, Some("unsaved.txt has unsaved changes."));
        let strip = prompt_rect(&layout());

        let fill = list.covering_quad(Layer::Overlay, strip).expect("an opaque strip");
        assert_eq!(fill.color.srgb[3], 255);
        assert_eq!(list.layer_of(Region::Prompt), Some(Layer::Overlay));
    }

    #[test]
    fn the_sidebar_is_a_base_surface_not_an_overlay() {
        // The defect this guards against: the file tree used to be drawn as
        // a floating debug-style overlay box. A docked explorer belongs to
        // the same layer as the gutter and status bar, since it never has to
        // hide anything drawn under it -- the editor's own layout already
        // makes room for it.
        let menu = MenuState::default();
        let fixture = fixture(menu);
        let panel = Rect::new(0.0, 30.0, 260.0, 600.0);
        let list = chrome(&Chrome {
            layout: layout(),
            theme: &fixture.theme,
            tabs: &fixture.tabs,
            menu,
            menu_geometry: &fixture.menu_geometry,
            menu_enabled: &fixture.enabled,
            status_color: fixture.theme.status_text,
            prompt: None,
            overlay_panel: None,
            sidebar_panel: Some(panel),
            sidebar_selected_row: Some(1),
            sidebar_hovered_row: None,
        });
        assert_eq!(list.layer_of(Region::Sidebar), Some(Layer::Base));
        let fill = list
            .covering_quad(Layer::Base, panel)
            .expect("the sidebar's background must be an opaque base surface");
        assert_eq!(fill.color.srgb[3], 255);

        // The selected row's highlight sits one line down, inside the panel.
        let line_height = layout().metrics.line_height;
        let highlighted = list.base_quads.iter().any(|quad| {
            (quad.rect.y - (panel.y + line_height)).abs() < 0.5
                && quad.color == fixture.theme.sidebar_selected
        });
        assert!(highlighted, "the selected row must be highlighted");
    }

    #[test]
    fn a_hidden_sidebar_draws_nothing() {
        let menu = MenuState::default();
        let fixture = fixture(menu);
        let list = compose(menu, &fixture, None);
        assert_eq!(list.layer_of(Region::Sidebar), None);
    }

    #[test]
    fn tab_plates_come_from_the_hit_test_geometry() {
        // The rule from section 8 of the fix list: one computation, two
        // consumers. If a plate were drawn anywhere other than its own
        // rectangle, clicks and pixels would disagree.
        let menu = MenuState::default();
        let fixture = fixture(menu);
        let list = compose(menu, &fixture, None);
        for tab in &fixture.tabs.tabs {
            assert!(
                list.base_quads.iter().any(|quad| quad.rect == tab.full),
                "tab {:?} is drawn somewhere other than its hit region",
                tab.id
            );
        }
    }

    #[test]
    fn hiding_the_status_bar_removes_its_surface_and_its_text() {
        let menu = MenuState::default();
        let fixture = fixture(menu);
        let layout = Layout::with_chrome(
            1000.0,
            700.0,
            1.0,
            metrics(),
            4,
            true,
            false,
            false,
            crate::layout::SIDEBAR_WIDTH,
        );
        let list = chrome(&Chrome {
            layout,
            theme: &fixture.theme,
            tabs: &fixture.tabs,
            menu,
            menu_geometry: &fixture.menu_geometry,
            menu_enabled: &fixture.enabled,
            status_color: fixture.theme.status_text,
            prompt: None,
            overlay_panel: None,
            sidebar_panel: None,
            sidebar_selected_row: None,
            sidebar_hovered_row: None,
        });
        assert_eq!(list.layer_of(Region::Status), None);
        assert_eq!(list.layer_of(Region::StatusRight), None);
    }
}
