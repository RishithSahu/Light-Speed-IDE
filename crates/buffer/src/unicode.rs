//! Grapheme and display-width queries (specification sections 15, 17).
//!
//! Storage offsets, user-perceived characters and screen columns are three
//! different things:
//!
//! ```text
//! "e" + U+0301        2 scalars, 1 grapheme, 1 column
//! "\u{1F1EF}\u{1F1F5}" 2 scalars, 1 grapheme, 2 columns
//! "\t"                1 scalar,  1 grapheme, up to tab_width columns
//! ```
//!
//! Cursor movement steps by grapheme so that pressing the arrow key once never
//! lands inside a combining sequence, while the buffer keeps addressing text by
//! scalar offset.

use crate::offsets::{CharOffset, DisplayColumn};
use crate::TextBuffer;
use unicode_segmentation::GraphemeCursor;
use unicode_width::UnicodeWidthChar;

/// Characters of surrounding text handed to the segmentation algorithm.
///
/// Grapheme clusters are bounded in practice (the longest realistic ones are
/// emoji ZWJ sequences of a dozen or so scalars), so a window this size gives
/// the segmenter all the context it needs without reading the whole document
/// for one cursor step.
const CONTEXT_CHARS: usize = 64;

struct Window {
    text: String,
    byte_in_window: usize,
}

fn window_around(buffer: &TextBuffer, position: CharOffset) -> Window {
    let start = position.saturating_sub(CONTEXT_CHARS);
    let end = buffer.clamp(position + CONTEXT_CHARS);
    let text = buffer.slice(start..end);
    let byte_in_window = byte_of_char(&text, position - start);
    Window { text, byte_in_window }
}

fn byte_of_char(text: &str, char_index: usize) -> usize {
    text.char_indices().nth(char_index).map(|(byte, _)| byte).unwrap_or(text.len())
}

/// Next grapheme-cluster boundary at or after `position`.
pub fn next_grapheme_boundary(buffer: &TextBuffer, position: CharOffset) -> CharOffset {
    let position = buffer.clamp(position);
    if position == buffer.end() {
        return position;
    }
    let window = window_around(buffer, position);
    let mut cursor = GraphemeCursor::new(window.byte_in_window, window.text.len(), true);
    match cursor.next_boundary(&window.text, 0) {
        Ok(Some(byte)) => {
            let advanced = window.text[window.byte_in_window..byte].chars().count();
            position + advanced
        }
        // The window ended without a boundary, or the segmenter wanted context
        // from outside it: stepping one scalar is always a safe approximation.
        _ => buffer.clamp(position + 1),
    }
}

/// Previous grapheme-cluster boundary before `position`.
pub fn prev_grapheme_boundary(buffer: &TextBuffer, position: CharOffset) -> CharOffset {
    let position = buffer.clamp(position);
    if position == CharOffset::ZERO {
        return position;
    }
    let window = window_around(buffer, position);
    let mut cursor = GraphemeCursor::new(window.byte_in_window, window.text.len(), true);
    match cursor.prev_boundary(&window.text, 0) {
        Ok(Some(byte)) => {
            let retreated = window.text[byte..window.byte_in_window].chars().count();
            position.saturating_sub(retreated)
        }
        _ => position.saturating_sub(1),
    }
}

/// Whether `position` sits on a grapheme-cluster boundary.
pub fn is_grapheme_boundary(buffer: &TextBuffer, position: CharOffset) -> bool {
    let position = buffer.clamp(position);
    if position == CharOffset::ZERO || position == buffer.end() {
        return true;
    }
    let window = window_around(buffer, position);
    let mut cursor = GraphemeCursor::new(window.byte_in_window, window.text.len(), true);
    cursor.is_boundary(&window.text, 0).unwrap_or(true)
}

/// Width of one character in display cells.
fn char_width(ch: char, column: usize, tab_width: usize) -> usize {
    match ch {
        '\t' => tab_width - (column % tab_width.max(1)),
        _ => UnicodeWidthChar::width(ch).unwrap_or(0),
    }
}

/// Display column of `char_offset` characters into `line`.
pub fn display_column(line: &str, char_offset: usize, tab_width: usize) -> DisplayColumn {
    let mut column = 0;
    for ch in line.chars().take(char_offset) {
        column += char_width(ch, column, tab_width);
    }
    DisplayColumn::new(column)
}

/// Total display width of `line`.
pub fn display_width(line: &str, tab_width: usize) -> usize {
    display_column(line, usize::MAX, tab_width).get()
}

/// Longest line prefix scanned exactly when converting between character
/// offsets and display columns.
///
/// Tab stops and wide characters mean an exact column depends on every
/// character before it, so a document that is one 10 MB line would make each
/// keystroke scan megabytes. Beyond this bound the columns before the scanned
/// window are counted as one column per character, which is exact for ordinary
/// text and approximate only for absurdly long lines - and, either way, bounded.
pub const MAX_EXACT_COLUMN_SCAN: usize = 64 * 1024;

/// Display column of `position` within the line starting at `line_start`.
///
/// Bounded by [`MAX_EXACT_COLUMN_SCAN`], so this is safe to call on every
/// keystroke regardless of how long the line is.
pub fn display_column_in(
    buffer: &TextBuffer,
    line_start: CharOffset,
    position: CharOffset,
    tab_width: usize,
) -> DisplayColumn {
    let position = buffer.clamp(position);
    if position <= line_start {
        return DisplayColumn::ZERO;
    }
    let prefix_chars = position - line_start;
    if prefix_chars <= MAX_EXACT_COLUMN_SCAN {
        let text = buffer.slice(line_start..position);
        return display_column(&text, prefix_chars, tab_width);
    }
    let scan_start = position.saturating_sub(MAX_EXACT_COLUMN_SCAN);
    let text = buffer.slice(scan_start..position);
    let unscanned = prefix_chars - MAX_EXACT_COLUMN_SCAN;
    DisplayColumn::new(unscanned + display_column(&text, MAX_EXACT_COLUMN_SCAN, tab_width).get())
}

/// Character offset on `line` nearest to `column`, bounded the same way as
/// [`display_column_in`].
pub fn offset_for_display_column_in(
    buffer: &TextBuffer,
    line: crate::offsets::LineIndex,
    column: DisplayColumn,
    tab_width: usize,
) -> CharOffset {
    let range = buffer.line_range(line);
    let length = range.end - range.start;
    let scanned = length.min(MAX_EXACT_COLUMN_SCAN);
    let text = buffer.slice(range.start..(range.start + scanned));
    let offset = char_offset_for_display_column(&text, column, tab_width);
    if offset < scanned || length == scanned {
        range.start + offset
    } else {
        // The target column is past the scanned window: fall back to counting
        // the remaining characters as one column each.
        let remaining = column.get().saturating_sub(display_width(&text, tab_width));
        range.start + scanned + remaining.min(length - scanned)
    }
}

/// Character offset into `line` that is nearest to `column`.
///
/// A column that lands inside a tab or a wide character resolves to whichever
/// edge of that character is closer, which is what a mouse click should do.
pub fn char_offset_for_display_column(
    line: &str,
    column: DisplayColumn,
    tab_width: usize,
) -> usize {
    let target = column.get();
    let mut current = 0;
    for (index, ch) in line.chars().enumerate() {
        let width = char_width(ch, current, tab_width);
        if current + width > target {
            // The target is inside this character: snap to the nearer edge.
            return if target - current >= width.div_ceil(2) { index + 1 } else { index };
        }
        current += width;
    }
    line.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMBINING: &str = "e\u{0301}"; // e + combining acute
    const FLAG: &str = "\u{1F1EF}\u{1F1F5}"; // regional indicators JP
    const FAMILY: &str = "\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}"; // ZWJ sequence

    #[test]
    fn moves_over_a_combining_sequence_as_one_unit() {
        let buffer = TextBuffer::from_str(COMBINING);
        assert_eq!(buffer.len_chars(), 2);
        assert_eq!(next_grapheme_boundary(&buffer, CharOffset::ZERO), CharOffset::new(2));
        assert_eq!(prev_grapheme_boundary(&buffer, CharOffset::new(2)), CharOffset::ZERO);
        assert!(!is_grapheme_boundary(&buffer, CharOffset::new(1)));
    }

    #[test]
    fn moves_over_a_flag_as_one_unit() {
        let buffer = TextBuffer::from_str(FLAG);
        assert_eq!(buffer.len_chars(), 2);
        assert_eq!(next_grapheme_boundary(&buffer, CharOffset::ZERO), CharOffset::new(2));
    }

    #[test]
    fn moves_over_a_zwj_sequence_as_one_unit() {
        let buffer = TextBuffer::from_str(FAMILY);
        assert_eq!(buffer.len_chars(), 5);
        assert_eq!(next_grapheme_boundary(&buffer, CharOffset::ZERO), CharOffset::new(5));
        assert_eq!(prev_grapheme_boundary(&buffer, CharOffset::new(5)), CharOffset::ZERO);
    }

    #[test]
    fn ascii_moves_one_character_at_a_time() {
        let buffer = TextBuffer::from_str("abc");
        assert_eq!(next_grapheme_boundary(&buffer, CharOffset::new(1)), CharOffset::new(2));
        assert_eq!(prev_grapheme_boundary(&buffer, CharOffset::new(1)), CharOffset::ZERO);
        assert!(is_grapheme_boundary(&buffer, CharOffset::new(1)));
    }

    #[test]
    fn boundaries_clamp_at_the_document_edges() {
        let buffer = TextBuffer::from_str("ab");
        assert_eq!(next_grapheme_boundary(&buffer, CharOffset::new(2)), CharOffset::new(2));
        assert_eq!(prev_grapheme_boundary(&buffer, CharOffset::ZERO), CharOffset::ZERO);
        assert_eq!(next_grapheme_boundary(&buffer, CharOffset::new(99)), CharOffset::new(2));
    }

    #[test]
    fn crlf_is_one_grapheme() {
        // The buffer normalizes line endings, but text pasted in can still hold
        // a pair, and it must not be split by cursor movement.
        let buffer = TextBuffer::from_str("a\r\nb");
        assert_eq!(next_grapheme_boundary(&buffer, CharOffset::new(1)), CharOffset::new(3));
    }

    #[test]
    fn works_across_chunk_boundaries() {
        // Push the interesting characters well past a single leaf.
        let text = format!("{}{}", "x".repeat(5000), FAMILY);
        let buffer = TextBuffer::from_str(&text);
        let start = CharOffset::new(5000);
        assert_eq!(next_grapheme_boundary(&buffer, start), CharOffset::new(5005));
        assert_eq!(prev_grapheme_boundary(&buffer, buffer.end()), start);
    }

    #[test]
    fn display_columns_expand_tabs() {
        assert_eq!(display_column("\tx", 1, 4), DisplayColumn::new(4));
        assert_eq!(display_column("ab\tx", 3, 4), DisplayColumn::new(4));
        assert_eq!(display_column("abcd\tx", 5, 4), DisplayColumn::new(8));
        assert_eq!(display_width("a\tb", 4), 5);
    }

    #[test]
    fn wide_characters_take_two_columns() {
        assert_eq!(display_column("\u{4F60}\u{597D}", 2, 4), DisplayColumn::new(4));
        assert_eq!(display_column("ab", 2, 4), DisplayColumn::new(2));
    }

    #[test]
    fn combining_marks_take_no_extra_column() {
        assert_eq!(display_column(COMBINING, 2, 4), DisplayColumn::new(1));
    }

    #[test]
    fn column_to_offset_snaps_to_the_nearer_edge() {
        assert_eq!(char_offset_for_display_column("abc", DisplayColumn::new(2), 4), 2);
        // Column 1 is inside a 4-wide tab: nearer the left edge.
        assert_eq!(char_offset_for_display_column("\tabc", DisplayColumn::new(1), 4), 0);
        // Column 3 is inside the same tab but nearer the right edge.
        assert_eq!(char_offset_for_display_column("\tabc", DisplayColumn::new(3), 4), 1);
        // Past the end clamps to the line length.
        assert_eq!(char_offset_for_display_column("abc", DisplayColumn::new(99), 4), 3);
    }
}
