//! Text position types (specification section 15).
//!
//! The specification is explicit that these are different things and must not
//! be confused:
//!
//! ```text
//! ByteOffset  != CharOffset != DisplayColumn
//! LineIndex   indexes lines, not characters
//! ```
//!
//! Making them distinct types means "1 byte = 1 character" and "1 character =
//! 1 column" cannot be assumed by accident; a conversion has to be written out,
//! and conversions are what [`crate::TextBuffer`] provides.
//!
//! Lengths and counts stay as plain `usize`. Only *positions* are newtypes,
//! because positions are what get mixed up.

use std::fmt;
use std::ops::{Add, AddAssign, Sub, SubAssign};

macro_rules! offset_type {
    ($(#[$meta:meta])* $name:ident, $unit:literal) => {
        $(#[$meta])*
        #[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(usize);

        impl $name {
            pub const ZERO: Self = $name(0);

            #[inline]
            pub const fn new(value: usize) -> Self {
                $name(value)
            }

            /// The underlying count. Named `get` rather than exposing the field
            /// so that every unwrap is visible at the call site.
            #[inline]
            pub const fn get(self) -> usize {
                self.0
            }

            #[inline]
            pub fn saturating_sub(self, count: usize) -> Self {
                $name(self.0.saturating_sub(count))
            }

            #[inline]
            pub fn min_value(self, other: Self) -> Self {
                $name(self.0.min(other.0))
            }

            #[inline]
            pub fn max_value(self, other: Self) -> Self {
                $name(self.0.max(other.0))
            }
        }

        impl From<usize> for $name {
            #[inline]
            fn from(value: usize) -> Self {
                $name(value)
            }
        }

        impl Add<usize> for $name {
            type Output = Self;
            #[inline]
            fn add(self, count: usize) -> Self {
                $name(self.0 + count)
            }
        }

        impl AddAssign<usize> for $name {
            #[inline]
            fn add_assign(&mut self, count: usize) {
                self.0 += count;
            }
        }

        impl Sub<usize> for $name {
            type Output = Self;
            #[inline]
            fn sub(self, count: usize) -> Self {
                $name(self.0 - count)
            }
        }

        impl SubAssign<usize> for $name {
            #[inline]
            fn sub_assign(&mut self, count: usize) {
                self.0 -= count;
            }
        }

        /// Distance between two positions, in this position's unit.
        impl Sub<$name> for $name {
            type Output = usize;
            #[inline]
            fn sub(self, other: Self) -> usize {
                self.0 - other.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", self.0, $unit)
            }
        }
    };
}

offset_type!(
    /// Offset in UTF-8 bytes from the start of the document.
    ByteOffset,
    "b"
);

offset_type!(
    /// Offset in Unicode scalar values from the start of the document. This is
    /// the unit the editor core edits and stores selections in.
    CharOffset,
    "c"
);

offset_type!(
    /// Zero-based line number. Line `n` is the text after the `n`th line break.
    LineIndex,
    "L"
);

offset_type!(
    /// Zero-based column measured in display cells, with tabs expanded and
    /// wide (CJK/emoji) characters counted as two. Never a storage offset.
    DisplayColumn,
    "col"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_are_distinct_types_with_arithmetic() {
        let a = CharOffset::new(10);
        assert_eq!((a + 5).get(), 15);
        assert_eq!((a - 3).get(), 7);
        assert_eq!(a - CharOffset::new(4), 6);
        assert_eq!(CharOffset::ZERO.saturating_sub(5), CharOffset::ZERO);
    }

    #[test]
    fn ordering_matches_the_underlying_count() {
        assert!(LineIndex::new(2) < LineIndex::new(10));
        assert_eq!(LineIndex::new(2).max_value(LineIndex::new(10)), LineIndex::new(10));
        assert_eq!(ByteOffset::new(2).min_value(ByteOffset::new(10)), ByteOffset::new(2));
    }

    #[test]
    fn display_names_the_unit() {
        assert_eq!(ByteOffset::new(3).to_string(), "3b");
        assert_eq!(CharOffset::new(3).to_string(), "3c");
        assert_eq!(LineIndex::new(3).to_string(), "3L");
        assert_eq!(DisplayColumn::new(3).to_string(), "3col");
    }
}
