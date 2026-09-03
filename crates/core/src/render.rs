//! Viewport, invalidation and `RenderSnapshot` (specification sections 26-28, 66).
//!
//! The renderer never reads mutable editor state. Instead the core builds an
//! immutable [`RenderSnapshot`] describing exactly one frame's worth of
//! presentation - the visible lines, where the caret is, which spans are
//! selected - and publishes it. A snapshot is not the document: it holds no
//! history, no persistence state and no text outside the viewport, which is why
//! a 1 GB file costs the same to render as a small one.

use crate::document::{ContentRevision, Document, DocumentId, ExternalState, PersistenceState};
use ls_buffer::{unicode, DisplayColumn, LineIndex};
use std::ops::Range;
use std::sync::Arc;

/// What changed since the previous snapshot (specification section 28).
///
/// Line ranges are half-open. An end of `usize::MAX` means "and everything
/// below", which is what a structural edit causes: inserting a line renumbers
/// every line after it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Invalidation {
    /// Lines whose text or layout changed.
    pub text_lines: Option<Range<usize>>,
    /// Lines the caret left or entered.
    pub cursor_lines: Option<Range<usize>>,
    /// Lines whose selection highlight changed.
    pub selection_lines: Option<Range<usize>>,
    /// The visible region itself moved or resized.
    pub viewport: bool,
}

impl Invalidation {
    pub fn everything() -> Self {
        Invalidation {
            text_lines: Some(0..usize::MAX),
            cursor_lines: Some(0..usize::MAX),
            selection_lines: Some(0..usize::MAX),
            viewport: true,
        }
    }

    pub fn text(start: usize, end: usize) -> Self {
        Invalidation { text_lines: Some(start..end), ..Default::default() }
    }

    /// Everything from `start` downwards, for edits that change line numbering.
    pub fn text_from(start: usize) -> Self {
        Invalidation::text(start, usize::MAX)
    }

    pub fn cursor(old_line: usize, new_line: usize) -> Self {
        let (start, end) = (old_line.min(new_line), old_line.max(new_line) + 1);
        Invalidation { cursor_lines: Some(start..end), ..Default::default() }
    }

    pub fn selection(start: usize, end: usize) -> Self {
        Invalidation { selection_lines: Some(start..end), ..Default::default() }
    }

    pub fn viewport_moved() -> Self {
        Invalidation { viewport: true, ..Default::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.text_lines.is_none()
            && self.cursor_lines.is_none()
            && self.selection_lines.is_none()
            && !self.viewport
    }

    /// True when this invalidation covers the whole document.
    pub fn is_everything(&self) -> bool {
        matches!(&self.text_lines, Some(range) if range.start == 0 && range.end == usize::MAX)
    }

    /// Whether `line`'s text needs to be laid out again.
    pub fn text_covers(&self, line: usize) -> bool {
        self.text_lines.as_ref().is_some_and(|range| range.contains(&line))
    }

    pub fn merge(&mut self, other: Invalidation) {
        self.text_lines = union(self.text_lines.take(), other.text_lines);
        self.cursor_lines = union(self.cursor_lines.take(), other.cursor_lines);
        self.selection_lines = union(self.selection_lines.take(), other.selection_lines);
        self.viewport |= other.viewport;
    }
}

/// Smallest range covering both inputs. Merging disjoint ranges over-reports
/// rather than tracking a set: two ranges per frame is the common case and an
/// over-report only costs redundant layout, never a wrong frame.
fn union(left: Option<Range<usize>>, right: Option<Range<usize>>) -> Option<Range<usize>> {
    match (left, right) {
        (None, other) | (other, None) => other,
        (Some(a), Some(b)) => Some(a.start.min(b.start)..a.end.max(b.end)),
    }
}

/// The visible region, in document coordinates.
///
/// Pixels belong to the renderer; the core only needs to know which lines and
/// columns are on screen so it can build a snapshot of exactly those.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Viewport {
    pub first_line: LineIndex,
    pub visible_lines: usize,
    pub first_column: DisplayColumn,
    pub visible_columns: usize,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            first_line: LineIndex::ZERO,
            visible_lines: 40,
            first_column: DisplayColumn::ZERO,
            visible_columns: 120,
        }
    }
}

impl Viewport {
    /// Half-open range of lines this viewport shows, clamped to the document.
    pub fn line_range(&self, total_lines: usize) -> Range<usize> {
        let start = self.first_line.get().min(total_lines.saturating_sub(1));
        let end = (start + self.visible_lines).min(total_lines);
        start..end
    }

    /// Scrolls so that `line` is visible, moving as little as possible.
    pub fn scrolled_to_reveal(&self, line: LineIndex, total_lines: usize) -> Viewport {
        let mut viewport = *self;
        let line = line.get();
        let first = self.first_line.get();
        if line < first {
            viewport.first_line = LineIndex::new(line);
        } else if self.visible_lines > 0 && line >= first + self.visible_lines {
            viewport.first_line = LineIndex::new(line + 1 - self.visible_lines);
        }
        let max_first = total_lines.saturating_sub(1);
        viewport.first_line = LineIndex::new(viewport.first_line.get().min(max_first));
        viewport
    }
}

/// Characters kept past the right edge of the viewport, so a small horizontal
/// scroll does not need a new snapshot.
pub const COLUMN_SLACK: usize = 256;

/// One visible line of text, without its line break.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderLine {
    pub index: LineIndex,
    /// Shared so cloning a snapshot does not copy the visible text.
    pub text: Arc<str>,
    /// The line continues past what this snapshot carries. Snapshot cost stays
    /// proportional to the viewport even for a document that is one long line.
    pub truncated: bool,
}

/// Where to draw a caret.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CursorPresentation {
    pub line: LineIndex,
    /// Characters into the line; what the renderer needs to place the caret.
    pub column_chars: usize,
    /// Display column, for the status bar.
    pub display_column: DisplayColumn,
    pub primary: bool,
}

/// A selected span on one line.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SelectionSpan {
    pub line: LineIndex,
    pub start_column_chars: usize,
    pub end_column_chars: usize,
    /// The selection continues onto the next line, so the highlight should
    /// extend past the last character.
    pub includes_line_break: bool,
}

/// Document facts the shell displays. Bundling them into the snapshot keeps the
/// UI from reaching into the document for status-bar text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentPresentation {
    pub display_name: String,
    pub path: Option<String>,
    pub dirty: bool,
    pub language: &'static str,
    pub encoding: &'static str,
    pub line_ending: &'static str,
    pub mixed_line_endings: bool,
    pub external_state: ExternalState,
    pub persistence_state: PersistenceState,
    pub can_undo: bool,
    pub can_redo: bool,
}

/// An immutable presentation snapshot for one rendering update
/// (specification section 26).
///
/// Once published it never changes: there are no `&mut self` methods on this
/// type, and the editor hands it out behind an `Arc`.
#[derive(Clone, Debug)]
pub struct RenderSnapshot {
    pub document_id: DocumentId,
    pub content_revision: ContentRevision,
    pub viewport: Viewport,
    pub total_lines: usize,
    pub longest_visible_columns: usize,
    pub lines: Vec<RenderLine>,
    pub cursors: Vec<CursorPresentation>,
    pub selections: Vec<SelectionSpan>,
    /// Empty in Stage 1; the field exists so the renderer's contract does not
    /// change when diagnostics arrive.
    pub diagnostics: Vec<Diagnostic>,
    /// Empty in Stage 1 (no syntax highlighting, no Git decorations).
    pub decorations: Vec<Decoration>,
    pub invalidation: Invalidation,
    pub document: DocumentPresentation,
}

/// Reserved for the Foundation Stage; never produced in Stage 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: LineIndex,
    pub start_column_chars: usize,
    pub end_column_chars: usize,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Hint,
    Information,
    Warning,
    Error,
}

/// Reserved for the Foundation Stage; never produced in Stage 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoration {
    pub line: LineIndex,
    pub start_column_chars: usize,
    pub end_column_chars: usize,
    pub kind: DecorationKind,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DecorationKind {
    SyntaxToken(crate::highlight::TokenKind),
    GitChange,
    SearchMatch,
}

impl RenderSnapshot {
    /// The line the primary caret is on, for the status bar.
    pub fn primary_cursor(&self) -> Option<&CursorPresentation> {
        self.cursors.iter().find(|c| c.primary).or_else(|| self.cursors.first())
    }

    pub fn line_text(&self, line: LineIndex) -> Option<&str> {
        self.lines.iter().find(|l| l.index == line).map(|l| l.text.as_ref())
    }
}

/// Builds the snapshot for one frame.
///
/// Only the visible lines are copied, so cost is proportional to the viewport,
/// not to the document.
pub fn build_snapshot(document: &mut Document, viewport: Viewport) -> RenderSnapshot {
    let _timer = ls_perf::metric(ls_perf::names::SNAPSHOT_BUILD).timer();

    let buffer = document.text();
    let total_lines = buffer.len_lines();
    let range = viewport.line_range(total_lines);
    let tab_width = document.settings().tab_width;

    let max_chars = viewport.first_column.get() + viewport.visible_columns + COLUMN_SLACK;
    let mut lines = Vec::with_capacity(range.len());
    let mut longest_visible_columns = 0;
    for index in range.clone() {
        let line = LineIndex::new(index);
        let line_range = buffer.line_range(line);
        let length = line_range.end - line_range.start;
        let kept = length.min(max_chars);
        let text = buffer.slice(line_range.start..(line_range.start + kept));
        longest_visible_columns =
            longest_visible_columns.max(unicode::display_width(&text, tab_width));
        lines.push(RenderLine {
            index: line,
            text: Arc::from(text.as_str()),
            truncated: kept < length,
        });
    }

    let mut cursors = Vec::new();
    let mut selections = Vec::new();
    for (position, selection) in document.selections().iter().enumerate() {
        let head_line = buffer.char_to_line(selection.head);
        let line_start = buffer.line_range(head_line).start;
        let column_chars = selection.head - line_start;
        let display_column =
            unicode::display_column_in(buffer, line_start, selection.head, tab_width);
        cursors.push(CursorPresentation {
            line: head_line,
            column_chars,
            display_column,
            primary: position == 0,
        });

        if selection.is_caret() {
            continue;
        }
        let span = selection.range();
        let first = buffer.char_to_line(span.start).get().max(range.start);
        let last = buffer.char_to_line(span.end).get().min(range.end.saturating_sub(1));
        for index in first..=last {
            if index >= range.end {
                break;
            }
            let line = LineIndex::new(index);
            let line_range = buffer.line_range(line);
            let start = span.start.max_value(line_range.start);
            let end = span.end.min_value(line_range.end);
            if start > end {
                continue;
            }
            selections.push(SelectionSpan {
                line,
                start_column_chars: start - line_range.start,
                end_column_chars: end - line_range.start,
                includes_line_break: span.end > line_range.end,
            });
        }
    }

    let document_presentation = DocumentPresentation {
        display_name: document.display_name().to_string(),
        path: document.path().map(|p| p.display_string()),
        dirty: document.is_dirty(),
        language: document.language().name(),
        encoding: document.encoding().label(),
        line_ending: document.line_ending().label(),
        mixed_line_endings: document.has_mixed_line_endings(),
        external_state: document.external_state(),
        persistence_state: document.persistence_state(),
        can_undo: document.can_undo(),
        can_redo: document.can_redo(),
    };
    let content_revision = document.revision();
    let document_id = document.id();
    let invalidation = document.take_invalidation();

    // Only the visible slice: an off-screen match or token needs no
    // decoration, the same reasoning that already limits `lines` to the
    // viewport. Syntax tokens are scanned from `lines`, which is already the
    // exact visible text -- reusing it rather than re-slicing the buffer.
    let mut syntax_decorations = Vec::new();
    for line in &lines {
        for token in document.tokenize_visible_line(line.index.get()) {
            syntax_decorations.push(Decoration {
                line: line.index,
                start_column_chars: token.start_column_chars,
                end_column_chars: token.end_column_chars,
                kind: DecorationKind::SyntaxToken(token.kind),
            });
        }
    }

    let search_decorations =
        document.find().matches().iter().filter(|found| range.contains(&found.line.get())).map(
            |found| Decoration {
                line: found.line,
                start_column_chars: found.start_column_chars,
                end_column_chars: found.end_column_chars,
                kind: DecorationKind::SearchMatch,
            },
        );

    let decorations: Vec<Decoration> =
        syntax_decorations.into_iter().chain(search_decorations).collect();

    RenderSnapshot {
        document_id,
        content_revision,
        viewport,
        total_lines,
        longest_visible_columns,
        lines,
        cursors,
        selections,
        diagnostics: document.diagnostics().to_vec(),
        decorations,
        invalidation,
        document: document_presentation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{DocumentSettings, EditResult};
    use crate::history::EditKind;
    use crate::selection::Selection;
    use ls_buffer::CharOffset;

    fn document(text: &str) -> Document {
        let mut document =
            Document::untitled(DocumentId::new(1), "untitled-1", DocumentSettings::default());
        let _: EditResult = document.insert(text, EditKind::Paste);
        document.set_selection(Selection::caret(CharOffset::ZERO));
        document.take_invalidation();
        document
    }

    /// A document whose language is detected as Rust, for syntax-highlighting
    /// tests -- an untitled document is always `PlainText`, so those need a
    /// path (never touching the filesystem: `unverified` just normalizes it).
    fn rust_document(text: &str) -> Document {
        let path = ls_platform::CanonicalPath::unverified("scratch.rs").unwrap();
        let stamp = crate::document::DiskStamp { modified: None, len_bytes: 0 };
        let mut document = Document::from_buffer(
            DocumentId::new(1),
            path,
            ls_buffer::TextBuffer::from_str(text),
            crate::encoding::Encoding::Utf8,
            ls_buffer::LineEnding::Lf,
            false,
            stamp,
            DocumentSettings::default(),
        );
        document.take_invalidation();
        document
    }

    fn viewport(first: usize, count: usize) -> Viewport {
        Viewport {
            first_line: LineIndex::new(first),
            visible_lines: count,
            first_column: DisplayColumn::ZERO,
            visible_columns: 100,
        }
    }

    #[test]
    fn a_snapshot_holds_only_the_visible_lines() {
        let text: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let mut document = document(&text);
        let snapshot = build_snapshot(&mut document, viewport(500, 20));

        assert_eq!(snapshot.lines.len(), 20);
        assert_eq!(snapshot.lines[0].index, LineIndex::new(500));
        assert_eq!(snapshot.lines[0].text.as_ref(), "line 500");
        assert_eq!(snapshot.total_lines, 1001);
    }

    #[test]
    fn a_viewport_past_the_end_is_clamped() {
        let mut document = document("one\ntwo\nthree");
        let snapshot = build_snapshot(&mut document, viewport(50, 10));
        assert_eq!(snapshot.lines.len(), 1, "clamps to the last line");
        assert_eq!(snapshot.lines[0].index, LineIndex::new(2));
    }

    #[test]
    fn the_cursor_is_presented_with_a_display_column() {
        let mut document = document("\tindented");
        document.set_selection(Selection::caret(CharOffset::new(1)));
        let snapshot = build_snapshot(&mut document, viewport(0, 10));
        let cursor = snapshot.primary_cursor().unwrap();
        assert_eq!(cursor.line, LineIndex::ZERO);
        assert_eq!(cursor.column_chars, 1);
        assert_eq!(cursor.display_column, DisplayColumn::new(4), "one tab is four columns");
    }

    #[test]
    fn a_multi_line_selection_produces_one_span_per_line() {
        let mut document = document("aaa\nbbb\nccc\nddd");
        document.set_selection(Selection::new(CharOffset::new(1), CharOffset::new(10)));
        let snapshot = build_snapshot(&mut document, viewport(0, 10));

        assert_eq!(snapshot.selections.len(), 3);
        assert_eq!(snapshot.selections[0].line, LineIndex::new(0));
        assert_eq!(snapshot.selections[0].start_column_chars, 1);
        assert!(snapshot.selections[0].includes_line_break);
        assert_eq!(snapshot.selections[2].line, LineIndex::new(2));
        assert_eq!(snapshot.selections[2].end_column_chars, 2);
        assert!(!snapshot.selections[2].includes_line_break);
    }

    #[test]
    fn a_caret_produces_no_selection_spans() {
        let mut document = document("abc");
        let snapshot = build_snapshot(&mut document, viewport(0, 10));
        assert!(snapshot.selections.is_empty());
        assert_eq!(snapshot.cursors.len(), 1);
    }

    #[test]
    fn selection_spans_are_limited_to_the_viewport() {
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let mut document = document(&text);
        document.select_all();
        let snapshot = build_snapshot(&mut document, viewport(10, 5));
        assert_eq!(snapshot.selections.len(), 5, "only visible lines are described");
        assert_eq!(snapshot.selections[0].line, LineIndex::new(10));
    }

    #[test]
    fn snapshots_carry_the_revision_they_describe() {
        let mut document = document("abc");
        let first = build_snapshot(&mut document, viewport(0, 10));
        document.set_selection(Selection::caret(document.text().end()));
        document.insert("d", EditKind::Typing);
        let second = build_snapshot(&mut document, viewport(0, 10));
        assert!(second.content_revision > first.content_revision);
        assert_eq!(first.lines[0].text.as_ref(), "abc", "the old snapshot is unchanged");
        assert_eq!(second.lines[0].text.as_ref(), "abcd");
    }

    #[test]
    fn invalidation_is_reported_once_then_cleared() {
        let mut document = document("one\ntwo");
        document.set_selection(Selection::caret(CharOffset::new(5)));
        document.insert("X", EditKind::Typing);

        let first = build_snapshot(&mut document, viewport(0, 10));
        assert!(!first.invalidation.is_empty());

        let second = build_snapshot(&mut document, viewport(0, 10));
        assert!(second.invalidation.is_empty(), "nothing changed between the two frames");
    }

    #[test]
    fn merging_invalidations_covers_both() {
        let mut invalidation = Invalidation::text(2, 3);
        invalidation.merge(Invalidation::text(7, 9));
        assert_eq!(invalidation.text_lines, Some(2..9));
        assert!(invalidation.text_covers(5), "the union over-reports rather than missing a line");
        assert!(!invalidation.text_covers(1));

        invalidation.merge(Invalidation::viewport_moved());
        assert!(invalidation.viewport);
        assert!(!invalidation.is_everything());
        invalidation.merge(Invalidation::everything());
        assert!(invalidation.is_everything());
    }

    #[test]
    fn scrolling_to_reveal_moves_as_little_as_possible() {
        let viewport = viewport(10, 20);
        assert_eq!(
            viewport.scrolled_to_reveal(LineIndex::new(15), 100),
            viewport,
            "already visible"
        );
        assert_eq!(
            viewport.scrolled_to_reveal(LineIndex::new(5), 100).first_line,
            LineIndex::new(5)
        );
        assert_eq!(
            viewport.scrolled_to_reveal(LineIndex::new(40), 100).first_line,
            LineIndex::new(21)
        );
    }

    #[test]
    fn presentation_carries_document_status() {
        let mut document = document("text");
        let snapshot = build_snapshot(&mut document, viewport(0, 10));
        assert_eq!(snapshot.document.display_name, "untitled-1");
        assert!(snapshot.document.dirty);
        assert_eq!(snapshot.document.language, "Plain Text");
        assert_eq!(snapshot.document.encoding, "UTF-8");
        assert!(snapshot.diagnostics.is_empty(), "Stage 1 produces no diagnostics");
        assert!(snapshot.decorations.is_empty(), "Stage 1 produces no decorations");
    }

    #[test]
    fn very_long_lines_are_truncated_to_the_viewport() {
        // One line of 500_000 characters: the snapshot must stay viewport-sized.
        let mut document = document(&"x".repeat(500_000));
        let snapshot = build_snapshot(&mut document, viewport(0, 10));
        let line = &snapshot.lines[0];
        assert!(line.truncated, "the line should be marked as clipped");
        assert!(
            line.text.len() <= 100 + COLUMN_SLACK,
            "kept {} characters for a 100-column viewport",
            line.text.len()
        );
    }

    #[test]
    fn the_cursor_column_is_reported_on_a_very_long_line() {
        let mut document = document(&"x".repeat(500_000));
        document.set_selection(Selection::caret(CharOffset::new(400_000)));
        let snapshot = build_snapshot(&mut document, viewport(0, 10));
        let cursor = snapshot.primary_cursor().unwrap();
        assert_eq!(cursor.column_chars, 400_000);
        assert_eq!(cursor.display_column, DisplayColumn::new(400_000));
    }

    #[test]
    fn the_longest_visible_line_is_measured_for_horizontal_scrolling() {
        let mut document = document("short\n\tlonger line with a tab");
        let snapshot = build_snapshot(&mut document, viewport(0, 10));
        // One tab (4 columns) plus "longer line with a tab" (22 characters).
        assert_eq!(snapshot.longest_visible_columns, 4 + 22);
    }

    // --- syntax highlighting (item 8): the incremental case end to end -------

    #[test]
    fn a_block_comment_spanning_lines_highlights_as_one_comment_through_the_snapshot() {
        let mut document = rust_document("/*\ninside\n*/\nlet x = 10;");
        let snapshot = build_snapshot(&mut document, viewport(0, 10));

        let is_comment = |line: usize| {
            snapshot.decorations.iter().any(|d| {
                d.line == LineIndex::new(line) && matches!(d.kind, DecorationKind::SyntaxToken(_))
            })
        };
        assert!(is_comment(0), "the opening line is inside the comment");
        assert!(is_comment(1), "a line with no markers at all is still inside it");
        assert!(is_comment(2), "the closing line is inside the comment");

        // Line 3 is real code again: "let" is a keyword, not a comment.
        let keyword_on_line_3 = snapshot.decorations.iter().any(|d| {
            d.line == LineIndex::new(3)
                && d.kind == DecorationKind::SyntaxToken(crate::highlight::TokenKind::Keyword)
        });
        assert!(keyword_on_line_3, "code after the comment closes is tokenized normally");
    }

    #[test]
    fn editing_before_a_block_comment_reopens_it_for_relexing() {
        // The regression this guards: once line 0's exit state is cached as
        // BlockComment, an edit at line 0 must invalidate that cached state --
        // otherwise closing the comment would never be noticed.
        let mut document = rust_document("/* still open\nsecond line");
        let _ = build_snapshot(&mut document, viewport(0, 10));

        // Close the comment by editing the first line.
        document.set_selection(Selection::caret(CharOffset::new(2)));
        let _: EditResult = document.insert(" */ int x =", EditKind::Paste);
        let snapshot = build_snapshot(&mut document, viewport(0, 10));

        let comment_on_line_1 = snapshot.decorations.iter().any(|d| {
            d.line == LineIndex::new(1) && matches!(d.kind, DecorationKind::SyntaxToken(_))
        });
        assert!(!comment_on_line_1, "the comment closed on line 0, so line 1 must be code again");
    }
}
