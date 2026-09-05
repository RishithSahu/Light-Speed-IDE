//! Command palette: the floating, fuzzy-filtered command list opened from
//! the title bar's own command field (or Ctrl+Shift+P) -- Lapce's primary
//! way to run anything, since it has no classic File/Edit/View menu bar for
//! most of what a menu would otherwise hold.
//!
//! Filtering is a pure function of the registry and the typed query, for the
//! same reason `wheel_target` and `tabs::geometry` are: the ordering and
//! matching rules are worth asserting on directly, without a window or a
//! real `EditorCore`.

use crate::layout::Rect;
use ls_core::CommandDescriptor;

/// Rows shown without scrolling -- a command palette is a quick-launch
/// tool, not a browsable list, so a query specific enough to narrow past
/// this many matches is doing its job rather than hitting a limit that
/// needs a scrollbar.
pub const MAX_VISIBLE_ROWS: usize = 12;

/// One command still available to run, matched against the typed query.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PaletteRow {
    pub id: &'static str,
    pub display_name: &'static str,
}

/// Filters `commands` to the ones `enabled` currently allows and whose
/// display name contains `query` case-insensitively, keeping the registry's
/// own order (already grouped sensibly by area) rather than re-sorting by
/// match quality.
///
/// A disabled command never appears: showing an entry only to have it refuse
/// to run is worse than not listing it, and the palette has no separate
/// "greyed out" treatment the way the dropdown menu does.
pub fn filter(
    commands: &'static [CommandDescriptor],
    query: &str,
    enabled: impl Fn(&'static str) -> bool,
) -> Vec<PaletteRow> {
    let query = query.to_ascii_lowercase();
    commands
        .iter()
        .filter(|command| enabled(command.id))
        .filter(|command| {
            query.is_empty() || command.display_name.to_ascii_lowercase().contains(&query)
        })
        .map(|command| PaletteRow { id: command.id, display_name: command.display_name })
        .collect()
}

/// The palette's floating panel: centered horizontally, sitting near the
/// top of the window the way VS Code's and Lapce's own palettes do (a
/// modal in the middle of the screen would bury the very document it is
/// there to jump around in), sized to its query row plus however many
/// (capped) rows are actually showing -- never taller than the content, the
/// same discipline the sidebar's own content-sized text buffer follows.
pub fn geometry(window: Rect, row_count: usize, line_height: f32, scale: f32) -> Rect {
    let width = (520.0 * scale).min(window.width * 0.7).max(240.0 * scale);
    let visible_rows = row_count.min(MAX_VISIBLE_ROWS);
    // +1 for the query row itself, which is not one of `row_count`.
    let height = line_height * (visible_rows as f32 + 1.0);
    let x = window.x + (window.width - width) / 2.0;
    let y = window.y + 72.0 * scale;
    Rect::new(x, y, width, height)
}

/// Which filtered row (if any) a point falls in, given the panel and one
/// line's height. The panel's first line is the query field, not a
/// selectable row, so a point there (or outside the panel) yields `None`.
pub fn row_hit(panel: Rect, line_height: f32, x: f32, y: f32) -> Option<usize> {
    if line_height <= 0.0 || !panel.contains(x, y) {
        return None;
    }
    let list_top = panel.y + line_height;
    if y < list_top {
        return None;
    }
    Some(((y - list_top) / line_height) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always_enabled(_id: &'static str) -> bool {
        true
    }

    fn sample() -> &'static [CommandDescriptor] {
        ls_core::commands::all()
    }

    #[test]
    fn an_empty_query_lists_every_enabled_command_in_registry_order() {
        let rows = filter(sample(), "", always_enabled);
        assert_eq!(rows.len(), sample().len());
        assert_eq!(rows[0].id, sample()[0].id);
    }

    #[test]
    fn the_query_matches_the_display_name_case_insensitively() {
        let rows = filter(sample(), "SAVE AS", always_enabled);
        assert!(
            rows.iter().any(|row| row.display_name.eq_ignore_ascii_case("Save As")),
            "expected a \"Save As\"-ish command in {rows:?}"
        );
        assert!(
            rows.iter().all(|row| row.display_name.to_ascii_lowercase().contains("save as")),
            "every row must actually match the query"
        );
    }

    #[test]
    fn a_disabled_command_never_appears_even_if_its_name_matches() {
        let target = sample()[0].id;
        let rows = filter(sample(), "", |id| id != target);
        assert!(rows.iter().all(|row| row.id != target));
        assert_eq!(rows.len(), sample().len() - 1);
    }

    #[test]
    fn a_query_matching_nothing_yields_an_empty_list_not_a_panic() {
        let rows = filter(sample(), "no command could ever be named this", always_enabled);
        assert!(rows.is_empty());
    }

    #[test]
    fn the_panel_is_centered_and_sized_to_its_own_content_not_the_window() {
        let window = Rect::new(0.0, 0.0, 1000.0, 700.0);
        let panel = geometry(window, 3, 20.0, 1.0);
        assert!(
            (panel.x + panel.width / 2.0 - window.width / 2.0).abs() < 0.5,
            "the panel must be horizontally centered in the window"
        );
        assert_eq!(panel.height, 20.0 * 4.0, "one query row plus 3 result rows");
    }

    #[test]
    fn the_panel_never_grows_past_its_row_cap_no_matter_how_many_commands_match() {
        let window = Rect::new(0.0, 0.0, 1000.0, 700.0);
        let panel = geometry(window, 500, 20.0, 1.0);
        assert_eq!(panel.height, 20.0 * (MAX_VISIBLE_ROWS as f32 + 1.0));
    }

    #[test]
    fn a_click_on_the_query_row_hits_no_result_row() {
        let panel = Rect::new(100.0, 50.0, 400.0, 100.0);
        assert_eq!(row_hit(panel, 20.0, 150.0, 55.0), None, "the query row itself, not a result");
        assert_eq!(row_hit(panel, 20.0, 150.0, 10.0), None, "above the panel entirely");
    }

    #[test]
    fn a_click_below_the_query_row_hits_the_result_row_under_it() {
        let panel = Rect::new(100.0, 50.0, 400.0, 100.0);
        // Query row occupies [50, 70); the first result row is [70, 90).
        assert_eq!(row_hit(panel, 20.0, 150.0, 75.0), Some(0));
        assert_eq!(row_hit(panel, 20.0, 150.0, 95.0), Some(1));
    }
}
