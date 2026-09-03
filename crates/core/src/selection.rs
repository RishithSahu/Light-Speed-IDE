//! Cursor and selection (specification sections 21, 61).
//!
//! A [`Selection`] is an anchor and a head. When they coincide it is a caret;
//! otherwise the covered range is the selection, and the head is the end the
//! user is moving. Movement is grapheme-aware, and vertical movement remembers
//! the column the user started from so a trip through a short line does not
//! lose the original column.
//!
//! Stage 1 has exactly one selection. It is nevertheless held in a
//! [`SelectionSet`] so that adding multiple cursors later is a change to this
//! type rather than to every editing operation (specification section 21).

use ls_buffer::unicode;
use ls_buffer::{CharOffset, DisplayColumn, LineIndex, TextBuffer};
use std::ops::Range;

/// One cursor, possibly with a selected range.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// The fixed end, set when the selection started.
    pub anchor: CharOffset,
    /// The moving end; where the caret is drawn.
    pub head: CharOffset,
    /// Column to aim for during vertical movement.
    pub goal_column: Option<DisplayColumn>,
}

impl Default for Selection {
    fn default() -> Self {
        Selection::caret(CharOffset::ZERO)
    }
}

impl Selection {
    pub fn caret(at: CharOffset) -> Self {
        Selection { anchor: at, head: at, goal_column: None }
    }

    pub fn new(anchor: CharOffset, head: CharOffset) -> Self {
        Selection { anchor, head, goal_column: None }
    }

    /// Ordered range covered by this selection.
    pub fn range(&self) -> Range<CharOffset> {
        if self.anchor <= self.head {
            self.anchor..self.head
        } else {
            self.head..self.anchor
        }
    }

    pub fn start(&self) -> CharOffset {
        self.anchor.min_value(self.head)
    }

    pub fn end(&self) -> CharOffset {
        self.anchor.max_value(self.head)
    }

    pub fn is_caret(&self) -> bool {
        self.anchor == self.head
    }

    pub fn len_chars(&self) -> usize {
        self.end() - self.start()
    }

    /// Drops the selected range, keeping the head.
    pub fn collapse(&self) -> Selection {
        Selection { anchor: self.head, head: self.head, goal_column: self.goal_column }
    }

    /// Keeps both ends inside a document of `length` characters.
    pub fn clamped(&self, buffer: &TextBuffer) -> Selection {
        Selection {
            anchor: buffer.clamp(self.anchor),
            head: buffer.clamp(self.head),
            goal_column: self.goal_column,
        }
    }
}

/// All cursors in a document. Stage 1 keeps exactly one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionSet {
    primary: Selection,
}

impl SelectionSet {
    pub fn new(primary: Selection) -> Self {
        SelectionSet { primary }
    }

    pub fn primary(&self) -> Selection {
        self.primary
    }

    pub fn set_primary(&mut self, selection: Selection) {
        self.primary = selection;
    }

    /// Number of cursors. Always 1 in Stage 1.
    pub fn len(&self) -> usize {
        1
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn iter(&self) -> impl Iterator<Item = Selection> + '_ {
        std::iter::once(self.primary)
    }
}

/// A cursor movement request.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Movement {
    /// One grapheme cluster left/right.
    CharLeft,
    CharRight,
    WordLeft,
    WordRight,
    LineUp,
    LineDown,
    PageUp,
    PageDown,
    /// Column zero.
    LineStart,
    /// First non-blank character, or column zero if already there.
    LineStartSmart,
    LineEnd,
    DocumentStart,
    DocumentEnd,
    /// Directly to an offset, as produced by a mouse click.
    To(CharOffset),
}

impl Movement {
    /// Vertical movements are the ones that preserve the goal column.
    pub fn is_vertical(self) -> bool {
        matches!(
            self,
            Movement::LineUp | Movement::LineDown | Movement::PageUp | Movement::PageDown
        )
    }
}

/// Everything movement needs to know that is not in the buffer.
#[derive(Copy, Clone, Debug)]
pub struct MovementContext {
    pub tab_width: usize,
    /// Lines moved by PageUp/PageDown; the renderer supplies the viewport height.
    pub page_lines: usize,
}

impl Default for MovementContext {
    fn default() -> Self {
        MovementContext { tab_width: 4, page_lines: 20 }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum CharClass {
    Whitespace,
    Word,
    Punctuation,
}

fn class_of(ch: char) -> CharClass {
    if ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else if ch.is_whitespace() {
        CharClass::Whitespace
    } else {
        CharClass::Punctuation
    }
}

/// Applies `movement` to `selection`.
///
/// With `extend`, the anchor stays put and the selection grows; without it, the
/// selection collapses onto the new head.
pub fn move_selection(
    buffer: &TextBuffer,
    selection: Selection,
    movement: Movement,
    extend: bool,
    context: MovementContext,
) -> Selection {
    let selection = selection.clamped(buffer);
    let mut goal_column = None;

    let head = match movement {
        // Collapsing a selection with an arrow key moves to its edge rather
        // than one character from the head; this is what every editor does.
        Movement::CharLeft if !extend && !selection.is_caret() => selection.start(),
        Movement::CharRight if !extend && !selection.is_caret() => selection.end(),
        Movement::CharLeft => unicode::prev_grapheme_boundary(buffer, selection.head),
        Movement::CharRight => unicode::next_grapheme_boundary(buffer, selection.head),
        Movement::WordLeft => previous_word_boundary(buffer, selection.head),
        Movement::WordRight => next_word_boundary(buffer, selection.head),
        Movement::LineUp | Movement::LineDown | Movement::PageUp | Movement::PageDown => {
            let delta: isize = match movement {
                Movement::LineUp => -1,
                Movement::LineDown => 1,
                Movement::PageUp => -(context.page_lines as isize),
                _ => context.page_lines as isize,
            };
            let (head, goal) = move_vertically(buffer, selection, delta, context);
            goal_column = Some(goal);
            head
        }
        Movement::LineStart => line_start(buffer, selection.head),
        Movement::LineStartSmart => smart_line_start(buffer, selection.head, context),
        Movement::LineEnd => line_end(buffer, selection.head),
        Movement::DocumentStart => CharOffset::ZERO,
        Movement::DocumentEnd => buffer.end(),
        Movement::To(position) => buffer.clamp(position),
    };

    Selection { anchor: if extend { selection.anchor } else { head }, head, goal_column }
}

fn line_start(buffer: &TextBuffer, position: CharOffset) -> CharOffset {
    let line = buffer.char_to_line(position);
    buffer.line_range(line).start
}

fn line_end(buffer: &TextBuffer, position: CharOffset) -> CharOffset {
    let line = buffer.char_to_line(position);
    buffer.line_range(line).end
}

/// Home: to the first non-blank character, or to column zero when already there.
fn smart_line_start(
    buffer: &TextBuffer,
    position: CharOffset,
    _context: MovementContext,
) -> CharOffset {
    let line = buffer.char_to_line(position);
    let range = buffer.line_range(line);
    // Indentation is at the start of the line, so only that much is scanned.
    let scan_end = range.start + (range.end - range.start).min(unicode::MAX_EXACT_COLUMN_SCAN);
    let text = buffer.slice(range.start..scan_end);
    let indent = text.chars().take_while(|c| c.is_whitespace()).count();
    let first_non_blank = range.start + indent;
    if position == first_non_blank {
        range.start
    } else {
        first_non_blank
    }
}

fn move_vertically(
    buffer: &TextBuffer,
    selection: Selection,
    delta_lines: isize,
    context: MovementContext,
) -> (CharOffset, DisplayColumn) {
    let line = buffer.char_to_line(selection.head);
    let goal = selection.goal_column.unwrap_or_else(|| {
        let range = buffer.line_range(line);
        unicode::display_column_in(buffer, range.start, selection.head, context.tab_width)
    });

    let last_line = buffer.len_lines().saturating_sub(1);
    let target = (line.get() as isize + delta_lines).clamp(0, last_line as isize) as usize;
    let target_line = LineIndex::new(target);

    // Moving off either end of the document parks the caret at that end, which
    // matches what users expect from Up on the first line.
    if target == line.get() {
        if delta_lines < 0 {
            return (CharOffset::ZERO, goal);
        }
        if delta_lines > 0 {
            return (buffer.end(), goal);
        }
    }

    let head = unicode::offset_for_display_column_in(buffer, target_line, goal, context.tab_width);
    (head, goal)
}

/// Start of the next word at or after `position`.
fn next_word_boundary(buffer: &TextBuffer, position: CharOffset) -> CharOffset {
    let end = buffer.end();
    let mut cursor = position;
    if cursor >= end {
        return end;
    }
    // A line break is its own stop, so word movement never silently jumps lines.
    if buffer.char_at(cursor) == Some('\n') {
        return cursor + 1;
    }
    while cursor < end {
        match buffer.char_at(cursor) {
            Some(ch) if ch != '\n' && class_of(ch) == CharClass::Whitespace => cursor += 1,
            _ => break,
        }
    }
    let Some(first) = buffer.char_at(cursor) else { return cursor };
    if first == '\n' {
        return cursor;
    }
    let class = class_of(first);
    while cursor < end {
        match buffer.char_at(cursor) {
            Some(ch) if ch != '\n' && class_of(ch) == class => cursor += 1,
            _ => break,
        }
    }
    cursor
}

/// Start of the word before `position`.
fn previous_word_boundary(buffer: &TextBuffer, position: CharOffset) -> CharOffset {
    let mut cursor = position;
    if cursor == CharOffset::ZERO {
        return cursor;
    }
    if buffer.char_at(cursor - 1) == Some('\n') {
        return cursor - 1;
    }
    while cursor > CharOffset::ZERO {
        match buffer.char_at(cursor - 1) {
            Some(ch) if ch != '\n' && class_of(ch) == CharClass::Whitespace => cursor -= 1,
            _ => break,
        }
    }
    let Some(first) = (if cursor > CharOffset::ZERO { buffer.char_at(cursor - 1) } else { None })
    else {
        return cursor;
    };
    if first == '\n' {
        return cursor;
    }
    let class = class_of(first);
    while cursor > CharOffset::ZERO {
        match buffer.char_at(cursor - 1) {
            Some(ch) if ch != '\n' && class_of(ch) == class => cursor -= 1,
            _ => break,
        }
    }
    cursor
}

/// Range of the word surrounding `position`, for double-click selection.
pub fn word_at(buffer: &TextBuffer, position: CharOffset) -> Range<CharOffset> {
    let position = buffer.clamp(position);
    let class = buffer
        .char_at(position)
        .or_else(|| if position > CharOffset::ZERO { buffer.char_at(position - 1) } else { None })
        .map(class_of);
    let Some(class) = class else { return position..position };

    let mut start = position;
    while start > CharOffset::ZERO {
        match buffer.char_at(start - 1) {
            Some(ch) if ch != '\n' && class_of(ch) == class => start -= 1,
            _ => break,
        }
    }
    let mut end = position;
    while end < buffer.end() {
        match buffer.char_at(end) {
            Some(ch) if ch != '\n' && class_of(ch) == class => end += 1,
            _ => break,
        }
    }
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> TextBuffer {
        TextBuffer::from_str(text)
    }

    fn caret(at: usize) -> Selection {
        Selection::caret(CharOffset::new(at))
    }

    fn apply(buffer: &TextBuffer, selection: Selection, movement: Movement) -> Selection {
        move_selection(buffer, selection, movement, false, MovementContext::default())
    }

    fn extend(buffer: &TextBuffer, selection: Selection, movement: Movement) -> Selection {
        move_selection(buffer, selection, movement, true, MovementContext::default())
    }

    #[test]
    fn caret_basics() {
        let selection = caret(5);
        assert!(selection.is_caret());
        assert_eq!(selection.len_chars(), 0);
        assert_eq!(selection.range(), CharOffset::new(5)..CharOffset::new(5));
    }

    #[test]
    fn a_backwards_selection_still_has_an_ordered_range() {
        let selection = Selection::new(CharOffset::new(9), CharOffset::new(3));
        assert_eq!(selection.range(), CharOffset::new(3)..CharOffset::new(9));
        assert_eq!(selection.start(), CharOffset::new(3));
        assert_eq!(selection.end(), CharOffset::new(9));
        assert_eq!(selection.len_chars(), 6);
    }

    #[test]
    fn character_movement_is_grapheme_aware() {
        let buffer = buffer("a\u{1F469}\u{200D}\u{1F467}b");
        let moved = apply(&buffer, caret(1), Movement::CharRight);
        assert_eq!(moved.head, CharOffset::new(4), "should step over the whole ZWJ sequence");
        let back = apply(&buffer, moved, Movement::CharLeft);
        assert_eq!(back.head, CharOffset::new(1));
    }

    #[test]
    fn movement_stops_at_the_document_edges() {
        let buffer = buffer("ab");
        assert_eq!(apply(&buffer, caret(0), Movement::CharLeft).head, CharOffset::ZERO);
        assert_eq!(apply(&buffer, caret(2), Movement::CharRight).head, CharOffset::new(2));
    }

    #[test]
    fn arrow_keys_collapse_an_existing_selection_to_its_edge() {
        let buffer = buffer("hello world");
        let selection = Selection::new(CharOffset::new(2), CharOffset::new(7));
        assert_eq!(apply(&buffer, selection, Movement::CharLeft).head, CharOffset::new(2));
        assert_eq!(apply(&buffer, selection, Movement::CharRight).head, CharOffset::new(7));
        // Extending is different: it really does move one character.
        assert_eq!(extend(&buffer, selection, Movement::CharRight).head, CharOffset::new(8));
    }

    #[test]
    fn extending_keeps_the_anchor() {
        let buffer = buffer("hello");
        let extended = extend(&buffer, caret(1), Movement::CharRight);
        assert_eq!(extended.anchor, CharOffset::new(1));
        assert_eq!(extended.head, CharOffset::new(2));
        assert!(!extended.is_caret());
    }

    #[test]
    fn word_movement_walks_words_then_punctuation() {
        let buffer = buffer("let value = compute(x);");
        let mut selection = caret(0);
        let expected = [3, 9, 11, 19, 20, 21, 23];
        for want in expected {
            selection = apply(&buffer, selection, Movement::WordRight);
            assert_eq!(selection.head, CharOffset::new(want), "walking right");
        }
        for want in [21, 20, 19, 12, 10, 4, 0] {
            selection = apply(&buffer, selection, Movement::WordLeft);
            assert_eq!(selection.head, CharOffset::new(want), "walking left");
        }
    }

    #[test]
    fn word_movement_stops_at_line_breaks() {
        let buffer = buffer("one\ntwo");
        let selection = apply(&buffer, caret(3), Movement::WordRight);
        assert_eq!(selection.head, CharOffset::new(4), "should step over the break only");
        let back = apply(&buffer, caret(4), Movement::WordLeft);
        assert_eq!(back.head, CharOffset::new(3));
    }

    #[test]
    fn line_start_and_end() {
        let buffer = buffer("    indented line\nnext");
        assert_eq!(apply(&buffer, caret(10), Movement::LineStart).head, CharOffset::ZERO);
        assert_eq!(apply(&buffer, caret(10), Movement::LineEnd).head, CharOffset::new(17));
        assert_eq!(apply(&buffer, caret(20), Movement::LineStart).head, CharOffset::new(18));
    }

    #[test]
    fn smart_home_toggles_between_indent_and_column_zero() {
        let buffer = buffer("    indented");
        let first = apply(&buffer, caret(8), Movement::LineStartSmart);
        assert_eq!(first.head, CharOffset::new(4), "first press goes to the text");
        let second = apply(&buffer, first, Movement::LineStartSmart);
        assert_eq!(second.head, CharOffset::ZERO, "second press goes to column zero");
    }

    #[test]
    fn document_start_and_end() {
        let buffer = buffer("a\nb\nc");
        assert_eq!(apply(&buffer, caret(3), Movement::DocumentStart).head, CharOffset::ZERO);
        assert_eq!(apply(&buffer, caret(0), Movement::DocumentEnd).head, buffer.end());
    }

    #[test]
    fn vertical_movement_keeps_the_goal_column() {
        //          0123456789
        let buffer = buffer("long line here\nshort\nlong line again");
        let start = caret(12); // column 12 on line 0
        let down = apply(&buffer, start, Movement::LineDown);
        assert_eq!(down.head, CharOffset::new(20), "clamped to the end of the short line");
        let down_again = apply(&buffer, down, Movement::LineDown);
        assert_eq!(
            down_again.head,
            CharOffset::new(21 + 12),
            "the original column is restored on the long line"
        );
    }

    #[test]
    fn vertical_movement_accounts_for_tabs() {
        let buffer = buffer("\tafter tab\nplain text line");
        let context = MovementContext { tab_width: 4, page_lines: 20 };
        // Just after the tab is display column 4.
        let start = Selection::caret(CharOffset::new(1));
        let down = move_selection(&buffer, start, Movement::LineDown, false, context);
        assert_eq!(down.head, CharOffset::new(11 + 4));
    }

    #[test]
    fn up_on_the_first_line_goes_to_the_start() {
        let buffer = buffer("first\nsecond");
        assert_eq!(apply(&buffer, caret(3), Movement::LineUp).head, CharOffset::ZERO);
    }

    #[test]
    fn down_on_the_last_line_goes_to_the_end() {
        let buffer = buffer("first\nsecond");
        assert_eq!(apply(&buffer, caret(8), Movement::LineDown).head, buffer.end());
    }

    #[test]
    fn page_movement_uses_the_viewport_height() {
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let buffer = buffer(&text);
        let context = MovementContext { tab_width: 4, page_lines: 25 };
        let start = Selection::caret(buffer.line_to_char(LineIndex::new(50)));
        let up = move_selection(&buffer, start, Movement::PageUp, false, context);
        assert_eq!(buffer.char_to_line(up.head), LineIndex::new(25));
        let down = move_selection(&buffer, up, Movement::PageDown, false, context);
        assert_eq!(buffer.char_to_line(down.head), LineIndex::new(50));
    }

    #[test]
    fn mouse_movement_goes_straight_to_an_offset() {
        let buffer = buffer("hello world");
        let moved = apply(&buffer, caret(0), Movement::To(CharOffset::new(7)));
        assert_eq!(moved.head, CharOffset::new(7));
        assert!(moved.is_caret());
        let clamped = apply(&buffer, caret(0), Movement::To(CharOffset::new(500)));
        assert_eq!(clamped.head, buffer.end());
    }

    #[test]
    fn word_at_selects_the_surrounding_word() {
        let buffer = buffer("let value = 42;");
        assert_eq!(word_at(&buffer, CharOffset::new(6)), CharOffset::new(4)..CharOffset::new(9));
        // On punctuation, the punctuation run is selected.
        assert_eq!(word_at(&buffer, CharOffset::new(14)), CharOffset::new(14)..CharOffset::new(15));
    }

    #[test]
    fn selection_set_holds_one_cursor_in_stage_one() {
        let mut set = SelectionSet::default();
        assert_eq!(set.len(), 1);
        set.set_primary(caret(4));
        assert_eq!(set.primary().head, CharOffset::new(4));
        assert_eq!(set.iter().count(), 1);
    }
}
