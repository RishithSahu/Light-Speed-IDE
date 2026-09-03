//! Line-ending semantics (specification section 18).
//!
//! Line endings are document metadata, not content. Text is normalized to `\n`
//! on the way in and written back in the document's own style on the way out,
//! so the buffer only ever contains one newline representation and a file's
//! style survives a round trip.

use std::borrow::Cow;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum LineEnding {
    Lf,
    CrLf,
    Cr,
}

impl LineEnding {
    /// The bytes this style writes for one line break.
    pub const fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
            LineEnding::Cr => "\r",
        }
    }

    /// Short label for the status bar.
    pub const fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
            LineEnding::Cr => "CR",
        }
    }

    /// Style used for a new document on this platform.
    pub const fn platform_default() -> Self {
        if cfg!(windows) {
            LineEnding::CrLf
        } else {
            LineEnding::Lf
        }
    }
}

/// What a scan of the raw text found.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LineEndingAnalysis {
    /// Style to preserve on save: the majority style, or the platform default
    /// for a document with no line breaks at all.
    pub dominant: LineEnding,
    /// True when more than one style is present (specification section 18
    /// requires this to be detected and reported, not silently rewritten).
    pub mixed: bool,
    pub lf: usize,
    pub crlf: usize,
    pub cr: usize,
}

impl LineEndingAnalysis {
    pub fn total(&self) -> usize {
        self.lf + self.crlf + self.cr
    }
}

/// Counts line-ending styles in raw (un-normalized) text.
pub fn detect(text: &str) -> LineEndingAnalysis {
    let bytes = text.as_bytes();
    let (mut lf, mut crlf, mut cr) = (0usize, 0usize, 0usize);
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                if bytes.get(index + 1) == Some(&b'\n') {
                    crlf += 1;
                    index += 2;
                    continue;
                }
                cr += 1;
            }
            b'\n' => lf += 1,
            _ => {}
        }
        index += 1;
    }

    let styles_present = (lf > 0) as u8 + (crlf > 0) as u8 + (cr > 0) as u8;
    let dominant = if crlf >= lf && crlf >= cr && crlf > 0 {
        LineEnding::CrLf
    } else if lf >= cr && lf > 0 {
        LineEnding::Lf
    } else if cr > 0 {
        LineEnding::Cr
    } else {
        LineEnding::platform_default()
    };

    LineEndingAnalysis { dominant, mixed: styles_present > 1, lf, crlf, cr }
}

/// Converts every style to `\n`. Borrows when the text is already normalized.
pub fn normalize(text: &str) -> Cow<'_, str> {
    if !text.as_bytes().contains(&b'\r') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut copied = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' {
            out.push_str(&text[copied..index]);
            out.push('\n');
            index += if bytes.get(index + 1) == Some(&b'\n') { 2 } else { 1 };
            copied = index;
        } else {
            index += 1;
        }
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// Converts normalized text back to `ending`. Borrows for [`LineEnding::Lf`].
///
/// Safe to apply chunk by chunk: only `\n` is rewritten, and a chunk boundary
/// never splits a character.
pub fn denormalize(text: &str, ending: LineEnding) -> Cow<'_, str> {
    match ending {
        LineEnding::Lf => Cow::Borrowed(text),
        LineEnding::CrLf | LineEnding::Cr => {
            if !text.as_bytes().contains(&b'\n') {
                return Cow::Borrowed(text);
            }
            Cow::Owned(text.replace('\n', ending.as_str()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_each_style() {
        assert_eq!(detect("a\nb\nc").dominant, LineEnding::Lf);
        assert_eq!(detect("a\r\nb\r\nc").dominant, LineEnding::CrLf);
        assert_eq!(detect("a\rb\rc").dominant, LineEnding::Cr);
    }

    #[test]
    fn a_document_without_line_breaks_uses_the_platform_default() {
        let analysis = detect("no breaks here");
        assert_eq!(analysis.dominant, LineEnding::platform_default());
        assert!(!analysis.mixed);
        assert_eq!(analysis.total(), 0);
    }

    #[test]
    fn mixed_endings_are_reported_with_a_majority() {
        let analysis = detect("a\r\nb\r\nc\nd");
        assert!(analysis.mixed);
        assert_eq!(analysis.dominant, LineEnding::CrLf);
        assert_eq!(analysis.crlf, 2);
        assert_eq!(analysis.lf, 1);
        assert_eq!(analysis.cr, 0);
    }

    #[test]
    fn a_lone_carriage_return_is_not_counted_as_crlf() {
        let analysis = detect("a\rb\r\nc");
        assert_eq!(analysis.cr, 1);
        assert_eq!(analysis.crlf, 1);
        assert!(analysis.mixed);
    }

    #[test]
    fn normalize_rewrites_every_style() {
        assert_eq!(normalize("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert!(matches!(normalize("already\nnormalized"), Cow::Borrowed(_)));
    }

    #[test]
    fn denormalize_restores_the_original_style() {
        assert_eq!(denormalize("a\nb", LineEnding::CrLf), "a\r\nb");
        assert_eq!(denormalize("a\nb", LineEnding::Cr), "a\rb");
        assert!(matches!(denormalize("a\nb", LineEnding::Lf), Cow::Borrowed(_)));
    }

    #[test]
    fn round_trip_preserves_the_document() {
        for original in ["a\r\nb\r\n", "a\nb\n", "a\rb\r", ""] {
            let analysis = detect(original);
            let normalized = normalize(original);
            let restored = denormalize(&normalized, analysis.dominant);
            assert_eq!(restored, original, "round trip failed for {original:?}");
        }
    }

    #[test]
    fn denormalizing_chunk_by_chunk_matches_whole_text() {
        let text = "one\ntwo\nthree\n";
        let whole = denormalize(text, LineEnding::CrLf).into_owned();
        let mut assembled = String::new();
        for chunk in ["one\ntw", "o\nthr", "ee\n"] {
            assembled.push_str(&denormalize(chunk, LineEnding::CrLf));
        }
        assert_eq!(assembled, whole);
    }
}
