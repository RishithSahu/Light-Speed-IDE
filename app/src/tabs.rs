//! Tab bar geometry.
//!
//! One computation feeds both drawing and hit testing:
//!
//! ```text
//!       tabs::geometry(bar, presentations, char_width, icon_width, scale)
//!                                  |
//!                    +-------------+-------------+
//!                    v                           v
//!              renderer draws              tabs::hit() routes clicks
//! ```
//!
//! The renderer used to derive tab rectangles from the shaped text row and hand
//! them back to the click handler after the fact. That had two failure modes,
//! both of which were real: the rectangles were a frame stale, so a click on a
//! just-opened tab hit the previous layout, and each box carried a positional
//! index rather than a `DocumentId`, so a tab list that changed shape between
//! frames could activate or close a different document than the one clicked.
//!
//! Everything here is a pure function of the layout, so it can be tested
//! without a window and cannot drift from what is drawn. Widths come from the
//! measured monospace advance, the same assumption the gutter and the editor
//! already make.

use crate::layout::Rect;
use ls_core::{DocumentId, TabPresentation};

/// The close control's glyph.
pub const CLOSE_GLYPH: char = '\u{00d7}';

/// Leading padding before the icon, in characters -- without this the icon
/// glyph sits flush against the tab's own left edge (and the previous tab's
/// separator), reading as clipped into the tab beside it rather than inset
/// within its own.
const ICON_LEAD: usize = 1;
/// Characters between the title and the close control: a marker slot, a space,
/// the close glyph, and one trailing space.
const MARKER_SLOT: usize = 1;
const GAP: usize = 2;
const TRAIL: usize = 2;

/// The state character shown after a tab's title.
///
/// A dirty document is the one the user must not lose, so it wins over the
/// loading marker when a document somehow has both.
pub fn marker(tab: &TabPresentation) -> char {
    if tab.dirty {
        '*'
    } else if tab.loading {
        '.'
    } else {
        ' '
    }
}

/// The label drawn for one tab, padded so every cell is a whole number of
/// monospace advances.
///
/// The file-type icon is *not* part of this string: it is shaped from the
/// icon font, whose advance differs from the monospace one, so it is
/// composed in front of this by the renderer and accounted for separately in
/// [`geometry`].
pub fn label(tab: &TabPresentation) -> String {
    let mut text = String::with_capacity(tab.title.len() + MARKER_SLOT + GAP + TRAIL + 1);
    text.push(' ');
    text.push_str(&tab.title);
    text.push(marker(tab));
    text.push_str("  ");
    text.push(CLOSE_GLYPH);
    text.push_str("  ");
    text
}

/// The file-type icon drawn at the head of one tab: a modified document
/// keeps the dirty dot in its marker slot, so this is purely about file type.
pub fn icon(tab: &TabPresentation) -> crate::icons::FileIcon {
    crate::icons::icon_for_file(&tab.title)
}

/// Monospace characters in one tab's cell, excluding its icon.
fn cell_chars(tab: &TabPresentation) -> usize {
    1 + tab.title.chars().count() + MARKER_SLOT + GAP + 1 + TRAIL
}

/// Character offset of the close glyph within a tab's cell, measured from
/// after the icon.
fn close_offset(tab: &TabPresentation) -> usize {
    1 + tab.title.chars().count() + MARKER_SLOT + GAP
}

/// Where one tab sits, and which document it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct TabRects {
    /// The document this tab acts on. Identity travels with the rectangle so a
    /// click can never be resolved against a stale position.
    pub id: DocumentId,
    /// The whole cell, including the close control.
    pub full: Rect,
    /// The part that activates the document.
    pub body: Rect,
    /// The part that closes it.
    pub close: Rect,
    pub active: bool,
    pub dirty: bool,
    pub label: String,
    /// The file-type glyph drawn at the head of the cell, before `label`.
    pub icon: crate::icons::Glyph,
    /// The icon's own characteristic color -- unlike `label`, which dims when
    /// the tab is inactive, the icon keeps its color regardless, the same way
    /// `material-icon-theme` colors a tab in VS Code or Lapce.
    pub icon_color: crate::theme::Color,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabGeometry {
    pub tabs: Vec<TabRects>,
}

/// Lays out the tab bar.
///
/// `icon_width` is the icon font's advance, which differs from the monospace
/// `char_width`: every cell leads with a file-type glyph, and a rectangle
/// computed as though it did not would drift further from the drawn text with
/// every tab along the row.
pub fn geometry(
    bar: Rect,
    tabs: &[TabPresentation],
    char_width: f32,
    icon_width: f32,
    scale: f32,
) -> TabGeometry {
    let mut rects = Vec::with_capacity(tabs.len());
    let mut x = bar.x;
    for tab in tabs {
        // Lapce gives every tab a minimum width regardless of how short its
        // title is, so a one-character filename doesn't produce a sliver of
        // a tab.
        let natural = ICON_LEAD as f32 * char_width + icon_width + cell_chars(tab) as f32 * char_width;
        let width = natural.max(crate::layout::TAB_MIN_WIDTH * scale);
        let full = Rect::new(x, bar.y, width, bar.height);

        // The close control is a square around its glyph, inset so it does not
        // swallow clicks meant for the last character of the title.
        let glyph_x =
            x + ICON_LEAD as f32 * char_width + icon_width + close_offset(tab) as f32 * char_width;
        let side = bar.height.min(char_width * 2.0).max(char_width);
        let close = Rect::new(
            glyph_x - (side - char_width) / 2.0,
            bar.y + (bar.height - side) / 2.0,
            side,
            side,
        );
        let body = Rect::new(x, bar.y, (close.x - x).max(0.0), bar.height);

        let file_icon = icon(tab);
        rects.push(TabRects {
            id: tab.id,
            full,
            body,
            close,
            active: tab.active,
            dirty: tab.dirty,
            label: label(tab),
            icon: file_icon.into(),
            icon_color: file_icon.color(),
        });
        x += width + scale;
    }
    TabGeometry { tabs: rects }
}

/// What a click on the tab bar landed on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TabHit {
    /// Activate this document.
    Body(DocumentId),
    /// Close this document.
    Close(DocumentId),
    /// Empty tab bar space.
    None,
}

/// Resolves a click. The close control is tested first, so the two regions
/// cannot both claim a point and a close never activates the tab on its way.
pub fn hit(geometry: &TabGeometry, x: f32, y: f32) -> TabHit {
    for tab in &geometry.tabs {
        if tab.close.contains(x, y) {
            return TabHit::Close(tab.id);
        }
    }
    for tab in &geometry.tabs {
        if tab.body.contains(x, y) {
            return TabHit::Body(tab.id);
        }
    }
    TabHit::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presentation(id: u64, title: &str, active: bool, dirty: bool) -> TabPresentation {
        TabPresentation {
            id: DocumentId::new(id),
            title: title.to_string(),
            tooltip: None,
            dirty,
            active,
            loading: false,
        }
    }

    fn bar() -> Rect {
        Rect::new(0.0, 24.0, 800.0, 26.0)
    }

    fn three() -> Vec<TabPresentation> {
        vec![
            presentation(1, "main.rs", true, false),
            presentation(2, "lib.rs", false, true),
            presentation(3, "a-much-longer-name.txt", false, false),
        ]
    }

    #[test]
    fn tabs_are_laid_out_left_to_right_without_overlapping() {
        let geometry = geometry(bar(), &three(), 8.0, 14.0, 1.0);
        assert_eq!(geometry.tabs.len(), 3);
        for pair in geometry.tabs.windows(2) {
            assert!(pair[0].full.right() <= pair[1].full.x + 0.01, "tabs must not overlap");
        }
        for tab in &geometry.tabs {
            assert_eq!(tab.full.y, bar().y);
            assert_eq!(tab.full.height, bar().height);
        }
    }

    #[test]
    fn a_wider_title_gets_a_wider_tab() {
        let geometry = geometry(bar(), &three(), 8.0, 14.0, 1.0);
        assert!(
            geometry.tabs[2].full.width > geometry.tabs[0].full.width,
            "the long name needs more room"
        );
    }

    #[test]
    fn the_close_control_sits_inside_its_own_tab_and_not_over_the_body() {
        let geometry = geometry(bar(), &three(), 8.0, 14.0, 1.0);
        for tab in &geometry.tabs {
            assert!(tab.close.x >= tab.full.x, "the close control stays inside its tab");
            assert!(tab.close.right() <= tab.full.right() + 0.01);
            assert!(
                tab.body.right() <= tab.close.x + 0.01,
                "the body and the close control must not overlap"
            );
            assert!(tab.body.width > 0.0);
        }
    }

    #[test]
    fn clicking_a_tab_body_reports_that_document() {
        let geometry = geometry(bar(), &three(), 8.0, 14.0, 1.0);
        for tab in &geometry.tabs {
            let point = (tab.body.x + tab.body.width / 2.0, tab.body.y + tab.body.height / 2.0);
            assert_eq!(hit(&geometry, point.0, point.1), TabHit::Body(tab.id));
        }
    }

    #[test]
    fn clicking_the_close_control_closes_that_document_rather_than_activating_it() {
        let geometry = geometry(bar(), &three(), 8.0, 14.0, 1.0);
        for tab in &geometry.tabs {
            let point = (tab.close.x + tab.close.width / 2.0, tab.close.y + tab.close.height / 2.0);
            assert_eq!(
                hit(&geometry, point.0, point.1),
                TabHit::Close(tab.id),
                "the close control must never resolve to an activation"
            );
        }
    }

    #[test]
    fn hit_testing_carries_the_document_not_its_position() {
        // The second tab is removed; the third keeps its own identity even
        // though its index changed.
        let mut tabs = three();
        let third = tabs[2].id;
        tabs.remove(1);
        let geometry = geometry(bar(), &tabs, 8.0, 14.0, 1.0);
        let moved = &geometry.tabs[1];
        assert_eq!(moved.id, third);
        assert_eq!(hit(&geometry, moved.body.x + 4.0, moved.body.y + 4.0), TabHit::Body(third));
    }

    #[test]
    fn a_click_on_empty_tab_bar_space_hits_nothing() {
        let geometry = geometry(bar(), &three(), 8.0, 14.0, 1.0);
        let past_the_end = geometry.tabs.last().unwrap().full.right() + 20.0;
        assert_eq!(hit(&geometry, past_the_end, bar().y + 4.0), TabHit::None);
    }

    #[test]
    fn the_label_and_its_icon_are_exactly_as_wide_as_the_cell() {
        // The cell is an icon glyph plus a monospace label. Measuring it as
        // though it were only one or the other is how the drawn text and the
        // rectangle a click lands in drift apart along the row.
        for tab in three() {
            let width =
                geometry(bar(), std::slice::from_ref(&tab), 8.0, 14.0, 1.0).tabs[0].full.width;
            let natural = ICON_LEAD as f32 * 8.0 + 14.0 + label(&tab).chars().count() as f32 * 8.0;
            assert_eq!(
                natural.max(crate::layout::TAB_MIN_WIDTH),
                width,
                "drawn text and hit geometry must be the same width"
            );
        }
    }

    #[test]
    fn the_close_glyph_lands_where_the_close_rect_is() {
        // A long enough title that the cell is its natural width rather than
        // the minimum, so the glyph's own position is what is being checked.
        let tab = presentation(1, "a-much-longer-name.txt", true, false);
        let geometry = geometry(bar(), std::slice::from_ref(&tab), 8.0, 14.0, 1.0);
        let rects = &geometry.tabs[0];
        let glyph_index =
            rects.label.chars().position(|character| character == CLOSE_GLYPH).expect("drawn");
        // The icon is shaped after a one-character leading pad, so the
        // glyph's x starts a lead plus one icon advance in.
        let glyph_x = rects.full.x + ICON_LEAD as f32 * 8.0 + 14.0 + glyph_index as f32 * 8.0;
        assert!(
            rects.close.contains(glyph_x + 1.0, rects.close.y + 1.0),
            "the close rectangle must cover the glyph the user is aiming at"
        );
    }

    #[test]
    fn a_dirty_tab_is_marked_and_a_loading_tab_is_marked_differently() {
        let clean = presentation(1, "a.rs", false, false);
        let dirty = presentation(2, "b.rs", false, true);
        let mut loading = presentation(3, "c.rs", false, false);
        loading.loading = true;

        assert_eq!(marker(&clean), ' ');
        assert_eq!(marker(&dirty), '*');
        assert_eq!(marker(&loading), '.');
        assert!(label(&dirty).contains('*'));
    }

    #[test]
    fn scaling_the_font_moves_the_visual_and_the_hit_region_together() {
        // The rule that makes a future font-size change safe: both rectangles
        // come from the same computation, so neither can be updated alone.
        let small = geometry(bar(), &three(), 8.0, 14.0, 1.0);
        let large = geometry(bar(), &three(), 16.0, 28.0, 1.0);
        for (small, large) in small.tabs.iter().zip(large.tabs.iter()) {
            assert!(large.full.width > small.full.width);
            assert!(large.close.x > small.close.x);
            assert!(
                large.close.x >= large.body.right() - 0.01,
                "the close region moved with the glyph"
            );
        }
    }
}
