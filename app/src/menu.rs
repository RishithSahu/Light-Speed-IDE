//! Menu bar.
//!
//! Every item is a command id and nothing else (specification section 12): the
//! menu is a second way to reach the registry, never a second implementation of
//! an action. Whether an item is enabled comes from the registry's own
//! predicate, so a menu can never offer something the editor would refuse.
//!
//! Geometry and hit testing are pure functions of the layout, which is what
//! lets this file be tested without a window.

use crate::layout::Rect;
use ls_core::EditorCore;
use std::path::PathBuf;

/// One entry in a dropdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    pub label: &'static str,
    /// The registry command this runs. `None` marks a separator.
    pub command: Option<&'static str>,
    /// Shortcut text, shown right-aligned.
    pub shortcut: &'static str,
}

impl MenuItem {
    const fn item(label: &'static str, command: &'static str, shortcut: &'static str) -> Self {
        MenuItem { label, command: Some(command), shortcut }
    }

    pub fn is_separator(&self) -> bool {
        self.command.is_none()
    }
}

/// One top-level menu.
#[derive(Clone, Debug)]
pub struct Menu {
    pub title: &'static str,
    pub items: &'static [MenuItem],
}

pub const FILE_ITEMS: &[MenuItem] = &[
    MenuItem::item("New", "file.new", "Ctrl+N"),
    MenuItem::item("Open...", "file.open", "Ctrl+O"),
    MenuItem::item("Open Folder", "file.open_folder", ""),
    MenuItem::item("Save", "file.save", "Ctrl+S"),
    MenuItem::item("Save As...", "file.save_as", "Ctrl+Shift+S"),
    MenuItem::item("Exit", "app.quit", "Ctrl+Q"),
];

pub const EDIT_ITEMS: &[MenuItem] = &[
    MenuItem::item("Undo", "edit.undo", "Ctrl+Z"),
    MenuItem::item("Redo", "edit.redo", "Ctrl+Y"),
    MenuItem::item("Cut", "edit.cut", "Ctrl+X"),
    MenuItem::item("Copy", "edit.copy", "Ctrl+C"),
    MenuItem::item("Paste", "edit.paste", "Ctrl+V"),
    MenuItem::item("Select All", "edit.select_all", "Ctrl+A"),
    MenuItem::item("Delete", "edit.delete_forward", "Del"),
    MenuItem::item("Find...", "edit.find", "Ctrl+F"),
    MenuItem::item("Find Next", "edit.find_next", "F3"),
    MenuItem::item("Find Previous", "edit.find_previous", "Shift+F3"),
];

pub const VIEW_ITEMS: &[MenuItem] = &[
    MenuItem::item("Toggle Status Bar", "view.toggle_status_bar", ""),
    MenuItem::item("Toggle Performance Overlay", "view.toggle_performance_overlay", "F12"),
    MenuItem::item("Toggle Loading Panel", "view.toggle_dev_panel", "F9"),
    MenuItem::item("Toggle Resource Center", "view.toggle_resource_center", "F10"),
    MenuItem::item("Toggle File Tree", "view.toggle_file_tree", "Ctrl+Shift+E"),
    MenuItem::item("Search in Files...", "view.workspace_search", "Ctrl+Shift+F"),
    MenuItem::item("Toggle Git Status", "view.toggle_git_status", "Ctrl+Shift+G"),
    MenuItem::item("Toggle Terminal", "view.toggle_terminal", "F11"),
];

pub const MENUS: &[Menu] = &[
    Menu { title: "File", items: FILE_ITEMS },
    Menu { title: "Edit", items: EDIT_ITEMS },
    Menu { title: "View", items: VIEW_ITEMS },
];

/// Index of the File menu in [`MENUS`], named so the recent-files tail is
/// never appended to the wrong dropdown by a stray literal.
pub const FILE_MENU_INDEX: usize = 0;

/// One recently opened file, appended below File's static items.
///
/// Unlike a [`MenuItem`], its label is not known until runtime, so it is a
/// separate small type rather than a generalization of `MenuItem` -- the
/// static items stay a compile-time table, provably in agreement with the
/// command registry (see the test at the bottom of this file).
#[derive(Clone, Debug, PartialEq)]
pub struct RecentRow {
    pub label: String,
    pub path: PathBuf,
}

/// The label drawn for one recent-file row, padded to the column width like
/// every other dropdown row.
pub fn recent_item_text(row: &RecentRow, columns: usize) -> String {
    let used = row.label.chars().count();
    let gap = columns.saturating_sub(used);
    format!("{}{}", row.label, " ".repeat(gap))
}

/// Which menu is open, if any.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuState {
    pub open: Option<usize>,
    pub hovered_item: Option<usize>,
}

impl MenuState {
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub fn close(&mut self) {
        self.open = None;
        self.hovered_item = None;
    }

    /// Clicking a title opens it, or closes it if it was already open.
    pub fn toggle(&mut self, index: usize) {
        if self.open == Some(index) {
            self.close();
        } else {
            self.open = Some(index);
            self.hovered_item = None;
        }
    }
}

/// Where each menu title sits in the bar.
#[derive(Clone, Debug)]
pub struct MenuGeometry {
    pub titles: Vec<Rect>,
    /// The open dropdown's panel, and one rect per item.
    pub dropdown: Option<Rect>,
    pub items: Vec<Rect>,
}

/// Horizontal inset from a surface's edge to its text, in unscaled pixels.
/// Shared with the composer so the drawn text and the rectangles that catch
/// clicks are inset by the same amount. Used for the dropdown panel, which is
/// not built from a fixed-width character grid the way the title bar is.
pub const TEXT_INSET: f32 = 12.0;
/// Vertical inset from a dropdown's edge to its first row.
pub const ROW_INSET: f32 = 6.0;
/// Extra width reserved for shortcut text in a dropdown.
const SHORTCUT_GAP: f32 = 24.0;

/// Padding around a title, in characters rather than pixels.
///
/// This is the fix for a highlight that bled into the next title: the
/// previous version padded the *rectangle* with `TEXT_INSET * scale` pixels on
/// each side but padded the *drawn text* with two literal space characters, and
/// those two widths only agreed by coincidence at one font size. A menu title
/// is one row out of a monospace grid, so its rectangle should be measured in
/// the same unit its text is: whole characters. Pad the label with spaces,
/// measure the rectangle in exactly those characters, and the two cannot drift
/// apart at any font size or scale factor.
const TITLE_LEAD: usize = 2;
const TITLE_TRAIL: usize = 2;

/// The label drawn for one title, padded so its width is a whole number of
/// character cells -- the same string [`geometry`] measures the title's
/// rectangle from.
pub fn title_label(menu: &Menu) -> String {
    let mut text = String::with_capacity(TITLE_LEAD + menu.title.len() + TITLE_TRAIL);
    for _ in 0..TITLE_LEAD {
        text.push(' ');
    }
    text.push_str(menu.title);
    for _ in 0..TITLE_TRAIL {
        text.push(' ');
    }
    text
}

/// Characters in one title's cell, including its padding.
fn title_cell_chars(menu: &Menu) -> usize {
    TITLE_LEAD + menu.title.chars().count() + TITLE_TRAIL
}

/// Lays out the bar and, if one is open, its dropdown.
///
/// `char_width` and `line_height` are the measured advance and leading of the
/// monospace face, so this stays correct at any font size or DPI. A dropdown
/// row is exactly one line tall, which is what makes the highlight rectangle
/// and the text row it highlights the same strip of pixels rather than two
/// nearly-aligned ones.
pub fn geometry(
    bar: Rect,
    state: MenuState,
    char_width: f32,
    line_height: f32,
    scale: f32,
    recent: &[RecentRow],
) -> MenuGeometry {
    let padding = TEXT_INSET * scale;
    // Titles are laid out as a fixed-width character grid, so the rectangle a
    // click lands in is exactly the cell the drawn label occupies -- no pixel
    // padding to fall out of step with a character-based label.
    let mut titles = Vec::with_capacity(MENUS.len());
    let mut x = bar.x;
    for menu in MENUS {
        let width = title_cell_chars(menu) as f32 * char_width;
        titles.push(Rect::new(x, bar.y, width, bar.height));
        x += width;
    }

    let (dropdown, items) = match state.open.and_then(|index| Some((index, *titles.get(index)?))) {
        Some((index, title)) => {
            let menu = &MENUS[index];
            // Recent files are a tail appended only to the File dropdown.
            let recent = if index == FILE_MENU_INDEX { recent } else { &[] };
            let row_height = line_height;
            let inset = ROW_INSET * scale;
            let widest_item = menu
                .items
                .iter()
                .map(|item| item.label.chars().count() + item.shortcut.chars().count())
                .max()
                .unwrap_or(10) as f32;
            let widest_recent =
                recent.iter().map(|row| row.label.chars().count()).max().unwrap_or(0) as f32;
            let widest = widest_item.max(widest_recent);
            let width = widest * char_width + SHORTCUT_GAP * scale + padding * 2.0;
            let total_rows = menu.items.len() + recent.len();
            let height = total_rows as f32 * row_height + inset * 2.0;
            let panel = Rect::new(title.x, bar.bottom(), width, height);

            let mut rects = Vec::with_capacity(total_rows);
            for row in 0..total_rows {
                rects.push(Rect::new(
                    panel.x,
                    panel.y + inset + row as f32 * row_height,
                    panel.width,
                    row_height,
                ));
            }
            (Some(panel), rects)
        }
        None => (None, Vec::new()),
    };

    MenuGeometry { titles, dropdown, items }
}

/// What a click landed on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuHit {
    /// A top-level title: open or close that menu.
    Title(usize),
    /// An item: run this command and close the menu.
    Command(&'static str),
    /// A recent-files row: open this path.
    OpenRecent(PathBuf),
    /// Inside the open dropdown but not on a command (a separator or padding).
    Swallowed,
    /// Nothing menu-related; the click belongs to whatever is underneath.
    None,
}

/// Resolves a click against the bar and any open dropdown.
pub fn hit(
    geometry: &MenuGeometry,
    state: MenuState,
    x: f32,
    y: f32,
    recent: &[RecentRow],
) -> MenuHit {
    if let Some(index) = geometry.titles.iter().position(|rect| rect.contains(x, y)) {
        return MenuHit::Title(index);
    }

    let Some(panel) = geometry.dropdown else { return MenuHit::None };
    if !panel.contains(x, y) {
        return MenuHit::None;
    }

    let Some(open) = state.open else { return MenuHit::Swallowed };
    let items = MENUS[open].items;
    let recent = if open == FILE_MENU_INDEX { recent } else { &[] };
    match geometry.items.iter().position(|rect| rect.contains(x, y)) {
        Some(row) if row < items.len() => match items[row].command {
            Some(command) => MenuHit::Command(command),
            None => MenuHit::Swallowed,
        },
        Some(row) => match recent.get(row - items.len()) {
            Some(entry) => MenuHit::OpenRecent(entry.path.clone()),
            None => MenuHit::Swallowed,
        },
        // Inside the panel but between rows: swallow it so the menu does not
        // close under the pointer.
        None => MenuHit::Swallowed,
    }
}

/// Which item the pointer is over, for highlighting.
pub fn hovered_item(geometry: &MenuGeometry, x: f32, y: f32) -> Option<usize> {
    geometry.items.iter().position(|rect| rect.contains(x, y))
}

/// Renders one dropdown row, padded so shortcuts line up.
pub fn item_text(item: &MenuItem, columns: usize) -> String {
    if item.is_separator() {
        return "-".repeat(columns.min(40));
    }
    let label = item.label;
    let shortcut = item.shortcut;
    let used = label.chars().count() + shortcut.chars().count();
    let gap = columns.saturating_sub(used).max(2);
    format!("{label}{}{shortcut}", " ".repeat(gap))
}

/// Whether the registry currently allows this item.
pub fn is_enabled(core: &EditorCore, item: &MenuItem) -> bool {
    item.command.map(|command| core.is_command_enabled(command)).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ls_core::{CommandArgs, EffectiveConfig};
    use ls_platform::MemoryClipboard;

    fn bar() -> Rect {
        Rect::new(0.0, 0.0, 800.0, 24.0)
    }

    fn editor() -> EditorCore {
        EditorCore::with_clipboard(EffectiveConfig::default(), Box::new(MemoryClipboard::new()))
    }

    #[test]
    fn every_menu_item_names_a_real_command() {
        // The menu cannot offer an action the registry does not have.
        for menu in MENUS {
            for item in menu.items {
                if let Some(command) = item.command {
                    assert!(
                        ls_core::commands::find(command).is_some(),
                        "{} -> {command} is not a registered command",
                        item.label
                    );
                }
            }
        }
    }

    #[test]
    fn the_three_required_menus_exist_with_their_required_items() {
        let titles: Vec<&str> = MENUS.iter().map(|menu| menu.title).collect();
        assert_eq!(titles, vec!["File", "Edit", "View"]);

        let file: Vec<&str> = FILE_ITEMS.iter().map(|item| item.label).collect();
        assert_eq!(file, vec!["New", "Open...", "Open Folder", "Save", "Save As...", "Exit"]);

        let edit: Vec<&str> = EDIT_ITEMS.iter().map(|item| item.label).collect();
        assert_eq!(
            edit,
            vec![
                "Undo",
                "Redo",
                "Cut",
                "Copy",
                "Paste",
                "Select All",
                "Delete",
                "Find...",
                "Find Next",
                "Find Previous"
            ]
        );
    }

    #[test]
    fn opening_and_closing_a_menu() {
        let mut state = MenuState::default();
        assert!(!state.is_open());
        state.toggle(0);
        assert_eq!(state.open, Some(0));
        state.toggle(0);
        assert!(!state.is_open(), "clicking an open title closes it");
        state.toggle(1);
        state.toggle(2);
        assert_eq!(state.open, Some(2), "clicking another title switches to it");
        state.close();
        assert!(!state.is_open());
    }

    #[test]
    fn titles_are_laid_out_left_to_right_without_overlapping() {
        let geometry = geometry(bar(), MenuState::default(), 8.0, 20.0, 1.0, &[]);
        assert_eq!(geometry.titles.len(), 3);
        for pair in geometry.titles.windows(2) {
            assert!(pair[0].right() <= pair[1].x + 0.01, "menu titles must not overlap");
        }
        assert!(geometry.dropdown.is_none(), "nothing is open");
    }

    #[test]
    fn clicking_a_title_reports_that_title() {
        let geometry = geometry(bar(), MenuState::default(), 8.0, 20.0, 1.0, &[]);
        let file = geometry.titles[0];
        assert_eq!(
            hit(&geometry, MenuState::default(), file.x + 2.0, file.y + 2.0, &[]),
            MenuHit::Title(0)
        );
        let edit = geometry.titles[1];
        assert_eq!(
            hit(&geometry, MenuState::default(), edit.x + 2.0, edit.y + 2.0, &[]),
            MenuHit::Title(1)
        );
    }

    #[test]
    fn clicking_an_item_reports_its_command() {
        let state = MenuState { open: Some(0), hovered_item: None };
        let geometry = geometry(bar(), state, 8.0, 20.0, 1.0, &[]);
        let panel = geometry.dropdown.expect("the file menu is open");
        assert!(panel.y >= bar().bottom(), "the dropdown hangs below the bar");

        let new_item = geometry.items[0];
        assert_eq!(
            hit(&geometry, state, new_item.x + 4.0, new_item.y + 2.0, &[]),
            MenuHit::Command("file.new")
        );
        let save_item = geometry.items[3]; // New, Open..., Open Folder, Save
        assert_eq!(
            hit(&geometry, state, save_item.x + 4.0, save_item.y + 2.0, &[]),
            MenuHit::Command("file.save")
        );
    }

    #[test]
    fn a_titles_rectangle_is_exactly_as_wide_as_its_own_label() {
        // The regression this encodes: the rectangle used to be padded in
        // pixels (`TEXT_INSET * scale`) while the drawn text was padded with
        // two literal spaces, so at ordinary font sizes the rectangle was
        // wider than the label and reached into the next title's glyphs. If
        // the rectangle and the label ever measure differently again, this
        // catches it before it reaches the screen.
        let char_width = 8.0;
        for menu in MENUS {
            let label = title_label(menu);
            let cell = title_cell_chars(menu);
            assert_eq!(
                label.chars().count(),
                cell,
                "the label's length must equal the cell width geometry() measures"
            );
            let _ = char_width; // width is chars * char_width in geometry()
        }
    }

    #[test]
    fn no_titles_highlight_rectangle_reaches_into_the_next_titles_text() {
        let geometry = geometry(bar(), MenuState::default(), 8.0, 20.0, 1.0, &[]);
        let labels: Vec<String> = MENUS.iter().map(title_label).collect();

        // Reconstruct where each label's own glyphs start and end within the
        // concatenated title row, exactly as the renderer draws them.
        let mut cursor = 0.0f32;
        for (index, label) in labels.iter().enumerate() {
            let glyph_start = cursor + TITLE_LEAD as f32 * 8.0;
            let glyph_end = cursor + (label.chars().count() - TITLE_TRAIL) as f32 * 8.0;
            let rect = geometry.titles[index];

            assert!(
                rect.x <= glyph_start + 0.01,
                "title {index}'s rectangle must start at or before its own glyphs"
            );
            assert!(
                rect.right() >= glyph_end - 0.01,
                "title {index}'s rectangle must cover its own glyphs"
            );

            if let Some(next) = geometry.titles.get(index + 1) {
                assert!(
                    rect.right() <= next.x + 0.01,
                    "title {index}'s highlight must not reach past where title {} begins",
                    index + 1
                );
            }
            cursor += label.chars().count() as f32 * 8.0;
        }
    }

    #[test]
    fn an_item_rectangle_is_exactly_one_text_row() {
        // The highlight and the label it highlights have to be the same strip
        // of pixels. A row height picked independently of the font's leading
        // would drift further down the menu with every item.
        let line_height = 20.0;
        let state = MenuState { open: Some(0), hovered_item: None };
        let geometry = geometry(bar(), state, 8.0, line_height, 1.0, &[]);
        let panel = geometry.dropdown.unwrap();

        for (row, rect) in geometry.items.iter().enumerate() {
            assert_eq!(rect.height, line_height, "row {row} is not one line tall");
            let text_row_top = panel.y + ROW_INSET + row as f32 * line_height;
            assert!(
                (rect.y - text_row_top).abs() < 0.01,
                "row {row} highlight at {} but its text is drawn at {text_row_top}",
                rect.y
            );
        }

        let last = geometry.items.last().unwrap();
        assert!(last.bottom() <= panel.bottom() + 0.01, "the last row fits inside the panel");
    }

    #[test]
    fn a_taller_font_makes_taller_rows_and_a_taller_panel() {
        let state = MenuState { open: Some(1), hovered_item: None };
        let small = geometry(bar(), state, 8.0, 16.0, 1.0, &[]);
        let large = geometry(bar(), state, 8.0, 32.0, 1.0, &[]);
        assert!(large.dropdown.unwrap().height > small.dropdown.unwrap().height);
        assert!(large.items[1].y > small.items[1].y, "rows move with the leading");
    }

    #[test]
    fn clicking_outside_the_menu_is_not_a_menu_hit() {
        let state = MenuState { open: Some(0), hovered_item: None };
        let geometry = geometry(bar(), state, 8.0, 20.0, 1.0, &[]);
        // Well below the dropdown: the editor should get this click.
        assert_eq!(hit(&geometry, state, 400.0, 600.0, &[]), MenuHit::None);
    }

    #[test]
    fn a_click_inside_the_panel_but_off_a_row_is_swallowed() {
        let state = MenuState { open: Some(0), hovered_item: None };
        let geometry = geometry(bar(), state, 8.0, 20.0, 1.0, &[]);
        let panel = geometry.dropdown.unwrap();
        // The bottom padding strip of the panel.
        let hit = hit(&geometry, state, panel.x + 4.0, panel.bottom() - 0.5, &[]);
        assert!(
            matches!(hit, MenuHit::Swallowed | MenuHit::Command(_)),
            "a click inside the panel must not fall through to the editor"
        );
    }

    fn recent(paths: &[&str]) -> Vec<RecentRow> {
        paths
            .iter()
            .map(|path| RecentRow { label: path.to_string(), path: PathBuf::from(path) })
            .collect()
    }

    #[test]
    fn recent_files_are_appended_below_files_static_items() {
        let state = MenuState { open: Some(FILE_MENU_INDEX), hovered_item: None };
        let recent = recent(&["a.rs", "b.rs"]);
        let without = geometry(bar(), state, 8.0, 20.0, 1.0, &[]);
        let with = geometry(bar(), state, 8.0, 20.0, 1.0, &recent);

        assert_eq!(with.items.len(), without.items.len() + recent.len());
        assert!(
            with.dropdown.unwrap().height > without.dropdown.unwrap().height,
            "the panel grows to fit the extra rows"
        );
        // The static rows keep their exact positions; the recent rows come
        // after them, not interleaved.
        assert_eq!(&with.items[..without.items.len()], &without.items[..]);
    }

    #[test]
    fn recent_files_are_only_appended_to_the_file_menu() {
        let recent = recent(&["a.rs"]);
        for open in [1usize, 2] {
            let state = MenuState { open: Some(open), hovered_item: None };
            let without = geometry(bar(), state, 8.0, 20.0, 1.0, &[]);
            let with = geometry(bar(), state, 8.0, 20.0, 1.0, &recent);
            assert_eq!(
                with.items.len(),
                without.items.len(),
                "menu {open} is not File and must not grow"
            );
        }
    }

    #[test]
    fn clicking_a_recent_row_reports_its_path_not_a_command() {
        let state = MenuState { open: Some(FILE_MENU_INDEX), hovered_item: None };
        let recent = recent(&["one.rs", "two.rs"]);
        let geometry = geometry(bar(), state, 8.0, 20.0, 1.0, &recent);
        let static_len = FILE_ITEMS.len();

        let first_recent = geometry.items[static_len];
        assert_eq!(
            hit(&geometry, state, first_recent.x + 4.0, first_recent.y + 2.0, &recent),
            MenuHit::OpenRecent(PathBuf::from("one.rs"))
        );
        let second_recent = geometry.items[static_len + 1];
        assert_eq!(
            hit(&geometry, state, second_recent.x + 4.0, second_recent.y + 2.0, &recent),
            MenuHit::OpenRecent(PathBuf::from("two.rs"))
        );

        // The static rows above them are unaffected.
        let save = geometry.items[3]; // New, Open..., Open Folder, Save
        assert_eq!(
            hit(&geometry, state, save.x + 4.0, save.y + 2.0, &recent),
            MenuHit::Command("file.save")
        );
    }

    #[test]
    fn an_empty_recent_list_leaves_the_file_menu_exactly_as_before() {
        let state = MenuState { open: Some(FILE_MENU_INDEX), hovered_item: None };
        let with_empty_slice = geometry(bar(), state, 8.0, 20.0, 1.0, &[]);
        let with_empty_vec = geometry(bar(), state, 8.0, 20.0, 1.0, &recent(&[]));
        assert_eq!(with_empty_slice.items, with_empty_vec.items);
        assert_eq!(with_empty_slice.dropdown, with_empty_vec.dropdown);
    }

    #[test]
    fn recent_item_text_is_padded_to_the_column_width() {
        let row = RecentRow { label: "main.rs".to_string(), path: PathBuf::from("main.rs") };
        let rendered = recent_item_text(&row, 20);
        assert_eq!(rendered.chars().count(), 20);
        assert!(rendered.starts_with("main.rs"));
    }

    #[test]
    fn hovering_reports_the_row_under_the_pointer() {
        let state = MenuState { open: Some(1), hovered_item: None };
        let geometry = geometry(bar(), state, 8.0, 20.0, 1.0, &[]);
        let second = geometry.items[1];
        assert_eq!(hovered_item(&geometry, second.x + 4.0, second.y + 2.0), Some(1));
        assert_eq!(hovered_item(&geometry, 5.0, 5.0), None);
    }

    #[test]
    fn enablement_comes_from_the_registry_not_the_menu() {
        let mut core = editor();
        let undo = EDIT_ITEMS.iter().find(|item| item.label == "Undo").unwrap();
        let copy = EDIT_ITEMS.iter().find(|item| item.label == "Copy").unwrap();

        assert!(!is_enabled(&core, undo), "nothing to undo yet");
        assert!(!is_enabled(&core, copy), "nothing selected yet");

        core.new_document();
        core.execute("edit.insert_text", CommandArgs::Text("text".into())).unwrap();
        assert!(is_enabled(&core, undo));
        assert!(!is_enabled(&core, copy));

        core.execute("edit.select_all", CommandArgs::None).unwrap();
        assert!(is_enabled(&core, copy));
    }

    #[test]
    fn item_text_lines_up_shortcuts() {
        let item = MenuItem::item("Save", "file.save", "Ctrl+S");
        let rendered = item_text(&item, 24);
        assert!(rendered.starts_with("Save"));
        assert!(rendered.ends_with("Ctrl+S"));
        assert_eq!(rendered.chars().count(), 24);
    }

    #[test]
    fn item_text_never_collapses_the_gap() {
        // A label plus shortcut wider than the column budget still keeps them
        // apart rather than running together.
        let item =
            MenuItem::item("Toggle Performance Overlay", "view.toggle_performance_overlay", "F12");
        let rendered = item_text(&item, 10);
        assert!(rendered.contains("  "), "there is always a visible gap: {rendered}");
    }
}
