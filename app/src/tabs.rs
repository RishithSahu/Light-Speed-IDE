//! Tab bar geometry.
//!
//! One computation feeds both drawing and hit testing:
//!
//! ```text
//!            tabs::geometry(bar, presentations, char_width, scale)
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

/// Leading padding, in characters.
const LEAD: usize = 2;
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
pub fn label(tab: &TabPresentation) -> String {
    let mut text = String::with_capacity(tab.title.len() + LEAD + MARKER_SLOT + GAP + TRAIL + 1);
    text.push_str("  ");
    text.push_str(&tab.title);
    text.push(marker(tab));
    text.push_str("  ");
    text.push(CLOSE_GLYPH);
    text.push_str("  ");
    text
}

/// Characters in one tab's cell.
fn cell_chars(tab: &TabPresentation) -> usize {
    LEAD + tab.title.chars().count() + MARKER_SLOT + GAP + 1 + TRAIL
}

/// Character offset of the close glyph within a tab's cell.
fn close_offset(tab: &TabPresentation) -> usize {
    LEAD + tab.title.chars().count() + MARKER_SLOT + GAP
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
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabGeometry {
    pub tabs: Vec<TabRects>,
}

/// Lays out the tab bar.
pub fn geometry(bar: Rect, tabs: &[TabPresentation], char_width: f32, scale: f32) -> TabGeometry {
    let mut rects = Vec::with_capacity(tabs.len());
    let mut x = bar.x;
    for tab in tabs {
        let width = cell_chars(tab) as f32 * char_width;
        let full = Rect::new(x, bar.y, width, bar.height);

        // The close control is a square around its glyph, inset so it does not
        // swallow clicks meant for the last character of the title.
        let glyph_x = x + close_offset(tab) as f32 * char_width;
        let side = bar.height.min(char_width * 2.0).max(char_width);
        let close = Rect::new(
            glyph_x - (side - char_width) / 2.0,
            bar.y + (bar.height - side) / 2.0,
            side,
            side,
        );
        let body = Rect::new(x, bar.y, (close.x - x).max(0.0), bar.height);

        rects.push(TabRects {
            id: tab.id,
            full,
            body,
            close,
            active: tab.active,
            dirty: tab.dirty,
            label: label(tab),
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
        let geometry = geometry(bar(), &three(), 8.0, 1.0);
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
        let geometry = geometry(bar(), &three(), 8.0, 1.0);
        assert!(
            geometry.tabs[2].full.width > geometry.tabs[0].full.width,
            "the long name needs more room"
        );
    }

    #[test]
    fn the_close_control_sits_inside_its_own_tab_and_not_over_the_body() {
        let geometry = geometry(bar(), &three(), 8.0, 1.0);
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
        let geometry = geometry(bar(), &three(), 8.0, 1.0);
        for tab in &geometry.tabs {
            let point = (tab.body.x + tab.body.width / 2.0, tab.body.y + tab.body.height / 2.0);
            assert_eq!(hit(&geometry, point.0, point.1), TabHit::Body(tab.id));
        }
    }

    #[test]
    fn clicking_the_close_control_closes_that_document_rather_than_activating_it() {
        let geometry = geometry(bar(), &three(), 8.0, 1.0);
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
        let geometry = geometry(bar(), &tabs, 8.0, 1.0);
        let moved = &geometry.tabs[1];
        assert_eq!(moved.id, third);
        assert_eq!(hit(&geometry, moved.body.x + 4.0, moved.body.y + 4.0), TabHit::Body(third));
    }

    #[test]
    fn a_click_on_empty_tab_bar_space_hits_nothing() {
        let geometry = geometry(bar(), &three(), 8.0, 1.0);
        let past_the_end = geometry.tabs.last().unwrap().full.right() + 20.0;
        assert_eq!(hit(&geometry, past_the_end, bar().y + 4.0), TabHit::None);
    }

    #[test]
    fn the_label_is_exactly_as_wide_as_the_cell() {
        for tab in three() {
            let width = geometry(bar(), std::slice::from_ref(&tab), 8.0, 1.0).tabs[0].full.width;
            assert_eq!(
                label(&tab).chars().count() as f32 * 8.0,
                width,
                "drawn text and hit geometry must be the same width"
            );
        }
    }

    #[test]
    fn the_close_glyph_lands_where_the_close_rect_is() {
        let tab = presentation(1, "main.rs", true, false);
        let geometry = geometry(bar(), std::slice::from_ref(&tab), 8.0, 1.0);
        let rects = &geometry.tabs[0];
        let glyph_index =
            rects.label.chars().position(|character| character == CLOSE_GLYPH).expect("drawn");
        let glyph_x = rects.full.x + glyph_index as f32 * 8.0;
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
        let small = geometry(bar(), &three(), 8.0, 1.0);
        let large = geometry(bar(), &three(), 16.0, 1.0);
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
