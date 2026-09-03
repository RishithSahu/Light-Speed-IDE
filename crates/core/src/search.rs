//! In-document incremental search (find).
//!
//! Find state belongs to the document it searches, the same as cursors,
//! selection and undo history -- it is per-document, not a global the shell
//! keeps track of. Recomputing matches is a synchronous scan over the buffer,
//! at the same tier as cursor movement, not scheduled work: it runs once per
//! query change, on an explicit user action, never per frame and never for
//! background documents.
//!
//! Matches feed the render snapshot's `decorations` field as
//! `DecorationKind::SearchMatch` -- a slot the snapshot contract already
//! reserved for exactly this. The *current* match additionally becomes the
//! document's selection, so it is drawn with the ordinary selection highlight
//! and the caret lands on it: no second highlight mechanism was needed for
//! "here is the one you're on" versus "here is everywhere it appears".

use ls_buffer::{LineIndex, TextBuffer};

/// One match: a half-open character range on one line.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FindMatch {
    pub line: LineIndex,
    pub start_column_chars: usize,
    pub end_column_chars: usize,
}

/// A document's find state. The empty query is the "not searching" state, so
/// there is nothing to construct or tear down when find opens and closes.
#[derive(Clone, Debug, Default)]
pub struct FindState {
    query: String,
    matches: Vec<FindMatch>,
    current: Option<usize>,
}

impl FindState {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn matches(&self) -> &[FindMatch] {
        &self.matches
    }

    /// 1-based position and total, for the find bar: "3 of 12".
    pub fn position(&self) -> Option<(usize, usize)> {
        self.current.map(|index| (index + 1, self.matches.len()))
    }

    pub fn current_match(&self) -> Option<FindMatch> {
        self.current.and_then(|index| self.matches.get(index)).copied()
    }

    /// Replaces the query and recomputes matches against `buffer`.
    ///
    /// The match nearest `from` (at or after it, wrapping to the first match
    /// otherwise) becomes current, so typing a query jumps to the nearest hit
    /// rather than always the first one in the document.
    pub fn set_query(&mut self, query: String, buffer: &TextBuffer, from: LineIndex) {
        self.matches = find_matches(buffer, &query);
        self.query = query;
        self.current = self
            .matches
            .iter()
            .position(|found| found.line >= from)
            .or(if self.matches.is_empty() { None } else { Some(0) });
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.matches.clear();
        self.current = None;
    }

    /// Moves to the next (`delta = 1`) or previous (`delta = -1`) match,
    /// wrapping around either end.
    pub fn advance(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let count = self.matches.len() as isize;
        let current = self.current.map(|index| index as isize).unwrap_or(-1);
        let next = ((current + delta) % count + count) % count;
        self.current = Some(next as usize);
    }
}

/// Scans `buffer` line by line for case-insensitive occurrences of `query`.
///
/// Line-by-line rather than one global scan so a match's position is already
/// in the coordinates the snapshot draws with (line + character column),
/// instead of a byte offset that would need translating back afterwards.
/// Occurrences within a line do not overlap.
pub fn find_matches(buffer: &TextBuffer, query: &str) -> Vec<FindMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let needle = query.to_lowercase();
    let needle_chars = needle.chars().count();
    let mut matches = Vec::new();

    for line_number in 0..buffer.len_lines() {
        let line = LineIndex::new(line_number);
        let text = buffer.line_text(line);
        let haystack = text.to_lowercase();

        let mut cursor = 0usize;
        while let Some(found) = haystack[cursor..].find(&needle) {
            let byte_start = cursor + found;
            let char_start = haystack[..byte_start].chars().count();
            matches.push(FindMatch {
                line,
                start_column_chars: char_start,
                end_column_chars: char_start + needle_chars,
            });
            cursor = byte_start + found_len(&haystack[byte_start..], &needle);
            if cursor >= haystack.len() {
                break;
            }
        }
    }
    matches
}

/// Byte length actually consumed by one match, so an empty needle (already
/// excluded by the caller) could never spin the scan forever.
fn found_len(remaining: &str, needle: &str) -> usize {
    remaining[..needle.len().min(remaining.len())].len().max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> TextBuffer {
        TextBuffer::from_str(text)
    }

    #[test]
    fn an_empty_query_has_no_matches() {
        assert!(find_matches(&buffer("hello hello"), "").is_empty());
    }

    #[test]
    fn matches_are_found_on_every_line() {
        let matches = find_matches(&buffer("cat\ndog\ncat cat\n"), "cat");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].line, LineIndex::new(0));
        assert_eq!(matches[1].line, LineIndex::new(2));
        assert_eq!(matches[2].line, LineIndex::new(2));
    }

    #[test]
    fn matches_do_not_overlap() {
        // "aaaa" against "aa" -> two matches, not three.
        let matches = find_matches(&buffer("aaaa"), "aa");
        assert_eq!(matches.len(), 2);
        assert_eq!((matches[0].start_column_chars, matches[0].end_column_chars), (0, 2));
        assert_eq!((matches[1].start_column_chars, matches[1].end_column_chars), (2, 4));
    }

    #[test]
    fn the_search_is_case_insensitive() {
        let matches = find_matches(&buffer("Hello WORLD hello"), "hello");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn columns_are_characters_not_bytes() {
        // A multi-byte character before the match must not shift the reported
        // column, because the renderer indexes by character, not by byte.
        let matches = find_matches(&buffer("caf\u{e9} find"), "find");
        assert_eq!(matches[0].start_column_chars, 5, "caf\u{e9} is 4 characters");
    }

    #[test]
    fn set_query_lands_on_the_nearest_match_at_or_after_the_cursor() {
        let mut state = FindState::default();
        let buf = buffer("cat\ndog\ncat\ncat\n");
        state.set_query("cat".to_string(), &buf, LineIndex::new(2));
        assert_eq!(state.current_match().unwrap().line, LineIndex::new(2));
        assert_eq!(state.position(), Some((2, 3)));
    }

    #[test]
    fn set_query_wraps_to_the_first_match_when_nothing_follows_the_cursor() {
        let mut state = FindState::default();
        let buf = buffer("cat\ndog\n");
        state.set_query("cat".to_string(), &buf, LineIndex::new(5));
        assert_eq!(state.current_match().unwrap().line, LineIndex::new(0));
    }

    #[test]
    fn no_matches_means_no_current_match() {
        let mut state = FindState::default();
        let buf = buffer("cat\ndog\n");
        state.set_query("zzz".to_string(), &buf, LineIndex::new(0));
        assert!(state.current_match().is_none());
        assert_eq!(state.position(), None);
    }

    #[test]
    fn advance_cycles_forward_and_wraps() {
        let mut state = FindState::default();
        let buf = buffer("cat\ncat\ncat\n");
        state.set_query("cat".to_string(), &buf, LineIndex::new(0));
        assert_eq!(state.position(), Some((1, 3)));
        state.advance(1);
        assert_eq!(state.position(), Some((2, 3)));
        state.advance(1);
        assert_eq!(state.position(), Some((3, 3)));
        state.advance(1);
        assert_eq!(state.position(), Some((1, 3)), "advancing past the last match wraps");
    }

    #[test]
    fn advance_backwards_wraps_the_other_way() {
        let mut state = FindState::default();
        let buf = buffer("cat\ncat\n");
        state.set_query("cat".to_string(), &buf, LineIndex::new(0));
        state.advance(-1);
        assert_eq!(
            state.position(),
            Some((2, 2)),
            "backing up from the first match wraps to the last"
        );
    }

    #[test]
    fn advancing_with_no_matches_does_nothing() {
        let mut state = FindState::default();
        state.advance(1);
        assert_eq!(state.position(), None);
    }

    #[test]
    fn clear_returns_to_the_not_searching_state() {
        let mut state = FindState::default();
        let buf = buffer("cat\n");
        state.set_query("cat".to_string(), &buf, LineIndex::new(0));
        state.clear();
        assert_eq!(state.query(), "");
        assert!(state.matches().is_empty());
        assert_eq!(state.position(), None);
    }
}
