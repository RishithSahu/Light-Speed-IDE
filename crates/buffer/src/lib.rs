//! LightSpeed text storage.
//!
//! This crate owns the document text container and the position vocabulary that
//! the rest of the editor speaks:
//!
//! * [`TextBuffer`] - incremental insert/delete/replace with O(log n) offset and
//!   line conversion, backed by a B-tree rope (specification section 16).
//! * [`ByteOffset`], [`CharOffset`], [`LineIndex`], [`DisplayColumn`] - the
//!   distinct position types the specification demands (section 15).
//! * [`line_ending`] - detection, normalization and restoration of LF/CRLF/CR
//!   (section 18).
//! * [`unicode`] - grapheme-cluster movement and display-width queries
//!   (section 17).
//!
//! The crate has no knowledge of files, encodings, undo or the UI. It is pure
//! text mechanics and is testable without any of them.

pub mod line_ending;
mod offsets;
mod rope;
mod text_buffer;
pub mod unicode;

pub use line_ending::{LineEnding, LineEndingAnalysis};
pub use offsets::{ByteOffset, CharOffset, DisplayColumn, LineIndex};
pub use text_buffer::TextBuffer;

/// Chunk size limits of the underlying rope, exposed for benchmarks and the
/// memory report.
pub mod tuning {
    pub const MAX_LEAF_BYTES: usize = super::rope::MAX_LEAF_BYTES;
    pub const TARGET_LEAF_BYTES: usize = super::rope::TARGET_LEAF_BYTES;
    pub const MAX_CHILDREN: usize = super::rope::MAX_CHILDREN;
}
