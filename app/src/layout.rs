//! Window layout.
//!
//! ```text
//! +--------------------------------------------------+
//! | menu bar     File  Edit  View                    |
//! +--------------------------------------------------+
//! | tab bar                                          |
//! +--------+-----------------------------------------+
//! | gutter | text                                    |
//! +--------+-----------------------------------------+
//! | status bar                                       |
//! +--------------------------------------------------+
//! ```
//!
//! All rectangles are in physical pixels: layout happens once per frame from
//! the window size and the measured font metrics, and nothing downstream has to
//! think about scale factors.

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Rect { x, y, width, height }
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }
}

/// Font measurements taken from the shaped text, not assumed.
#[derive(Copy, Clone, Debug)]
pub struct FontMetrics {
    pub font_size: f32,
    pub line_height: f32,
    /// Advance width of one digit, used for the gutter and for scroll steps.
    pub digit_width: f32,
}

#[derive(Copy, Clone, Debug)]
pub struct Layout {
    pub scale: f32,
    pub window: Rect,
    pub menu_bar: Rect,
    pub tab_bar: Rect,
    pub gutter: Rect,
    pub text: Rect,
    pub status_bar: Rect,
    pub scrollbar: Rect,
    pub metrics: FontMetrics,
    pub show_line_numbers: bool,
    pub show_status_bar: bool,
}

impl Layout {
    /// Lays the window out. The status bar can be hidden from the View menu, in
    /// which case its height goes to the editor rather than leaving a gap.
    #[allow(clippy::too_many_arguments)]
    pub fn with_chrome(
        width: f32,
        height: f32,
        scale: f32,
        metrics: FontMetrics,
        line_number_digits: usize,
        show_line_numbers: bool,
        show_status_bar: bool,
    ) -> Self {
        let menu_height = (metrics.line_height + 8.0 * scale).round();
        let tab_bar_height = (metrics.line_height + 10.0 * scale).round();
        let status_height =
            if show_status_bar { (metrics.line_height + 6.0 * scale).round() } else { 0.0 };
        let gutter_width = if show_line_numbers {
            (metrics.digit_width * line_number_digits.max(3) as f32 + 16.0 * scale).round()
        } else {
            6.0 * scale
        };
        let scrollbar_width = 8.0 * scale;

        let menu_bar = Rect::new(0.0, 0.0, width, menu_height);
        let tab_bar = Rect::new(0.0, menu_bar.bottom(), width, tab_bar_height);
        let body_top = tab_bar.bottom();
        let body_height = (height - menu_height - tab_bar_height - status_height).max(0.0);
        let gutter = Rect::new(0.0, body_top, gutter_width, body_height);
        let text = Rect::new(
            gutter_width,
            body_top,
            (width - gutter_width - scrollbar_width).max(0.0),
            body_height,
        );
        let scrollbar = Rect::new(text.right(), body_top, scrollbar_width, body_height);
        let status_bar = Rect::new(0.0, body_top + body_height, width, status_height);

        Layout {
            scale,
            window: Rect::new(0.0, 0.0, width, height),
            menu_bar,
            tab_bar,
            gutter,
            text,
            status_bar,
            scrollbar,
            metrics,
            show_line_numbers,
            show_status_bar,
        }
    }

    /// Whole lines that fit in the text area.
    pub fn visible_lines(&self) -> usize {
        if self.metrics.line_height <= 0.0 {
            return 1;
        }
        ((self.text.height / self.metrics.line_height).floor() as usize).max(1)
    }

    /// Lines that fit including a partially visible last line, which is what the
    /// snapshot should carry so scrolling has no gap at the bottom.
    pub fn visible_lines_with_partial(&self) -> usize {
        if self.metrics.line_height <= 0.0 {
            return 1;
        }
        ((self.text.height / self.metrics.line_height).ceil() as usize + 1).max(1)
    }

    /// Columns that fit in the text area, used to bound snapshot line length.
    pub fn visible_columns(&self) -> usize {
        if self.metrics.digit_width <= 0.0 {
            return 80;
        }
        ((self.text.width / self.metrics.digit_width).ceil() as usize).max(8)
    }

    /// Top of a visible row, given how far the view is scrolled within a line.
    pub fn row_top(&self, row: usize, scroll_fraction: f32) -> f32 {
        self.text.y + row as f32 * self.metrics.line_height - scroll_fraction
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The usual layout, with every piece of chrome shown.
    fn compute(
        width: f32,
        height: f32,
        scale: f32,
        metrics: FontMetrics,
        digits: usize,
        show_line_numbers: bool,
    ) -> Layout {
        Layout::with_chrome(width, height, scale, metrics, digits, show_line_numbers, true)
    }

    fn metrics() -> FontMetrics {
        FontMetrics { font_size: 14.0, line_height: 20.0, digit_width: 8.0 }
    }

    #[test]
    fn panels_tile_the_window_without_gaps() {
        let layout = compute(1000.0, 700.0, 1.0, metrics(), 4, true);
        assert_eq!(layout.menu_bar.y, 0.0);
        assert_eq!(layout.tab_bar.y, layout.menu_bar.bottom());
        assert_eq!(layout.gutter.y, layout.tab_bar.bottom());
        assert_eq!(layout.text.y, layout.gutter.y);
        assert_eq!(layout.status_bar.y, layout.text.bottom());
        assert!((layout.status_bar.bottom() - 700.0).abs() < 0.5);
        assert_eq!(layout.text.x, layout.gutter.right());
        assert_eq!(layout.scrollbar.x, layout.text.right());
    }

    #[test]
    fn hiding_line_numbers_shrinks_the_gutter() {
        let with = compute(1000.0, 700.0, 1.0, metrics(), 5, true);
        let without = compute(1000.0, 700.0, 1.0, metrics(), 5, false);
        assert!(without.gutter.width < with.gutter.width);
        assert!(without.text.width > with.text.width);
    }

    #[test]
    fn visible_line_count_follows_the_text_height() {
        let layout = compute(1000.0, 700.0, 1.0, metrics(), 4, true);
        let expected = (layout.text.height / 20.0).floor() as usize;
        assert_eq!(layout.visible_lines(), expected);
        assert!(layout.visible_lines_with_partial() > layout.visible_lines());
    }

    #[test]
    fn a_tiny_window_still_reports_one_line() {
        let layout = compute(200.0, 10.0, 1.0, metrics(), 4, true);
        assert_eq!(layout.visible_lines(), 1);
        assert!(layout.text.height >= 0.0);
    }

    #[test]
    fn hiding_the_status_bar_gives_its_height_to_the_editor() {
        let with = compute(1000.0, 700.0, 1.0, metrics(), 4, true);
        let without = Layout::with_chrome(1000.0, 700.0, 1.0, metrics(), 4, true, false);
        assert_eq!(without.status_bar.height, 0.0);
        assert!(without.text.height > with.text.height);
        assert!((without.text.bottom() - 700.0).abs() < 0.5, "the editor reaches the bottom");
    }

    #[test]
    fn the_menu_bar_sits_above_everything() {
        let layout = compute(1000.0, 700.0, 1.0, metrics(), 4, true);
        assert!(layout.menu_bar.height > 0.0);
        assert!(layout.menu_bar.bottom() <= layout.tab_bar.y);
        assert!(layout.tab_bar.bottom() <= layout.text.y);
    }

    #[test]
    fn scaling_grows_the_chrome() {
        let scaled = compute(2000.0, 1400.0, 2.0, metrics(), 4, true);
        let unscaled = compute(1000.0, 700.0, 1.0, metrics(), 4, true);
        assert!(scaled.tab_bar.height > unscaled.tab_bar.height);
    }

    #[test]
    fn rect_hit_testing() {
        let rect = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert!(rect.contains(10.0, 20.0));
        assert!(rect.contains(109.0, 69.0));
        assert!(!rect.contains(110.0, 40.0));
        assert!(!rect.contains(50.0, 19.0));
    }
}
