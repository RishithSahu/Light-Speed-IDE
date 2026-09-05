//! The settings screen: where everything on it sits, and what a click hits.
//!
//! # Geometry, not drawing
//!
//! Everything here is a pure function of the pane, the font metrics and the
//! current state. Nothing touches the GPU, nothing reads application state,
//! and the whole screen can therefore be asserted on directly -- which row a
//! setting landed on, which control a click at some pixel resolves to --
//! without a window. The renderer is handed the result and draws it; the
//! shell is handed the hit and acts on it.
//!
//! # Why the list is one text region and not a widget tree
//!
//! The text engine shapes one buffer per named region, drawn from one
//! origin. A settings list is a column of rows, which is exactly what a
//! single multi-line buffer already is, so the list is composed as rich text
//! -- title, description and value each on their own line, coloured by role
//! -- and the interactive parts (checkboxes, fields, option pills) are quads
//! laid over measured positions. That reuses the machinery the sidebar and
//! the command palette already use rather than inventing a second one.
//!
//! # Rows are measured once
//!
//! [`layout`] walks the visible settings once and produces a [`Placement`]
//! per setting: which text row it starts on, and the rectangle of its
//! control. Hit-testing and drawing both read that, so a click can never
//! resolve against a layout different from the one on screen.

use crate::layout::Rect;
use ls_core::settings::{Applies, SettingDescriptor, SettingKind, Settings};

/// Rows of text one setting occupies: title, description, value.
pub const ROWS_PER_SETTING: usize = 3;
/// Blank row between settings.
pub const ROWS_BETWEEN: usize = 1;
/// Rows a section heading occupies, blank line included.
pub const ROWS_PER_HEADING: usize = 2;
/// Width of the section list down the left, in characters.
pub const CATEGORY_COLUMNS: usize = 22;
/// Left inset of the list's text, in characters.
const INDENT: usize = 2;
/// Width of a text or number field, in characters.
const FIELD_COLUMNS: usize = 28;

/// One setting, as placed on screen.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub key: &'static str,
    /// Row the title sits on, counted from the top of the list.
    pub title_row: usize,
    /// The clickable control: a checkbox, a field, or the option pills.
    pub control: Rect,
    /// Where each option of a choice sits, in the same order as the kind's
    /// list. Empty for every other kind.
    pub options: Vec<Rect>,
    /// The "Reset" affordance, present only while the value has been changed.
    pub reset: Option<Rect>,
}

/// The whole screen, measured.
#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    pub search: Rect,
    pub categories: Rect,
    pub list: Rect,
    /// One entry per section shown down the left, in `SECTIONS` order.
    pub category_rows: Vec<Rect>,
    pub placements: Vec<Placement>,
    /// Total rows the list occupies, for the scrollbar and clamping.
    pub total_rows: usize,
}

/// What a click landed on.
#[derive(Clone, Debug, PartialEq)]
pub enum Hit {
    Search,
    /// A section in the left-hand list, by index into `SECTIONS`.
    Category(usize),
    /// A checkbox or a field, to be toggled or focused.
    Control(&'static str),
    /// One option of a choice: the setting, and the option's text.
    Option(&'static str, &'static str),
    /// The reset affordance beside a changed setting.
    Reset(&'static str),
    /// Empty space in the list, which only takes focus away from a field.
    Nothing,
}

impl Default for Screen {
    fn default() -> Self {
        let nothing = Rect::new(0.0, 0.0, 0.0, 0.0);
        Screen {
            search: nothing,
            categories: nothing,
            list: nothing,
            category_rows: Vec::new(),
            placements: Vec::new(),
            total_rows: 0,
        }
    }
}

/// Splits `pane` into the search box, the section list and the settings list.
pub fn frame(pane: Rect, line_height: f32, digit_width: f32) -> (Rect, Rect, Rect) {
    let padding = (line_height * 0.6).round();
    let search_height = (line_height * 1.8).round();
    let search = Rect::new(
        pane.x + padding,
        pane.y + padding,
        (pane.width - padding * 2.0).max(1.0),
        search_height,
    );
    let below = search.bottom() + padding;
    let categories_width = (CATEGORY_COLUMNS as f32 * digit_width).min(pane.width * 0.4);
    let categories =
        Rect::new(pane.x + padding, below, categories_width, (pane.bottom() - below).max(1.0));
    let list_x = categories.right() + padding;
    let list = Rect::new(list_x, below, (pane.right() - list_x - padding).max(1.0), categories.height);
    (search, categories, list)
}

/// Measures the screen for the settings `visible` in the order given.
///
/// `scroll` is in rows rather than pixels: rows are what the list is made of,
/// so scrolling by them keeps every line on the text grid and out of the
/// half-pixel blur that a free-scrolling list of shaped text falls into.
pub fn layout(
    pane: Rect,
    line_height: f32,
    digit_width: f32,
    visible: &[&'static SettingDescriptor],
    settings: &Settings,
    scroll_rows: usize,
) -> Screen {
    let (search, categories, list) = frame(pane, line_height, digit_width);

    let category_rows = ls_core::settings::SECTIONS
        .iter()
        .enumerate()
        .map(|(at, _)| {
            Rect::new(categories.x, categories.y + at as f32 * line_height, categories.width, line_height)
        })
        .collect();

    let mut placements = Vec::with_capacity(visible.len());
    let mut row = 0usize;
    let mut section: Option<&str> = None;
    for setting in visible {
        if section != Some(setting.section) {
            section = Some(setting.section);
            row += ROWS_PER_HEADING;
        }
        let title_row = row;
        // The value sits on the third row of the block.
        let value_row = row + 2;
        let drawn = value_row as isize - scroll_rows as isize;
        let y = list.y + drawn as f32 * line_height;
        let x = list.x + INDENT as f32 * digit_width;

        let (control, options) = match setting.kind {
            SettingKind::Bool => (
                Rect::new(x, y + line_height * 0.15, line_height * 0.7, line_height * 0.7),
                Vec::new(),
            ),
            SettingKind::Choice(list_of) => {
                let mut options = Vec::with_capacity(list_of.len());
                let mut at = x;
                for option in list_of {
                    let width = (option.chars().count() + 4) as f32 * digit_width;
                    options.push(Rect::new(at, y, width, line_height));
                    at += width + digit_width;
                }
                let span = Rect::new(x, y, (at - x - digit_width).max(0.0), line_height);
                (span, options)
            }
            _ => (Rect::new(x, y, FIELD_COLUMNS as f32 * digit_width, line_height), Vec::new()),
        };

        let reset = (!settings.is_default(setting.key)).then(|| {
            Rect::new(control.right() + digit_width * 2.0, y, 6.0 * digit_width, line_height)
        });

        placements.push(Placement { key: setting.key, title_row, control, options, reset });
        row += ROWS_PER_SETTING + ROWS_BETWEEN;
    }

    Screen { search, categories, list, category_rows, placements, total_rows: row }
}

/// How many rows of the list fit on screen at once.
pub fn visible_rows(list: Rect, line_height: f32) -> usize {
    (list.height / line_height.max(1.0)).floor().max(1.0) as usize
}

/// The furthest the list may be scrolled, so it always shows something.
pub fn max_scroll(screen: &Screen, line_height: f32) -> usize {
    let fits = visible_rows(screen.list, line_height);
    screen.total_rows.saturating_sub(fits.saturating_sub(1))
}

/// What is at a point.
///
/// Options are tested before the control they sit inside, and the reset
/// affordance before the row it shares, so the smaller target always wins --
/// otherwise the enclosing control would swallow every click meant for
/// something within it.
///
/// `visible` is looked up rather than indexed. The screen was measured on a
/// previous frame, and a search typed since then can leave a shorter list
/// behind: indexing crashed the application outright, and a click landing on
/// nothing for one frame is the right answer.
pub fn hit(screen: &Screen, visible: &[&'static SettingDescriptor], x: f32, y: f32) -> Hit {
    if screen.search.contains(x, y) {
        return Hit::Search;
    }
    if screen.categories.contains(x, y) {
        for (at, row) in screen.category_rows.iter().enumerate() {
            if row.contains(x, y) {
                return Hit::Category(at);
            }
        }
        return Hit::Nothing;
    }
    if !screen.list.contains(x, y) {
        return Hit::Nothing;
    }
    for (at, placement) in screen.placements.iter().enumerate() {
        if let Some(reset) = placement.reset {
            if reset.contains(x, y) {
                return Hit::Reset(placement.key);
            }
        }
        let Some(setting) = visible.get(at) else { return Hit::Nothing };
        if let SettingKind::Choice(options) = setting.kind {
            for (option, rect) in options.iter().zip(placement.options.iter()) {
                if rect.contains(x, y) {
                    return Hit::Option(placement.key, option);
                }
            }
        }
        if placement.control.contains(x, y) {
            return Hit::Control(placement.key);
        }
    }
    Hit::Nothing
}

/// The text of the settings list, as rows.
///
/// Returned as rows rather than one string so the renderer can colour each by
/// role and skip the ones scrolled out of view.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Blank,
    Heading(String),
    Title(String),
    Description(String),
    /// The value line. The control is drawn over it, so the text is only what
    /// sits beside the control: the value of a field, or nothing for a
    /// checkbox.
    Value(String),
}

/// Builds the rows for `visible`, in the order [`layout`] measured them.
///
/// `editing` is the field being typed into and what has been typed so far.
/// A field has to show its draft rather than the stored value, or typing
/// into one looks like nothing is happening -- the value only reaches the
/// store when the edit is committed.
pub fn rows(
    visible: &[&'static SettingDescriptor],
    settings: &Settings,
    editing: Option<(&str, &str)>,
) -> Vec<Row> {
    let mut rows = Vec::with_capacity(visible.len() * (ROWS_PER_SETTING + ROWS_BETWEEN));
    let mut section: Option<&str> = None;
    for setting in visible {
        if section != Some(setting.section) {
            section = Some(setting.section);
            rows.push(Row::Heading(setting.section.to_string()));
            rows.push(Row::Blank);
        }
        let mut title = format!("{}: {}", short_section(setting.section), setting.title);
        if setting.applies == Applies::OnRestart {
            title.push_str("   (takes effect on restart)");
        }
        if !settings.is_default(setting.key) {
            title.push_str("   \u{2022} modified");
        }
        rows.push(Row::Title(title));
        rows.push(Row::Description(setting.description.to_string()));
        rows.push(Row::Value(match setting.kind {
            // The checkbox is a quad; the words beside it say what ticking
            // it does, the way VS Code words its own.
            SettingKind::Bool => format!("    {}", setting.title),
            SettingKind::Choice(_) => String::new(),
            _ => match editing {
                Some((key, draft)) if key == setting.key => format!("{draft}{}", '\u{2588}'),
                _ => settings.text(setting.key),
            },
        }));
        rows.push(Row::Blank);
    }
    rows
}

/// The word before the colon on a setting's title line: "Editor: Font Size".
fn short_section(section: &str) -> &str {
    match section {
        "Text Editor" => "Editor",
        "Dependency View" => "Graph",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ls_core::settings::SETTINGS;

    const PANE: Rect = Rect::new(60.0, 100.0, 1200.0, 700.0);
    const LINE: f32 = 20.0;
    const DIGIT: f32 = 8.0;

    fn all() -> Vec<&'static SettingDescriptor> {
        SETTINGS.iter().collect()
    }

    fn of_kind(wanted: fn(SettingKind) -> bool) -> &'static SettingDescriptor {
        SETTINGS.iter().find(|setting| wanted(setting.kind)).expect("one exists")
    }

    #[test]
    fn the_screen_splits_into_a_search_box_a_category_list_and_a_settings_list() {
        let (search, categories, list) = frame(PANE, LINE, DIGIT);
        assert!(search.y < categories.y, "the search box is above both columns");
        assert!(categories.right() <= list.x, "the columns do not overlap");
        assert!(list.right() <= PANE.right(), "the list stays inside the pane");
        assert!(search.right() <= PANE.right());
    }

    #[test]
    fn every_visible_setting_gets_a_placement_in_order() {
        let visible = all();
        let screen = layout(PANE, LINE, DIGIT, &visible, &Settings::new(), 0);
        assert_eq!(screen.placements.len(), visible.len());
        for (placement, setting) in screen.placements.iter().zip(visible.iter()) {
            assert_eq!(placement.key, setting.key);
        }
        let ordered = screen.placements.windows(2).all(|pair| pair[0].title_row < pair[1].title_row);
        assert!(ordered, "rows run down the page");
    }

    #[test]
    fn the_rows_and_the_placements_agree_on_where_a_setting_starts() {
        // Drawing and hit-testing read these separately, so if they ever
        // disagree a click lands on a different setting than the one under
        // the pointer.
        let visible = all();
        let screen = layout(PANE, LINE, DIGIT, &visible, &Settings::new(), 0);
        let rows = rows(&visible, &Settings::new(), None);
        for (placement, setting) in screen.placements.iter().zip(visible.iter()) {
            match &rows[placement.title_row] {
                Row::Title(text) => assert!(
                    text.contains(setting.title),
                    "row {} is {text:?}, expected the title of {}",
                    placement.title_row,
                    setting.key
                ),
                other => panic!("row {} is {other:?}, not a title", placement.title_row),
            }
        }
    }

    #[test]
    fn a_section_heading_is_drawn_once_before_its_first_setting() {
        let visible = all();
        let rows = rows(&visible, &Settings::new(), None);
        let headings: Vec<&String> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Heading(text) => Some(text),
                _ => None,
            })
            .collect();
        let mut expected: Vec<&str> = Vec::new();
        for setting in &visible {
            if expected.last() != Some(&setting.section) {
                expected.push(setting.section);
            }
        }
        assert_eq!(headings.len(), expected.len(), "one heading per run of a section");
    }

    #[test]
    fn a_click_on_a_checkbox_names_its_setting() {
        let visible = all();
        let settings = Settings::new();
        let screen = layout(PANE, LINE, DIGIT, &visible, &settings, 0);
        let boolean = of_kind(|kind| matches!(kind, SettingKind::Bool));
        let placement = screen
            .placements
            .iter()
            .find(|placement| placement.key == boolean.key)
            .expect("placed");
        let middle = (
            placement.control.x + placement.control.width / 2.0,
            placement.control.y + placement.control.height / 2.0,
        );
        assert_eq!(hit(&screen, &visible, middle.0, middle.1), Hit::Control(boolean.key));
    }

    #[test]
    fn a_click_on_one_option_of_a_choice_names_that_option() {
        // The smaller target has to win over the span that encloses it, or
        // every option would read as a click on the control itself.
        let visible = all();
        let choice = of_kind(|kind| matches!(kind, SettingKind::Choice(_)));
        let SettingKind::Choice(options) = choice.kind else { panic!("a choice") };

        // Scrolled so the setting is actually on screen: a control below the
        // fold is correctly unclickable, which is what this used to hit.
        let unscrolled = layout(PANE, LINE, DIGIT, &visible, &Settings::new(), 0);
        let row = unscrolled
            .placements
            .iter()
            .find(|p| p.key == choice.key)
            .expect("placed")
            .title_row;
        let screen = layout(PANE, LINE, DIGIT, &visible, &Settings::new(), row);
        let placement =
            screen.placements.iter().find(|p| p.key == choice.key).expect("placed");
        assert_eq!(placement.options.len(), options.len());

        for (option, rect) in options.iter().zip(placement.options.iter()) {
            let middle = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
            assert_eq!(
                hit(&screen, &visible, middle.0, middle.1),
                Hit::Option(choice.key, option),
                "clicking {option:?}"
            );
        }
    }

    #[test]
    fn the_reset_affordance_appears_only_once_a_value_has_moved() {
        let visible = all();
        let clean = layout(PANE, LINE, DIGIT, &visible, &Settings::new(), 0);
        assert!(clean.placements.iter().all(|p| p.reset.is_none()), "nothing to reset yet");

        let mut settings = Settings::new();
        settings.set("editor.fontSize", "22");
        let dirty = layout(PANE, LINE, DIGIT, &visible, &settings, 0);
        let placement =
            dirty.placements.iter().find(|p| p.key == "editor.fontSize").expect("placed");
        let reset = placement.reset.expect("a changed setting can be put back");
        let middle = (reset.x + reset.width / 2.0, reset.y + reset.height / 2.0);
        assert_eq!(hit(&dirty, &visible, middle.0, middle.1), Hit::Reset("editor.fontSize"));
    }

    #[test]
    fn a_click_on_the_search_box_or_a_category_is_reported_as_such() {
        let visible = all();
        let screen = layout(PANE, LINE, DIGIT, &visible, &Settings::new(), 0);
        assert_eq!(
            hit(&screen, &visible, screen.search.x + 20.0, screen.search.y + 5.0),
            Hit::Search
        );
        let second = screen.category_rows[1];
        assert_eq!(
            hit(&screen, &visible, second.x + 10.0, second.y + 5.0),
            Hit::Category(1)
        );
    }

    #[test]
    fn a_control_scrolled_below_the_fold_cannot_be_clicked() {
        // Its rectangle is still measured -- scrolling has to move it -- but
        // it is outside the list, and a click there must not reach it.
        let visible = all();
        let screen = layout(PANE, LINE, DIGIT, &visible, &Settings::new(), 0);
        let below = screen
            .placements
            .iter()
            .find(|p| p.control.y > screen.list.bottom())
            .expect("the list is longer than the pane");
        let middle = (below.control.x + 2.0, below.control.y + 2.0);
        assert_eq!(hit(&screen, &visible, middle.0, middle.1), Hit::Nothing);
    }

    #[test]
    fn a_click_against_a_stale_screen_hits_nothing_rather_than_crashing() {
        // The screen is measured on one frame and clicked on the next, and a
        // search typed in between leaves a shorter list. Indexing it took the
        // whole application down.
        let visible = all();
        let screen = layout(PANE, LINE, DIGIT, &visible, &Settings::new(), 0);
        let narrowed: Vec<&'static SettingDescriptor> = visible[..1].to_vec();
        for placement in &screen.placements {
            let middle = (
                placement.control.x + placement.control.width / 2.0,
                placement.control.y + placement.control.height / 2.0,
            );
            // Must not panic, whatever it returns.
            let _ = hit(&screen, &narrowed, middle.0, middle.1);
        }
        assert_eq!(hit(&screen, &[], 0.0, 0.0), Hit::Nothing);
    }

    #[test]
    fn a_click_on_empty_space_hits_nothing() {
        let visible = all();
        let screen = layout(PANE, LINE, DIGIT, &visible, &Settings::new(), 0);
        assert_eq!(hit(&screen, &visible, screen.list.right() - 2.0, screen.list.bottom() - 2.0), Hit::Nothing);
        assert_eq!(hit(&screen, &visible, 0.0, 0.0), Hit::Nothing, "outside the pane entirely");
    }

    #[test]
    fn scrolling_moves_the_controls_with_the_text() {
        // A control that stayed put while its row scrolled away would be
        // clickable over the wrong setting.
        let visible = all();
        let settings = Settings::new();
        let top = layout(PANE, LINE, DIGIT, &visible, &settings, 0);
        let down = layout(PANE, LINE, DIGIT, &visible, &settings, 5);
        for (a, b) in top.placements.iter().zip(down.placements.iter()) {
            assert_eq!(a.key, b.key);
            assert!(
                (a.control.y - b.control.y - 5.0 * LINE).abs() < 0.01,
                "{} moved {} px for 5 rows",
                a.key,
                a.control.y - b.control.y
            );
        }
    }

    #[test]
    fn an_empty_list_measures_without_panicking() {
        let screen = layout(PANE, LINE, DIGIT, &[], &Settings::new(), 0);
        assert!(screen.placements.is_empty());
        assert_eq!(screen.total_rows, 0);
        assert_eq!(hit(&screen, &[], screen.list.x + 5.0, screen.list.y + 5.0), Hit::Nothing);
        assert!(rows(&[], &Settings::new(), None).is_empty());
    }

    #[test]
    fn a_pane_too_small_to_hold_the_screen_still_measures() {
        // A window dragged down to nothing must not produce negative widths
        // that the renderer would then try to draw.
        let tiny = Rect::new(0.0, 0.0, 30.0, 20.0);
        let screen = layout(tiny, LINE, DIGIT, &all(), &Settings::new(), 0);
        assert!(screen.list.width > 0.0 && screen.list.height > 0.0);
        assert!(visible_rows(screen.list, LINE) >= 1);
    }

    #[test]
    fn a_changed_setting_says_so_on_its_title_line() {
        let mut settings = Settings::new();
        settings.set("editor.fontSize", "22");
        let visible = all();
        let rows = rows(&visible, &settings, None);
        let title = rows
            .iter()
            .find_map(|row| match row {
                Row::Title(text) if text.contains("Font Size") => Some(text),
                _ => None,
            })
            .expect("the font size row");
        assert!(title.contains("modified"), "{title}");
    }

    #[test]
    fn a_setting_that_needs_a_restart_says_so() {
        let visible = all();
        let rows = rows(&visible, &Settings::new(), None);
        let restarts: Vec<&String> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Title(text) if text.contains("restart") => Some(text),
                _ => None,
            })
            .collect();
        assert!(!restarts.is_empty(), "some settings only apply on restart, and must say so");
    }

    #[test]
    fn a_field_being_typed_into_shows_the_draft_and_not_the_stored_value() {
        // Otherwise typing into a field looks like nothing is happening: the
        // store only hears about it when the edit is committed.
        let visible = all();
        let rows = rows(&visible, &Settings::new(), Some(("editor.fontFamily", "JetBra")));
        assert!(
            rows.iter().any(|row| matches!(row, Row::Value(text) if text.starts_with("JetBra"))),
            "the draft is on screen"
        );
        assert!(
            !rows.iter().any(|row| matches!(row, Row::Value(text) if text == "Cascadia Mono")),
            "and the stored value is not"
        );
    }

    #[test]
    fn the_value_of_a_field_is_shown_as_text() {
        let mut settings = Settings::new();
        settings.set("editor.fontFamily", "JetBrains Mono");
        let visible = all();
        let rows = rows(&visible, &settings, None);
        assert!(
            rows.iter().any(|row| matches!(row, Row::Value(text) if text == "JetBrains Mono")),
            "the current value is on screen"
        );
    }

    #[test]
    fn the_list_can_always_be_scrolled_back_to_something() {
        let visible = all();
        let screen = layout(PANE, LINE, DIGIT, &visible, &Settings::new(), 0);
        let furthest = max_scroll(&screen, LINE);
        assert!(furthest < screen.total_rows, "the last screenful is never scrolled past");
    }
}
