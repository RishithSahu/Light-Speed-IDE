//! Encoding and binary handling (specification section 19).
//!
//! ```text
//! bytes -> binary detection -> encoding detection -> decode -> TextBuffer
//! ```
//!
//! Supported: UTF-8, UTF-8 with BOM, UTF-16 LE/BE with BOM. Anything else fails
//! loudly rather than producing silently corrupted text, and binary files are
//! refused rather than being converted to text.

use crate::error::{BinaryReason, EncodingError};
use ls_buffer::LineEnding;
use std::io::Write;

/// Bytes inspected when deciding whether a file is binary.
pub const BINARY_SNIFF_BYTES: usize = 8 * 1024;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Encoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    pub const fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf8Bom => "UTF-8 BOM",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
        }
    }

    /// Byte order mark this encoding writes, if any.
    pub const fn bom(self) -> &'static [u8] {
        match self {
            Encoding::Utf8 => &[],
            Encoding::Utf8Bom => &[0xEF, 0xBB, 0xBF],
            Encoding::Utf16Le => &[0xFF, 0xFE],
            Encoding::Utf16Be => &[0xFE, 0xFF],
        }
    }
}

/// Detects the encoding from a byte order mark, defaulting to UTF-8.
pub fn detect_encoding(bytes: &[u8]) -> Encoding {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Encoding::Utf8Bom
    } else if bytes.starts_with(&[0xFF, 0xFE]) {
        Encoding::Utf16Le
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        Encoding::Utf16Be
    } else {
        Encoding::Utf8
    }
}

/// Classifies bytes as binary. A NUL byte in the inspected prefix is the
/// signal: it is the one thing that essentially never appears in source text
/// and always appears in object files, images and archives.
///
/// UTF-16 text is full of NUL bytes, so a byte order mark wins over the scan.
pub fn detect_binary(bytes: &[u8]) -> Option<BinaryReason> {
    let encoding = detect_encoding(bytes);
    if matches!(encoding, Encoding::Utf16Le | Encoding::Utf16Be) {
        return None;
    }
    let prefix = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    prefix.iter().position(|&b| b == 0).map(|offset| BinaryReason::NulByte { offset })
}

/// Decoded text plus what it was decoded from.
#[derive(Debug)]
pub struct Decoded {
    pub text: String,
    pub encoding: Encoding,
}

/// Decodes a whole file. The returned text still has its original line endings;
/// normalization is a separate, explicit step.
pub fn decode(bytes: &[u8]) -> Result<Decoded, EncodingError> {
    let encoding = detect_encoding(bytes);
    let body = &bytes[encoding.bom().len()..];
    let text = match encoding {
        Encoding::Utf8 | Encoding::Utf8Bom => String::from_utf8(body.to_vec()).map_err(|err| {
            EncodingError::Invalid { encoding: "UTF-8", offset: err.utf8_error().valid_up_to() }
        })?,
        Encoding::Utf16Le | Encoding::Utf16Be => decode_utf16(body, encoding)?,
    };
    Ok(Decoded { text, encoding })
}

fn decode_utf16(body: &[u8], encoding: Encoding) -> Result<String, EncodingError> {
    if body.len() % 2 != 0 {
        return Err(EncodingError::TruncatedUtf16);
    }
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|pair| match encoding {
            Encoding::Utf16Be => u16::from_be_bytes([pair[0], pair[1]]),
            _ => u16::from_le_bytes([pair[0], pair[1]]),
        })
        .collect();
    String::from_utf16(&units)
        .map_err(|_| EncodingError::Invalid { encoding: encoding.label(), offset: 0 })
}

/// Writes a document: byte order mark, then each chunk with its line endings
/// restored and re-encoded.
///
/// Chunks are processed one at a time so a large document is never copied into
/// a second full-size buffer just to be saved.
pub fn encode_to<'a, W, I>(
    writer: &mut W,
    chunks: I,
    encoding: Encoding,
    line_ending: LineEnding,
) -> std::io::Result<()>
where
    W: Write,
    I: IntoIterator<Item = &'a str>,
{
    writer.write_all(encoding.bom())?;
    for chunk in chunks {
        let restored = ls_buffer::line_ending::denormalize(chunk, line_ending);
        match encoding {
            Encoding::Utf8 | Encoding::Utf8Bom => writer.write_all(restored.as_bytes())?,
            Encoding::Utf16Le | Encoding::Utf16Be => {
                let mut bytes = Vec::with_capacity(restored.len() * 2);
                for unit in restored.encode_utf16() {
                    match encoding {
                        Encoding::Utf16Be => bytes.extend_from_slice(&unit.to_be_bytes()),
                        _ => bytes.extend_from_slice(&unit.to_le_bytes()),
                    }
                }
                writer.write_all(&bytes)?;
            }
        }
    }
    Ok(())
}

/// Convenience wrapper around [`encode_to`] for tests and small documents.
pub fn encode_to_vec(text: &str, encoding: Encoding, line_ending: LineEnding) -> Vec<u8> {
    let mut out = Vec::new();
    encode_to(&mut out, [text], encoding, line_ending).expect("writing to a Vec cannot fail");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_utf8_needs_no_bom() {
        let decoded = decode(b"fn main() {}").unwrap();
        assert_eq!(decoded.encoding, Encoding::Utf8);
        assert_eq!(decoded.text, "fn main() {}");
    }

    #[test]
    fn utf8_bom_is_detected_and_stripped() {
        let bytes = [&[0xEF, 0xBB, 0xBF][..], "hello".as_bytes()].concat();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.encoding, Encoding::Utf8Bom);
        assert_eq!(decoded.text, "hello");
    }

    #[test]
    fn utf16_little_endian_round_trips() {
        let original = "hi \u{1F600}\nsecond line";
        let bytes = encode_to_vec(original, Encoding::Utf16Le, LineEnding::Lf);
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.encoding, Encoding::Utf16Le);
        assert_eq!(decoded.text, original);
    }

    #[test]
    fn utf16_big_endian_round_trips() {
        let original = "caf\u{e9}";
        let bytes = encode_to_vec(original, Encoding::Utf16Be, LineEnding::Lf);
        assert_eq!(&bytes[..2], &[0xFE, 0xFF]);
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.encoding, Encoding::Utf16Be);
        assert_eq!(decoded.text, original);
    }

    #[test]
    fn invalid_utf8_is_reported_not_replaced() {
        let error = decode(&[0x66, 0x6F, 0xFF, 0x6F]).unwrap_err();
        match error {
            EncodingError::Invalid { encoding, offset } => {
                assert_eq!(encoding, "UTF-8");
                assert_eq!(offset, 2);
            }
            other => panic!("expected an invalid-UTF-8 error, got {other:?}"),
        }
    }

    #[test]
    fn odd_length_utf16_is_rejected() {
        let bytes = [0xFF, 0xFE, 0x41];
        assert!(matches!(decode(&bytes), Err(EncodingError::TruncatedUtf16)));
    }

    #[test]
    fn nul_bytes_mark_a_file_as_binary() {
        let bytes = b"MZ\x90\x00\x03\x00\x00\x00";
        assert_eq!(detect_binary(bytes), Some(BinaryReason::NulByte { offset: 3 }));
    }

    #[test]
    fn text_is_not_binary() {
        assert_eq!(detect_binary("fn main() {}\n".as_bytes()), None);
        assert_eq!(detect_binary("\u{1F600} unicode text".as_bytes()), None);
        assert_eq!(detect_binary(b""), None);
    }

    #[test]
    fn utf16_text_is_not_mistaken_for_binary() {
        let bytes = encode_to_vec("plain text", Encoding::Utf16Le, LineEnding::Lf);
        assert!(bytes.contains(&0), "UTF-16 text does contain NUL bytes");
        assert_eq!(detect_binary(&bytes), None);
    }

    #[test]
    fn a_nul_after_the_sniff_window_is_not_inspected() {
        let mut bytes = vec![b'a'; BINARY_SNIFF_BYTES + 10];
        bytes[BINARY_SNIFF_BYTES + 5] = 0;
        assert_eq!(detect_binary(&bytes), None);
    }

    #[test]
    fn encoding_restores_line_endings() {
        let bytes = encode_to_vec("a\nb\n", Encoding::Utf8, LineEnding::CrLf);
        assert_eq!(bytes, b"a\r\nb\r\n");
        let bytes = encode_to_vec("a\nb\n", Encoding::Utf8, LineEnding::Lf);
        assert_eq!(bytes, b"a\nb\n");
    }

    #[test]
    fn chunked_encoding_matches_whole_document_encoding() {
        let whole = encode_to_vec("one\ntwo\nthree\n", Encoding::Utf16Le, LineEnding::CrLf);
        let mut chunked = Vec::new();
        encode_to(&mut chunked, ["one\ntw", "o\nthre", "e\n"], Encoding::Utf16Le, LineEnding::CrLf)
            .unwrap();
        assert_eq!(whole, chunked);
    }
}
