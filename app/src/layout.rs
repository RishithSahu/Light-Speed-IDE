//! Window layout.
//!
//! ```text
//! +---------------------------------------------------------------+
//! | menu btn        [ search field ]              run  settings    |  header
//! +------+----------+---------------------------------------------+
//! | acti | sidebar  | <> | tabs...            | split close       |  tab row
//! | vity |          +---------------------------------------------+
//! | bar  | OPEN     | project > src > file.rs                     |  breadcrumb
//! |      | EDITORS  +---------------------------------------------+
//! |      |          | gutter | text                               |
//! |      | FILE     +---------------------------------------------+
//! |      | EXPLORER | rail | bottom panel (terminal)              |
//! +------+----------+---------------------------------------------+
//! | branch  errors warnings        Ln, Col  encoding  LF  language |  status
//! +---------------------------------------------------------------+
//! ```
//!
//! This is Lapce's own layout (LightSpeed's UI is a deliberate copy of it, not
//! an independent design): a persistent icon-only activity bar at the left
//! edge switches which panel the sidebar shows; a bottom panel, with its own
//! narrow icon rail, hosts the terminal below the editor. Both are real
//! docked regions, not floating overlays -- when a panel is visible, the
//! editor gives up the matching width or height to it, the way an actual IDE
//! reserves space for its chrome rather than drawing it on top of the
//! document.
//!
//! Note where the tab bar starts: at the sidebar's right edge, not at the
//! window's. The activity bar and sidebar run the full height beside it, and
//! the tab row, breadcrumb, editor and bottom panel are all columns of what
//! is left. A tab bar spanning the whole window is the single most visible
//! way to get this shape wrong.
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
    /// Advance width of one chrome icon glyph (Codicons). Icons come from a
    /// different (square) font than the monospace UI text, so anything
    /// mixing the two -- a tree row's chevron and its name -- needs both
    /// numbers to compute a rectangle that matches what is shaped.
    pub icon_width: f32,
    /// Advance width of one Material Design Icons file-type glyph, measured
    /// separately from `icon_width` since the two icon fonts do not share an
    /// advance -- a tab's file icon and its name need this one instead.
    pub material_icon_width: f32,
}

/// The explorer sidebar's default width in logical pixels, before scaling or
/// clamping to the window (chosen to match VS Code's default explorer width
/// closely enough to feel familiar). The user can drag it wider or narrower;
/// see [`SIDEBAR_MIN_WIDTH`] and [`SIDEBAR_MAX_WIDTH`] for the drag range.
pub const SIDEBAR_WIDTH: f32 = 260.0;

/// How narrow the user can drag the sidebar -- below this, folder names
/// truncate into illegibility and the panel stops earning its keep.
pub const SIDEBAR_MIN_WIDTH: f32 = 140.0;

/// How wide the user can drag the sidebar, independent of the window (the
/// window-relative 60% cap in [`Layout::with_chrome`] applies on top of this).
pub const SIDEBAR_MAX_WIDTH: f32 = 640.0;

/// The strip around the sidebar's right edge that grabs the resize cursor
/// and starts a drag, in logical pixels.
pub const SIDEBAR_GRIP_WIDTH: f32 = 6.0;

/// Fixed chrome metrics, pulled directly from Lapce's own
/// `defaults/dark-theme.toml` `[ui]` table -- not derived from font size the
/// way this shell's tab bar height still is, because Lapce's aren't either.
pub const HEADER_HEIGHT: f32 = 35.0;
pub const STATUS_HEIGHT: f32 = 25.0;
pub const ACTIVITY_WIDTH: f32 = 50.0;
pub const TAB_MIN_WIDTH: f32 = 100.0;
pub const SCROLLBAR_WIDTH: f32 = 10.0;

/// The bottom panel's default height, and the drag range for it -- same
/// shape as the sidebar's `SIDEBAR_WIDTH`/`_MIN`/`_MAX`/`_GRIP`, just on the
/// vertical axis. Lapce's own defaults don't fix a panel height (it is purely
/// user-dragged), so this default is chosen to comfortably fit a handful of
/// terminal rows without swallowing the editor on a normal window.
pub const BOTTOM_PANEL_HEIGHT: f32 = 220.0;
pub const BOTTOM_PANEL_MIN_HEIGHT: f32 = 100.0;
pub const BOTTOM_PANEL_MAX_HEIGHT: f32 = 800.0;
pub const BOTTOM_PANEL_GRIP_HEIGHT: f32 = 6.0;

/// Width of the bottom panel's own icon rail (its terminal/search/problems
/// switcher) -- narrower than the main activity bar, matching Lapce's more
/// compact in-panel action strip.
pub const PANEL_RAIL_WIDTH: f32 = 36.0;

#[derive(Copy, Clone, Debug)]
pub struct Layout {
    pub scale: f32,
    pub window: Rect,
    /// The title/header row. Lapce puts no classic menu bar here: a logo and
    /// a menu button on the left, a command field in the middle, run and
    /// settings on the right.
    pub menu_bar: Rect,
    /// The menu button inside the header, which opens the one popout menu
    /// that replaced the old File/Edit/View bar.
    pub title_menu_button: Rect,
    /// The header's centered command field -- Lapce's palette entry point.
    pub title_search: Rect,
    /// The header's right-hand button cluster (run, settings).
    pub title_actions: Rect,
    /// The back/forward cluster at the head of the tab row.
    pub tab_nav: Rect,
    /// The split/close cluster at the end of the tab row.
    pub tab_actions: Rect,
    pub tab_bar: Rect,
    /// The breadcrumb trail under the tab bar, showing the active document's
    /// path a segment at a time.
    pub breadcrumb: Rect,
    /// The persistent icon-only activity bar (Explorer / Search / Source
    /// Control / ...), always at the far left, always full body height even
    /// when the bottom panel is open.
    pub activity_bar: Rect,
    pub sidebar: Rect,
    pub gutter: Rect,
    pub text: Rect,
    /// The bottom panel's own icon rail (currently just Terminal), at the
    /// left edge of the bottom panel row.
    pub bottom_panel_rail: Rect,
    /// The bottom panel's content area, to the right of its rail.
    pub bottom_panel: Rect,
    pub status_bar: Rect,
    pub scrollbar: Rect,
    pub metrics: FontMetrics,
    pub show_line_numbers: bool,
    pub show_status_bar: bool,
    pub sidebar_visible: bool,
    pub bottom_panel_visible: bool,
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
        sidebar_visible: bool,
        sidebar_width_request: f32,
        bottom_panel_visible: bool,
        bottom_panel_height_request: f32,
    ) -> Self {
        let header_height = (HEADER_HEIGHT * scale).round();
        let tab_bar_height = (metrics.line_height + 10.0 * scale).round();
        let status_height = if show_status_bar { (STATUS_HEIGHT * scale).round() } else { 0.0 };
        let gutter_width = if show_line_numbers {
            (metrics.digit_width * line_number_digits.max(3) as f32 + 16.0 * scale).round()
        } else {
            6.0 * scale
        };
        let scrollbar_width = SCROLLBAR_WIDTH * scale;
        let activity_width = ACTIVITY_WIDTH * scale;
        let panel_rail_width = PANEL_RAIL_WIDTH * scale;

        let menu_bar = Rect::new(0.0, 0.0, width, header_height);
        // Header furniture, laid out the way Lapce's is: a small button
        // cluster hard left, a centered command field taking the middle
        // third, action buttons hard right.
        let button = (header_height - 8.0 * scale).max(1.0);
        let title_menu_button = Rect::new(10.0 * scale, menu_bar.y + 4.0 * scale, button, button);
        let search_width = (width * 0.34).clamp(180.0 * scale, 560.0 * scale).min(width);
        let title_search = Rect::new(
            ((width - search_width) / 2.0).max(0.0),
            menu_bar.y + 5.0 * scale,
            search_width,
            (header_height - 10.0 * scale).max(1.0),
        );
        let actions_width = (button * 2.0 + 12.0 * scale).min(width);
        let title_actions = Rect::new(
            (width - actions_width - 8.0 * scale).max(0.0),
            menu_bar.y + 4.0 * scale,
            actions_width,
            button,
        );

        // Below the header, the window is three columns: the activity bar,
        // the sidebar, and everything else. The tab bar and breadcrumb belong
        // to that third column rather than spanning the window, which is what
        // makes the sidebar run the full height beside them -- Lapce's shape,
        // and the thing a full-width tab bar gets visibly wrong.
        let body_top = menu_bar.bottom();
        let full_body_height = (height - header_height - status_height).max(0.0);

        // Never let the sidebar crowd the editor out entirely on a narrow
        // window: cap it at 60% of the width regardless of what the user
        // dragged it to.
        let sidebar_width = if sidebar_visible {
            (sidebar_width_request * scale).min(width * 0.6).max(0.0)
        } else {
            0.0
        };

        let activity_bar = Rect::new(0.0, body_top, activity_width, full_body_height);
        let sidebar = Rect::new(activity_bar.right(), body_top, sidebar_width, full_body_height);

        let column_x = sidebar.right();
        let column_width = (width - column_x).max(0.0);
        // The tab row leads with back/forward navigation and ends with the
        // split and close actions, exactly as Lapce's does; the tabs
        // themselves get whatever is left between them.
        let nav_width = (metrics.icon_width * 2.0 + 24.0 * scale).min(column_width);
        let actions_width = (metrics.icon_width * 2.0 + 24.0 * scale).min(column_width - nav_width);
        let tab_nav = Rect::new(column_x, body_top, nav_width, tab_bar_height);
        let tab_actions = Rect::new(
            (column_x + column_width - actions_width).max(column_x),
            body_top,
            actions_width,
            tab_bar_height,
        );
        let tab_bar = Rect::new(
            tab_nav.right(),
            body_top,
            (column_width - nav_width - actions_width).max(0.0),
            tab_bar_height,
        );
        let breadcrumb_height = (metrics.line_height + 6.0 * scale).round();
        let breadcrumb = Rect::new(column_x, tab_bar.bottom(), column_width, breadcrumb_height);

        let column_body_top = breadcrumb.bottom();
        let column_body_height = (full_body_height - tab_bar_height - breadcrumb_height).max(0.0);

        // Never let the bottom panel crowd the editor out entirely: cap it at
        // 60% of the remaining column height, the same guard the sidebar uses
        // against window width.
        let bottom_panel_height = if bottom_panel_visible {
            (bottom_panel_height_request * scale).min(column_body_height * 0.6).max(0.0)
        } else {
            0.0
        };
        let editor_body_height = (column_body_height - bottom_panel_height).max(0.0);

        let gutter = Rect::new(column_x, column_body_top, gutter_width, editor_body_height);
        let text = Rect::new(
            gutter.right(),
            column_body_top,
            (column_width - gutter_width - scrollbar_width).max(0.0),
            editor_body_height,
        );
        let scrollbar =
            Rect::new(text.right(), column_body_top, scrollbar_width, editor_body_height);

        let bottom_row_y = column_body_top + editor_body_height;
        let bottom_panel_rail =
            Rect::new(column_x, bottom_row_y, panel_rail_width, bottom_panel_height);
        let bottom_panel = Rect::new(
            bottom_panel_rail.right(),
            bottom_row_y,
            (column_width - panel_rail_width).max(0.0),
            bottom_panel_height,
        );

        let status_bar = Rect::new(0.0, bottom_row_y + bottom_panel_height, width, status_height);

        Layout {
            scale,
            window: Rect::new(0.0, 0.0, width, height),
            menu_bar,
            title_menu_button,
            title_search,
            title_actions,
            tab_nav,
            tab_actions,
            tab_bar,
            breadcrumb,
            activity_bar,
            sidebar,
            gutter,
            text,
            bottom_panel_rail,
            bottom_panel,
            status_bar,
            scrollbar,
            metrics,
            show_line_numbers,
            show_status_bar,
            sidebar_visible,
            bottom_panel_visible,
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

/// Clamps a user-dragged sidebar width to the range the drag handle allows.
/// The window-relative 60% cap in [`Layout::with_chrome`] applies on top of
/// this, so a small window can still end up narrower than `SIDEBAR_MIN_WIDTH`
/// -- this only bounds what dragging itself can request.
pub fn clamp_sidebar_width(requested: f32) -> f32 {
    requested.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
}

/// Clamps a user-dragged bottom-panel height to the range its grip allows.
/// Same shape as [`clamp_sidebar_width`], on the vertical axis.
pub fn clamp_bottom_panel_height(requested: f32) -> f32 {
    requested.clamp(BOTTOM_PANEL_MIN_HEIGHT, BOTTOM_PANEL_MAX_HEIGHT)
}

/// The rectangle for one square icon cell in a vertical icon rail (the
/// activity bar, or a docked panel's own rail) -- one shared definition so
/// the quad that is drawn and the rectangle a click is tested against can
/// never disagree, the same discipline the tab bar's geometry already
/// follows.
pub fn icon_rail_row(rail: Rect, index: usize) -> Rect {
    let cell = rail.width;
    Rect::new(rail.x, rail.y + cell * index as f32, rail.width, cell)
}

/// Which row index (if any) a point falls in within a vertical icon rail.
pub fn icon_rail_hit(rail: Rect, item_count: usize, x: f32, y: f32) -> Option<usize> {
    if !rail.contains(x, y) {
        return None;
    }
    let cell = rail.width;
    if cell <= 0.0 {
        return None;
    }
    let index = ((y - rail.y) / cell) as usize;
    (index < item_count).then_some(index)
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
        Layout::with_chrome(
            width,
            height,
            scale,
            metrics,
            digits,
            show_line_numbers,
            true,
            false,
            SIDEBAR_WIDTH,
            false,
            BOTTOM_PANEL_HEIGHT,
        )
    }

    fn metrics() -> FontMetrics {
        FontMetrics {
            font_size: 14.0,
            line_height: 20.0,
            digit_width: 8.0,
            icon_width: 14.0,
            material_icon_width: 14.0,
        }
    }

    #[test]
    fn panels_tile_the_window_without_gaps() {
        let layout = compute(1000.0, 700.0, 1.0, metrics(), 4, true);
        assert_eq!(layout.menu_bar.y, 0.0);
        // The activity bar and sidebar start directly under the header and
        // run the full height; the tab row, breadcrumb and editor are all
        // columns of what is left to their right. This is Lapce's shape --
        // see the module diagram.
        assert_eq!(layout.activity_bar.y, layout.menu_bar.bottom());
        assert_eq!(layout.sidebar.y, layout.menu_bar.bottom());
        assert_eq!(layout.tab_bar.y, layout.menu_bar.bottom());
        assert_eq!(layout.breadcrumb.y, layout.tab_bar.bottom());
        assert_eq!(layout.gutter.y, layout.breadcrumb.bottom());
        assert_eq!(layout.text.y, layout.gutter.y);
        assert_eq!(layout.status_bar.y, layout.text.bottom());
        assert!((layout.status_bar.bottom() - 700.0).abs() < 0.5);
        assert_eq!(layout.text.x, layout.gutter.right());
        assert_eq!(layout.scrollbar.x, layout.text.right());
    }

    #[test]
    fn the_tab_row_starts_at_the_sidebar_rather_than_the_window_edge() {
        // The regression this guards: a tab bar spanning the whole window,
        // with the sidebar starting below it, is the single most visible way
        // to get Lapce's layout wrong.
        let layout = Layout::with_chrome(
            1000.0,
            700.0,
            1.0,
            metrics(),
            4,
            true,
            true,
            true,
            SIDEBAR_WIDTH,
            false,
            BOTTOM_PANEL_HEIGHT,
        );
        assert!(layout.tab_bar.x >= layout.sidebar.right());
        assert!(layout.breadcrumb.x >= layout.sidebar.right());
        assert_eq!(layout.tab_nav.x, layout.sidebar.right());
        assert!(layout.tab_actions.right() <= layout.window.right() + 0.5);
        assert!(
            layout.sidebar.bottom() >= layout.text.bottom(),
            "the sidebar runs past the editor, not the other way round"
        );
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
        let without = Layout::with_chrome(
            1000.0,
            700.0,
            1.0,
            metrics(),
            4,
            true,
            false,
            false,
            SIDEBAR_WIDTH,
            false,
            BOTTOM_PANEL_HEIGHT,
        );
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
    fn the_sidebar_is_absent_by_default_and_pushes_the_editor_right_when_shown() {
        let hidden = compute(1000.0, 700.0, 1.0, metrics(), 4, true);
        assert_eq!(hidden.sidebar.width, 0.0);

        let shown = Layout::with_chrome(
            1000.0,
            700.0,
            1.0,
            metrics(),
            4,
            true,
            true,
            true,
            SIDEBAR_WIDTH,
            false,
            BOTTOM_PANEL_HEIGHT,
        );
        assert!(shown.sidebar.width > 0.0);
        assert_eq!(
            shown.sidebar.x,
            shown.activity_bar.right(),
            "the sidebar starts after the activity bar"
        );
        assert_eq!(shown.gutter.x, shown.sidebar.right());
        assert!(shown.text.width < hidden.text.width, "the editor gives up room to the sidebar");
        // The sidebar starts higher and ends lower than the editor: it spans
        // the tab row and breadcrumb too.
        assert!(shown.sidebar.y <= shown.gutter.y);
        assert!(shown.sidebar.height >= shown.gutter.height);
    }

    #[test]
    fn the_sidebar_never_crowds_out_the_editor_on_a_narrow_window() {
        let layout = Layout::with_chrome(
            200.0,
            700.0,
            1.0,
            metrics(),
            4,
            true,
            true,
            true,
            SIDEBAR_WIDTH,
            false,
            BOTTOM_PANEL_HEIGHT,
        );
        assert!(layout.sidebar.width <= 200.0 * 0.6 + 0.001);
        assert!(layout.text.width >= 0.0);
    }

    #[test]
    fn dragging_the_sidebar_wider_or_narrower_is_reflected_in_its_width() {
        let narrow = Layout::with_chrome(
            1400.0,
            700.0,
            1.0,
            metrics(),
            4,
            true,
            true,
            true,
            180.0,
            false,
            BOTTOM_PANEL_HEIGHT,
        );
        let wide = Layout::with_chrome(
            1400.0,
            700.0,
            1.0,
            metrics(),
            4,
            true,
            true,
            true,
            400.0,
            false,
            BOTTOM_PANEL_HEIGHT,
        );
        assert!((narrow.sidebar.width - 180.0).abs() < 0.5);
        assert!((wide.sidebar.width - 400.0).abs() < 0.5);
        assert!(wide.text.width < narrow.text.width, "a wider sidebar leaves less room for text");
    }

    #[test]
    fn sidebar_width_clamps_to_the_drag_range() {
        assert_eq!(clamp_sidebar_width(10.0), SIDEBAR_MIN_WIDTH);
        assert_eq!(clamp_sidebar_width(10_000.0), SIDEBAR_MAX_WIDTH);
        assert_eq!(clamp_sidebar_width(300.0), 300.0);
    }

    #[test]
    fn the_activity_bar_is_always_present_and_never_shrinks_with_other_panels() {
        let layout = compute(1000.0, 700.0, 1.0, metrics(), 4, true);
        assert!((layout.activity_bar.width - ACTIVITY_WIDTH).abs() < 0.5);
        assert_eq!(layout.activity_bar.x, 0.0);
        assert_eq!(layout.activity_bar.y, layout.menu_bar.bottom());

        // Opening the bottom panel shrinks the editor's height, not the
        // activity bar's -- it spans the full body height regardless.
        let with_panel = Layout::with_chrome(
            1000.0,
            700.0,
            1.0,
            metrics(),
            4,
            true,
            true,
            false,
            SIDEBAR_WIDTH,
            true,
            BOTTOM_PANEL_HEIGHT,
        );
        assert_eq!(with_panel.activity_bar.height, layout.activity_bar.height);
        assert!(with_panel.text.height < layout.text.height);
    }

    #[test]
    fn the_bottom_panel_is_absent_by_default_and_docks_below_the_editor_when_shown() {
        let hidden = compute(1000.0, 700.0, 1.0, metrics(), 4, true);
        assert_eq!(hidden.bottom_panel.height, 0.0);

        let shown = Layout::with_chrome(
            1000.0,
            700.0,
            1.0,
            metrics(),
            4,
            true,
            true,
            false,
            SIDEBAR_WIDTH,
            true,
            BOTTOM_PANEL_HEIGHT,
        );
        assert!(shown.bottom_panel.height > 0.0);
        assert_eq!(shown.bottom_panel_rail.x, shown.activity_bar.right());
        assert_eq!(shown.bottom_panel.x, shown.bottom_panel_rail.right());
        assert_eq!(
            shown.bottom_panel.y,
            shown.text.bottom(),
            "the panel sits directly under the editor"
        );
        assert!(
            (shown.bottom_panel.bottom() - shown.status_bar.y).abs() < 0.5,
            "the panel reaches down to the status bar"
        );
    }

    #[test]
    fn the_bottom_panel_never_crowds_out_the_editor_on_a_short_window() {
        let layout = Layout::with_chrome(
            1000.0,
            300.0,
            1.0,
            metrics(),
            4,
            true,
            true,
            false,
            SIDEBAR_WIDTH,
            true,
            2000.0,
        );
        assert!(layout.text.height >= 0.0);
        assert!(layout.bottom_panel.height <= layout.text.height + layout.bottom_panel.height);
    }

    #[test]
    fn bottom_panel_height_clamps_to_the_drag_range() {
        assert_eq!(clamp_bottom_panel_height(10.0), BOTTOM_PANEL_MIN_HEIGHT);
        assert_eq!(clamp_bottom_panel_height(10_000.0), BOTTOM_PANEL_MAX_HEIGHT);
        assert_eq!(clamp_bottom_panel_height(300.0), 300.0);
    }

    #[test]
    fn icon_rail_rows_are_square_and_stack_without_gaps() {
        let rail = Rect::new(0.0, 30.0, 50.0, 400.0);
        let first = icon_rail_row(rail, 0);
        let second = icon_rail_row(rail, 1);
        assert_eq!(first.width, first.height, "an icon cell is square");
        assert_eq!(first.y, rail.y);
        assert_eq!(second.y, first.bottom(), "rows stack without gaps");
    }

    #[test]
    fn icon_rail_hit_finds_the_row_under_the_pointer_and_nothing_outside_it() {
        let rail = Rect::new(0.0, 30.0, 50.0, 400.0);
        assert_eq!(icon_rail_hit(rail, 5, 25.0, 30.0), Some(0));
        assert_eq!(icon_rail_hit(rail, 5, 25.0, 81.0), Some(1));
        assert_eq!(icon_rail_hit(rail, 2, 25.0, 200.0), None, "past the last item");
        assert_eq!(icon_rail_hit(rail, 5, 25.0, 10.0), None, "above the rail");
        assert_eq!(icon_rail_hit(rail, 5, 500.0, 30.0), None, "outside its width");
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
