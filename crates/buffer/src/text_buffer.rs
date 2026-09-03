//! The document text container (specification section 16).
//!
//! `TextBuffer` supports insert, delete, replace, range access, line access and
//! offset conversion, and applies edits incrementally: an edit touches one
//! chunk plus the nodes above it, never the whole document. The representation
//! is a B-tree rope; the reasoning and the measurements behind that choice are
//! in `docs/adr/ADR-0001-textbuffer-representation.md`.
//!
//! Offsets are character (Unicode scalar) offsets. Byte offsets, line indices
//! and display columns are separate types and require an explicit conversion.

use crate::offsets::{ByteOffset, CharOffset, LineIndex};
use crate::rope::{self, Node};
use std::ops::Range;
use std::sync::Arc;

/// Immutable-on-clone text storage for one document.
#[derive(Clone, Debug)]
pub struct TextBuffer {
    root: Arc<Node>,
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl TextBuffer {
    /// An empty buffer: zero characters, one (empty) line.
    pub fn new() -> Self {
        TextBuffer { root: Arc::new(Node::empty()) }
    }

    /// Builds a buffer from existing text in one balanced pass.
    ///
    /// The text must already have normalized (`\n`) line endings; see
    /// [`crate::line_ending`].
    ///
    /// Deliberately not `FromStr`: that trait returns a `Result`, and building a
    /// buffer from text cannot fail.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Self {
        TextBuffer { root: rope::build(text) }
    }

    /// O(1) snapshot. The clone shares storage until either side is edited.
    pub fn snapshot(&self) -> TextBuffer {
        TextBuffer { root: Arc::clone(&self.root) }
    }

    pub fn len_bytes(&self) -> usize {
        self.root.info().bytes
    }

    pub fn len_chars(&self) -> usize {
        self.root.info().chars
    }

    /// Number of lines. An empty buffer has one line; a buffer ending in a line
    /// break has an empty final line.
    pub fn len_lines(&self) -> usize {
        self.root.info().line_breaks + 1
    }

    pub fn is_empty(&self) -> bool {
        self.len_chars() == 0
    }

    /// Depth of the underlying tree; exposed for benchmarks and diagnostics.
    pub fn depth(&self) -> usize {
        self.root.depth()
    }

    /// End of the document as a position.
    pub fn end(&self) -> CharOffset {
        CharOffset::new(self.len_chars())
    }

    /// Clamps a position into `0..=len_chars`.
    pub fn clamp(&self, position: CharOffset) -> CharOffset {
        position.min_value(self.end())
    }

    /// Inserts `text` at `at`.
    ///
    /// # Panics
    /// If `at` is past the end of the document.
    pub fn insert(&mut self, at: CharOffset, text: &str) {
        let length = self.len_chars();
        assert!(
            at.get() <= length,
            "insert position {at} is past the end of the document ({length} chars)"
        );
        if text.is_empty() {
            return;
        }

        let mut position = at.get();
        let mut start = 0;
        while start < text.len() {
            // Insertions are chunked so a leaf never has to split more than once.
            let end = rope::chunk_end(text, start, rope::TARGET_LEAF_BYTES);
            let piece = &text[start..end];
            if let Some(sibling) = rope::insert(&mut self.root, position, piece) {
                let left = std::mem::replace(&mut self.root, Arc::new(Node::empty()));
                self.root = Arc::new(Node::internal_pair(left, sibling));
            }
            position += rope::count_chars(piece);
            start = end;
        }
    }

    /// Removes the character range `range`.
    ///
    /// # Panics
    /// If the range is inverted or extends past the end of the document.
    pub fn remove(&mut self, range: Range<CharOffset>) {
        let length = self.len_chars();
        assert!(range.start <= range.end, "inverted range {range:?}");
        assert!(
            range.end.get() <= length,
            "remove range end {} is past the end of the document ({length} chars)",
            range.end
        );
        if range.start == range.end {
            return;
        }
        rope::remove(&mut self.root, range.start.get(), range.end.get());
        rope::collapse_root(&mut self.root);
    }

    /// Replaces `range` with `text` in one call.
    pub fn replace(&mut self, range: Range<CharOffset>, text: &str) {
        let start = range.start;
        self.remove(range);
        self.insert(start, text);
    }

    /// The character at `position`, or `None` at the end of the document.
    pub fn char_at(&self, position: CharOffset) -> Option<char> {
        self.root.char_at(position.get())
    }

    /// Copies `range` into a new `String`.
    ///
    /// # Panics
    /// If the range is inverted or extends past the end of the document.
    pub fn slice(&self, range: Range<CharOffset>) -> String {
        let length = self.len_chars();
        assert!(range.start <= range.end, "inverted range {range:?}");
        assert!(range.end.get() <= length, "slice end {} past document end", range.end);
        let mut out = String::with_capacity(range.end - range.start);
        self.root.append_slice(range.start.get(), range.end.get(), &mut out);
        out
    }

    /// Iterates the internal chunks in document order. Used for saving and
    /// scanning without materializing the whole document.
    pub fn chunks(&self) -> impl Iterator<Item = &str> {
        rope::Chunks::new(&self.root)
    }

    /// Writes the document to a sink chunk by chunk.
    pub fn write_to<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        for chunk in self.chunks() {
            writer.write_all(chunk.as_bytes())?;
        }
        Ok(())
    }

    /// Line containing `position`.
    pub fn char_to_line(&self, position: CharOffset) -> LineIndex {
        LineIndex::new(self.root.char_to_line(self.clamp(position).get()))
    }

    /// First character of `line`. A line past the end returns the document end.
    pub fn line_to_char(&self, line: LineIndex) -> CharOffset {
        CharOffset::new(self.root.line_to_char(line.get()))
    }

    pub fn char_to_byte(&self, position: CharOffset) -> ByteOffset {
        ByteOffset::new(self.root.char_to_byte(self.clamp(position).get()))
    }

    pub fn byte_to_char(&self, position: ByteOffset) -> CharOffset {
        CharOffset::new(self.root.byte_to_char(position.get().min(self.len_bytes())))
    }

    /// Character range of `line`, excluding its line break.
    pub fn line_range(&self, line: LineIndex) -> Range<CharOffset> {
        let start = self.line_to_char(line);
        let next = self.line_to_char(line + 1);
        let mut end = if next > start { next } else { self.end() };
        if end > start {
            // Trim the line break itself, and a preceding `\r` if the document
            // still holds one (a lone `\r` is ordinary text, but `\r\n` inside a
            // buffer that skipped normalization should not render as a box).
            if self.char_at(end - 1) == Some('\n') {
                end -= 1;
                if end > start && self.char_at(end - 1) == Some('\r') {
                    end -= 1;
                }
            }
        }
        start..end
    }

    /// Number of characters on `line`, excluding its line break.
    pub fn line_len_chars(&self, line: LineIndex) -> usize {
        let range = self.line_range(line);
        range.end - range.start
    }

    /// Text of `line`, excluding its line break.
    pub fn line_text(&self, line: LineIndex) -> String {
        self.slice(self.line_range(line))
    }

    /// Position of `column` characters into `line`, clamped to the line's end.
    pub fn position_at(&self, line: LineIndex, column: usize) -> CharOffset {
        let range = self.line_range(line);
        let length = range.end - range.start;
        range.start + column.min(length)
    }

    /// Checks the rope's structural invariants. Test and benchmark use only.
    pub fn validate(&self) -> Result<(), String> {
        let depth = self.root.depth();
        self.root.validate(depth).map(|_| ())
    }
}

impl std::fmt::Display for TextBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for chunk in self.chunks() {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}

impl PartialEq for TextBuffer {
    /// Compares contents, not structure: two buffers built by different edit
    /// sequences are equal when they hold the same text.
    fn eq(&self, other: &Self) -> bool {
        if self.len_bytes() != other.len_bytes() || self.len_chars() != other.len_chars() {
            return false;
        }
        if Arc::ptr_eq(&self.root, &other.root) {
            return true;
        }
        let mut left = self.chunks();
        let mut right = other.chunks();
        let (mut left_chunk, mut right_chunk) = ("", "");
        loop {
            if left_chunk.is_empty() {
                left_chunk = left.next().unwrap_or("");
            }
            if right_chunk.is_empty() {
                right_chunk = right.next().unwrap_or("");
            }
            if left_chunk.is_empty() && right_chunk.is_empty() {
                return true;
            }
            let shared = left_chunk.len().min(right_chunk.len());
            if shared == 0 || left_chunk[..shared] != right_chunk[..shared] {
                return false;
            }
            left_chunk = &left_chunk[shared..];
            right_chunk = &right_chunk[shared..];
        }
    }
}

impl Eq for TextBuffer {}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> TextBuffer {
        let buffer = TextBuffer::from_str(text);
        buffer.validate().expect("freshly built buffer is valid");
        assert_eq!(buffer.to_string(), text);
        buffer
    }

    #[test]
    fn empty_buffer_has_one_line() {
        let buffer = TextBuffer::new();
        assert_eq!(buffer.len_chars(), 0);
        assert_eq!(buffer.len_bytes(), 0);
        assert_eq!(buffer.len_lines(), 1);
        assert!(buffer.is_empty());
        assert_eq!(buffer.line_text(LineIndex::ZERO), "");
    }

    #[test]
    fn insert_and_remove_round_trip() {
        let mut buffer = buffer("Hello World");
        buffer.insert(CharOffset::new(6), "Beautiful ");
        assert_eq!(buffer.to_string(), "Hello Beautiful World");
        buffer.validate().unwrap();

        buffer.remove(CharOffset::new(6)..CharOffset::new(16));
        assert_eq!(buffer.to_string(), "Hello World");
        buffer.validate().unwrap();
    }

    #[test]
    fn replace_is_remove_then_insert() {
        let mut buffer = buffer("one two three");
        buffer.replace(CharOffset::new(4)..CharOffset::new(7), "TWO");
        assert_eq!(buffer.to_string(), "one TWO three");
    }

    #[test]
    fn insert_at_the_very_end_is_allowed() {
        let mut buffer = buffer("abc");
        buffer.insert(CharOffset::new(3), "d");
        assert_eq!(buffer.to_string(), "abcd");
    }

    #[test]
    #[should_panic(expected = "past the end")]
    fn insert_past_the_end_panics() {
        let mut buffer = buffer("abc");
        buffer.insert(CharOffset::new(4), "d");
    }

    #[test]
    #[should_panic(expected = "past the end")]
    fn remove_past_the_end_panics() {
        let mut buffer = buffer("abc");
        buffer.remove(CharOffset::new(1)..CharOffset::new(9));
    }

    #[test]
    fn empty_edits_are_no_ops() {
        let mut buffer = buffer("abc");
        buffer.insert(CharOffset::new(1), "");
        buffer.remove(CharOffset::new(1)..CharOffset::new(1));
        assert_eq!(buffer.to_string(), "abc");
    }

    #[test]
    fn line_queries_on_a_multi_line_document() {
        let buffer = buffer("alpha\nbeta\n\ngamma");
        assert_eq!(buffer.len_lines(), 4);
        assert_eq!(buffer.line_text(LineIndex::new(0)), "alpha");
        assert_eq!(buffer.line_text(LineIndex::new(1)), "beta");
        assert_eq!(buffer.line_text(LineIndex::new(2)), "");
        assert_eq!(buffer.line_text(LineIndex::new(3)), "gamma");

        assert_eq!(buffer.line_to_char(LineIndex::new(0)), CharOffset::new(0));
        assert_eq!(buffer.line_to_char(LineIndex::new(1)), CharOffset::new(6));
        assert_eq!(buffer.line_to_char(LineIndex::new(2)), CharOffset::new(11));
        assert_eq!(buffer.line_to_char(LineIndex::new(3)), CharOffset::new(12));

        assert_eq!(buffer.char_to_line(CharOffset::new(0)), LineIndex::new(0));
        assert_eq!(buffer.char_to_line(CharOffset::new(5)), LineIndex::new(0));
        assert_eq!(buffer.char_to_line(CharOffset::new(6)), LineIndex::new(1));
        assert_eq!(buffer.char_to_line(CharOffset::new(12)), LineIndex::new(3));
        assert_eq!(buffer.line_len_chars(LineIndex::new(3)), 5);
    }

    #[test]
    fn trailing_line_break_creates_an_empty_final_line() {
        let buffer = buffer("a\nb\n");
        assert_eq!(buffer.len_lines(), 3);
        assert_eq!(buffer.line_text(LineIndex::new(2)), "");
        assert_eq!(buffer.line_to_char(LineIndex::new(2)), CharOffset::new(4));
    }

    #[test]
    fn line_past_the_end_clamps_to_the_document_end() {
        let buffer = buffer("a\nb");
        assert_eq!(buffer.line_to_char(LineIndex::new(50)), buffer.end());
        assert_eq!(buffer.line_text(LineIndex::new(50)), "");
    }

    #[test]
    fn offset_conversions_handle_multi_byte_characters() {
        // "a" 1 byte, "é" 2 bytes, "☃" 3 bytes, emoji 4 bytes.
        let buffer = buffer("aé☃\u{1F600}b");
        assert_eq!(buffer.len_chars(), 5);
        assert_eq!(buffer.len_bytes(), 1 + 2 + 3 + 4 + 1);

        assert_eq!(buffer.char_to_byte(CharOffset::new(0)), ByteOffset::new(0));
        assert_eq!(buffer.char_to_byte(CharOffset::new(1)), ByteOffset::new(1));
        assert_eq!(buffer.char_to_byte(CharOffset::new(2)), ByteOffset::new(3));
        assert_eq!(buffer.char_to_byte(CharOffset::new(3)), ByteOffset::new(6));
        assert_eq!(buffer.char_to_byte(CharOffset::new(4)), ByteOffset::new(10));
        assert_eq!(buffer.char_to_byte(CharOffset::new(5)), ByteOffset::new(11));

        assert_eq!(buffer.byte_to_char(ByteOffset::new(10)), CharOffset::new(4));
        assert_eq!(buffer.char_at(CharOffset::new(3)), Some('\u{1F600}'));
        assert_eq!(buffer.char_at(CharOffset::new(5)), None);
    }

    #[test]
    fn editing_inside_multi_byte_text_keeps_chars_intact() {
        let mut buffer = buffer("héllo wörld");
        buffer.insert(CharOffset::new(1), "\u{1F600}");
        assert_eq!(buffer.to_string(), "h\u{1F600}éllo wörld");
        buffer.remove(CharOffset::new(1)..CharOffset::new(2));
        assert_eq!(buffer.to_string(), "héllo wörld");
        buffer.validate().unwrap();
    }

    #[test]
    fn slice_returns_exact_ranges() {
        let buffer = buffer("0123456789");
        assert_eq!(buffer.slice(CharOffset::new(0)..CharOffset::new(0)), "");
        assert_eq!(buffer.slice(CharOffset::new(2)..CharOffset::new(5)), "234");
        assert_eq!(buffer.slice(CharOffset::ZERO..buffer.end()), "0123456789");
    }

    #[test]
    fn buffers_spanning_many_chunks_stay_correct() {
        // Far more than one leaf, with multi-byte characters at chunk edges.
        let unit = "line of text with a wide character \u{1F600} at the end\n";
        let text = unit.repeat(4000);
        let mut buffer = buffer(&text);
        assert_eq!(buffer.len_lines(), 4001);
        assert!(buffer.depth() >= 2, "expected a real tree, got depth {}", buffer.depth());

        // Edits at the start, middle and end all keep the tree valid.
        buffer.insert(CharOffset::ZERO, "START ");
        let middle = CharOffset::new(buffer.len_chars() / 2);
        buffer.insert(middle, " MIDDLE ");
        let end = buffer.end();
        buffer.insert(end, "END");
        buffer.validate().unwrap();

        assert!(buffer.to_string().starts_with("START line of text"));
        assert!(buffer.to_string().ends_with("END"));
        assert_eq!(buffer.line_text(LineIndex::new(0)), "START ".to_string() + unit.trim_end());
    }

    #[test]
    fn large_deletions_rebalance_the_tree() {
        let text = "abcdefghij\n".repeat(20_000);
        let mut buffer = buffer(&text);
        let original_lines = buffer.len_lines();

        // Delete the middle 80% of the document in one operation.
        let start = CharOffset::new(buffer.len_chars() / 10);
        let end = CharOffset::new(buffer.len_chars() * 9 / 10);
        let removed = end - start;
        buffer.remove(start..end);

        buffer.validate().unwrap();
        assert_eq!(buffer.len_chars(), text.chars().count() - removed);
        assert!(buffer.len_lines() < original_lines);
        assert!(
            buffer.depth() <= 6,
            "tree should shrink after a large delete, depth is {}",
            buffer.depth()
        );

        let expected: String = {
            let chars: Vec<char> = text.chars().collect();
            let head: String = chars[..start.get()].iter().collect();
            let tail: String = chars[end.get()..].iter().collect();
            head + &tail
        };
        assert_eq!(buffer.to_string(), expected);
    }

    #[test]
    fn deleting_everything_leaves_a_valid_empty_buffer() {
        let mut buffer = buffer(&"x".repeat(50_000));
        buffer.remove(CharOffset::ZERO..buffer.end());
        buffer.validate().unwrap();
        assert!(buffer.is_empty());
        assert_eq!(buffer.len_lines(), 1);
        assert_eq!(buffer.depth(), 0, "an empty buffer should collapse to a single leaf");

        buffer.insert(CharOffset::ZERO, "back again");
        assert_eq!(buffer.to_string(), "back again");
    }

    #[test]
    fn inserting_a_large_block_chunks_it() {
        let mut buffer = buffer("[]");
        let block = "0123456789".repeat(20_000); // 200 KB in one insert
        buffer.insert(CharOffset::new(1), &block);
        buffer.validate().unwrap();
        assert_eq!(buffer.len_chars(), 2 + block.chars().count());
        assert!(buffer.to_string().starts_with("[0123456789"));
        assert!(buffer.to_string().ends_with("6789]"));
    }

    #[test]
    fn typing_one_character_at_a_time_keeps_the_tree_valid() {
        let mut buffer = TextBuffer::new();
        for i in 0..5_000 {
            let position = buffer.end();
            buffer.insert(position, if i % 40 == 39 { "\n" } else { "x" });
        }
        buffer.validate().unwrap();
        assert_eq!(buffer.len_chars(), 5_000);
        assert_eq!(buffer.len_lines(), 125 + 1);
    }

    #[test]
    fn snapshots_are_independent_of_later_edits() {
        let mut buffer = buffer("original text");
        let snapshot = buffer.snapshot();
        buffer.insert(CharOffset::new(9), "and edited ");

        assert_eq!(snapshot.to_string(), "original text");
        assert_eq!(buffer.to_string(), "original and edited text");
        snapshot.validate().unwrap();
        buffer.validate().unwrap();
    }

    #[test]
    fn equality_compares_content_not_structure() {
        let built_at_once = TextBuffer::from_str("hello world");
        let mut typed = TextBuffer::new();
        for (index, ch) in "hello world".chars().enumerate() {
            typed.insert(CharOffset::new(index), &ch.to_string());
        }
        assert_eq!(built_at_once, typed);
        assert_ne!(built_at_once, TextBuffer::from_str("hello worlds"));
    }

    #[test]
    fn write_to_streams_the_document() {
        let text = "streamed content\n".repeat(1000);
        let buffer = buffer(&text);
        let mut sink: Vec<u8> = Vec::new();
        buffer.write_to(&mut sink).unwrap();
        assert_eq!(String::from_utf8(sink).unwrap(), text);
    }

    #[test]
    fn position_at_clamps_to_line_length() {
        let buffer = buffer("ab\ncdef");
        assert_eq!(buffer.position_at(LineIndex::new(0), 1), CharOffset::new(1));
        assert_eq!(buffer.position_at(LineIndex::new(0), 99), CharOffset::new(2));
        assert_eq!(buffer.position_at(LineIndex::new(1), 2), CharOffset::new(5));
    }

    #[test]
    fn edits_are_deterministic() {
        // The same edit script from the same start state must produce the same
        // document, byte for byte (specification section 25).
        let script: Vec<(usize, &str)> = vec![
            (0, "fn main() {\n"),
            (12, "    println!(\"hi\");\n"),
            (32, "}\n"),
            (5, "very_long_name"),
            (0, "// comment\n"),
        ];
        let run = || {
            let mut buffer = TextBuffer::new();
            for (position, text) in &script {
                let clamped = CharOffset::new((*position).min(buffer.len_chars()));
                buffer.insert(clamped, text);
            }
            buffer.to_string()
        };
        assert_eq!(run(), run());
    }
}
