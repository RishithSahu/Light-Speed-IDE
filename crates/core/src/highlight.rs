//! Syntax highlighting (item 8): a line-by-line tokenizer feeding the render
//! snapshot's existing `Decoration` / `DecorationKind::SyntaxToken` slot.
//!
//! **Scoped deliberately.** This is a lexer, not a parser: it recognizes
//! keywords, string literals, comments and numbers by scanning characters,
//! with no grammar and no symbol table -- it never highlights *meaning*, only
//! lexical categories any scanner can see without knowing the language's
//! grammar.
//!
//! A block comment spanning several lines is still handled correctly, via
//! exactly the incremental state the type system's own doc comments describe:
//!
//! ```text
//! line 1  Normal        -> BlockComment   (line opens a /* with no closing */)
//! line 2  BlockComment  -> BlockComment   (no closer on this line either)
//! line 3  BlockComment  -> Normal         (the */ is here)
//! ```
//!
//! [`LexState`] is the one value carried from a line's exit to the next
//! line's entry -- not a parser, not a symbol table, just "was a block
//! comment left open". [`crate::document::Document`] caches the state at the
//! end of every line it has already tokenized and truncates that cache from
//! the edited line forward on every edit (the same line-range invalidation
//! every other per-line render concern already uses), so re-tokenizing after
//! a keystroke costs the edited line onward, not the whole document, and nothing
//! here is recomputed unless a comment's extent could actually have changed.

use crate::language::Language;

/// Lexical state carried from the end of one line to the start of the next.
/// The entire "incremental" half of this module is this one value.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum LexState {
    #[default]
    Normal,
    BlockComment,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    String,
    Comment,
    Number,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub start_column_chars: usize,
    pub end_column_chars: usize,
    pub kind: TokenKind,
}

struct LanguageConfig {
    line_comment: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    keywords: &'static [&'static str],
}

fn config_for(language: Language) -> Option<LanguageConfig> {
    match language {
        Language::Rust => Some(LanguageConfig {
            line_comment: &["//"],
            block_comment: Some(("/*", "*/")),
            keywords: RUST_KEYWORDS,
        }),
        Language::C | Language::Cpp | Language::CSharp => Some(LanguageConfig {
            line_comment: &["//"],
            block_comment: Some(("/*", "*/")),
            keywords: C_FAMILY_KEYWORDS,
        }),
        Language::JavaScript | Language::TypeScript => Some(LanguageConfig {
            line_comment: &["//"],
            block_comment: Some(("/*", "*/")),
            keywords: JS_KEYWORDS,
        }),
        Language::Go => Some(LanguageConfig {
            line_comment: &["//"],
            block_comment: Some(("/*", "*/")),
            keywords: GO_KEYWORDS,
        }),
        Language::Python => Some(LanguageConfig {
            line_comment: &["#"],
            block_comment: None,
            keywords: PYTHON_KEYWORDS,
        }),
        Language::Shell => Some(LanguageConfig {
            line_comment: &["#"],
            block_comment: None,
            keywords: SHELL_KEYWORDS,
        }),
        Language::Toml | Language::Yaml => {
            Some(LanguageConfig { line_comment: &["#"], block_comment: None, keywords: &[] })
        }
        Language::Json | Language::Markdown | Language::PlainText => None,
    }
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];

/// Go's 25 keywords, plus the predeclared identifiers a reader scans for the
/// same way (`nil`, the boolean literals, `iota`). Go deliberately has no
/// `class`/`public`/`template`, so this is not the C-family list with
/// additions -- it is a shorter list of its own.
const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "false",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "iota",
    "map",
    "nil",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "true",
    "type",
    "var",
];

const C_FAMILY_KEYWORDS: &[&str] = &[
    "auto",
    "break",
    "case",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "double",
    "else",
    "enum",
    "explicit",
    "extern",
    "false",
    "float",
    "for",
    "friend",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "namespace",
    "new",
    "operator",
    "private",
    "protected",
    "public",
    "register",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "template",
    "this",
    "true",
    "typedef",
    "typename",
    "union",
    "unsigned",
    "using",
    "virtual",
    "void",
    "volatile",
    "while",
];

const JS_KEYWORDS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "yield",
];

const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield",
];

const SHELL_KEYWORDS: &[&str] = &[
    "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if", "in",
    "local", "return", "select", "then", "until", "while",
];

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Tokenizes one line. `chars` are counted, not bytes, matching every other
/// column in this codebase (specification's rule: columns are character
/// offsets, so multi-byte text never shifts a highlight).
pub fn tokenize_line(text: &str, language: Language, state: LexState) -> (Vec<Token>, LexState) {
    let Some(config) = config_for(language) else { return (Vec::new(), LexState::Normal) };
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0usize;

    if state == LexState::BlockComment {
        let (_, end) =
            config.block_comment.expect("BlockComment state implies the language has one");
        match find_marker(&chars, 0, end) {
            Some(found) => {
                let close = found + end.chars().count();
                tokens.push(Token {
                    start_column_chars: 0,
                    end_column_chars: close,
                    kind: TokenKind::Comment,
                });
                index = close;
            }
            None => {
                // Still inside it at the end of this line too.
                tokens.push(Token {
                    start_column_chars: 0,
                    end_column_chars: chars.len(),
                    kind: TokenKind::Comment,
                });
                return (tokens, LexState::BlockComment);
            }
        }
    }

    while index < chars.len() {
        let c = chars[index];

        if let Some((start_marker, end_marker)) = config.block_comment {
            if starts_with_at(&chars, index, start_marker) {
                let start = index;
                match find_marker(&chars, index + start_marker.chars().count(), end_marker) {
                    Some(found) => {
                        let close = found + end_marker.chars().count();
                        tokens.push(Token {
                            start_column_chars: start,
                            end_column_chars: close,
                            kind: TokenKind::Comment,
                        });
                        index = close;
                        continue;
                    }
                    None => {
                        tokens.push(Token {
                            start_column_chars: start,
                            end_column_chars: chars.len(),
                            kind: TokenKind::Comment,
                        });
                        return (tokens, LexState::BlockComment);
                    }
                }
            }
        }

        if config.line_comment.iter().any(|marker| starts_with_at(&chars, index, marker)) {
            tokens.push(Token {
                start_column_chars: index,
                end_column_chars: chars.len(),
                kind: TokenKind::Comment,
            });
            break;
        }

        if c == '"' || c == '\'' {
            let start = index;
            let quote = c;
            index += 1;
            while index < chars.len() {
                if chars[index] == '\\' && index + 1 < chars.len() {
                    index += 2;
                    continue;
                }
                if chars[index] == quote {
                    index += 1;
                    break;
                }
                index += 1;
            }
            tokens.push(Token {
                start_column_chars: start,
                end_column_chars: index,
                kind: TokenKind::String,
            });
            continue;
        }

        if c.is_ascii_digit() {
            let start = index;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || chars[index] == '.')
            {
                index += 1;
            }
            tokens.push(Token {
                start_column_chars: start,
                end_column_chars: index,
                kind: TokenKind::Number,
            });
            continue;
        }

        if c.is_alphabetic() || c == '_' {
            let start = index;
            while index < chars.len() && is_ident_char(chars[index]) {
                index += 1;
            }
            let word: String = chars[start..index].iter().collect();
            if config.keywords.contains(&word.as_str()) {
                tokens.push(Token {
                    start_column_chars: start,
                    end_column_chars: index,
                    kind: TokenKind::Keyword,
                });
            }
            continue;
        }

        index += 1;
    }

    (tokens, LexState::Normal)
}

/// The character index of `marker` at or after `from`, if it occurs.
///
/// The subtraction below has to be checked, not saturating. On a line shorter
/// than the marker, `saturating_sub` bottoms out at 0 and produces the range
/// `0..=0` -- one iteration, which then slices `chars[0..2]` out of a line
/// with fewer than two characters and panics. That is not a theoretical
/// edge: `tokenize_line` looks for the block-comment terminator from column 0
/// of every line while inside one, so *any blank line inside a `/* ... */`
/// comment* crashed the highlighter, in every C-family language. Found by the
/// corpus tests in `tests/tests/languages.rs` on the first real repository
/// they were pointed at.
fn find_marker(chars: &[char], from: usize, marker: &str) -> Option<usize> {
    let marker_chars: Vec<char> = marker.chars().collect();
    if marker_chars.is_empty() {
        return None;
    }
    // `None` when the line is shorter than the marker: it cannot occur, and
    // there is no valid start index to scan from.
    let last_start = chars.len().checked_sub(marker_chars.len())?;
    if from > last_start {
        return None;
    }
    (from..=last_start).find(|&index| chars[index..index + marker_chars.len()] == marker_chars[..])
}

fn starts_with_at(chars: &[char], index: usize, marker: &str) -> bool {
    let marker_chars: Vec<char> = marker.chars().collect();
    if index + marker_chars.len() > chars.len() {
        return false;
    }
    chars[index..index + marker_chars.len()] == marker_chars[..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str, language: Language) -> Vec<(usize, usize, TokenKind)> {
        tokenize_line(text, language, LexState::Normal)
            .0
            .into_iter()
            .map(|t| (t.start_column_chars, t.end_column_chars, t.kind))
            .collect()
    }

    #[test]
    fn a_blank_line_inside_a_block_comment_does_not_panic() {
        // Regression test for a crash found by the language corpus tests on
        // real repositories: continuing a `/* ... */` comment onto a line
        // shorter than the two-character terminator sliced past the end of
        // that line. A blank line inside a block comment is ordinary in every
        // C-family language, so this crashed on commonplace source.
        for language in [
            Language::Rust,
            Language::C,
            Language::Cpp,
            Language::CSharp,
            Language::Go,
            Language::JavaScript,
            Language::TypeScript,
        ] {
            for line in ["", "*", "x", " "] {
                let (tokens, state) = tokenize_line(line, language, LexState::BlockComment);
                assert_eq!(
                    state,
                    LexState::BlockComment,
                    "{}: {line:?} does not close the comment, so it stays open",
                    language.name()
                );
                for token in tokens {
                    assert!(
                        token.end_column_chars <= line.chars().count(),
                        "{}: token escapes the line",
                        language.name()
                    );
                }
            }
        }
    }

    #[test]
    fn a_block_comment_still_closes_on_a_line_that_is_only_the_terminator() {
        // The fix must not overshoot: a line exactly as long as the marker is
        // the boundary case the checked subtraction now allows through.
        let (tokens, state) = tokenize_line("*/", Language::Rust, LexState::BlockComment);
        assert_eq!(state, LexState::Normal, "the comment ends here");
        assert_eq!(tokens[0].end_column_chars, 2);
    }

    #[test]
    fn a_multi_line_block_comment_with_a_gap_in_it_ends_where_it_should() {
        // The whole sequence, threaded the way the renderer threads it.
        let source = ["/* opening", "", "still inside", "*/ after"];
        let mut state = LexState::Normal;
        let mut states = Vec::new();
        for line in source {
            let (_, next) = tokenize_line(line, Language::C, state);
            state = next;
            states.push(state);
        }
        assert_eq!(
            states,
            vec![
                LexState::BlockComment,
                LexState::BlockComment,
                LexState::BlockComment,
                LexState::Normal
            ]
        );
    }

    #[test]
    fn plain_text_has_no_tokens() {
        assert!(tokenize_line("fn main() {}", Language::PlainText, LexState::Normal).0.is_empty());
    }

    #[test]
    fn rust_keywords_are_found() {
        let tokens = kinds("fn main() { let x = 1; }", Language::Rust);
        assert!(tokens.contains(&(0, 2, TokenKind::Keyword)), "fn");
        assert!(tokens.contains(&(12, 15, TokenKind::Keyword)), "let");
    }

    #[test]
    fn identifiers_that_are_not_keywords_are_left_alone() {
        let tokens = kinds("let function_name = 1;", Language::Rust);
        assert!(
            !tokens.iter().any(|(s, e, _)| *s == 4 && *e == 17),
            "function_name is not a keyword"
        );
    }

    #[test]
    fn a_line_comment_consumes_the_rest_of_the_line() {
        let tokens = kinds("let x = 1; // set x", Language::Rust);
        let comment = tokens.iter().find(|(_, _, kind)| *kind == TokenKind::Comment).unwrap();
        assert_eq!(comment.0, 11);
        assert_eq!(comment.1, "let x = 1; // set x".chars().count());
    }

    #[test]
    fn a_string_literal_is_one_token_including_its_quotes() {
        let tokens = kinds(r#"let s = "hello world";"#, Language::Rust);
        let string = tokens.iter().find(|(_, _, kind)| *kind == TokenKind::String).unwrap();
        assert_eq!(string.0, 8);
        assert_eq!(string.1, 21);
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string_early() {
        let tokens = kinds(r#""a\"b""#, Language::Rust);
        let string = tokens.iter().find(|(_, _, kind)| *kind == TokenKind::String).unwrap();
        assert_eq!(string.1, 6, "the string runs to the real closing quote");
    }

    #[test]
    fn numbers_are_tokenized() {
        let tokens = kinds("x = 42 + 3.14;", Language::Rust);
        assert!(tokens.iter().any(|(_, _, kind)| *kind == TokenKind::Number));
    }

    #[test]
    fn python_uses_hash_comments_and_its_own_keywords() {
        let tokens = kinds("def f(): # comment", Language::Python);
        assert!(tokens.contains(&(0, 3, TokenKind::Keyword)));
        assert!(tokens.iter().any(|(_, _, kind)| *kind == TokenKind::Comment));
    }

    #[test]
    fn multibyte_characters_do_not_shift_later_columns() {
        // "café " is 5 characters (é is one), 6 bytes. A byte-based scanner
        // would put the keyword one column too late.
        let tokens = kinds("caf\u{e9} let x = 1;", Language::Rust);
        assert!(tokens
            .iter()
            .any(|(s, e, kind)| *kind == TokenKind::Keyword && *s == 5 && *e == 8));
    }

    // --- block comments spanning multiple lines --------------------------------

    #[test]
    fn a_block_comment_that_opens_and_closes_on_one_line_does_not_change_state() {
        let (tokens, exit) =
            tokenize_line("let x = 1; /* fine */ let y = 2;", Language::Rust, LexState::Normal);
        assert_eq!(exit, LexState::Normal);
        assert!(tokens.iter().any(|t| t.kind == TokenKind::Comment));
        // The keyword after the closed comment is still recognized: the
        // comment did not swallow the rest of the line the way a line
        // comment correctly does.
        assert!(tokens.iter().any(|t| t.kind == TokenKind::Keyword && t.start_column_chars >= 22));
    }

    #[test]
    fn an_unterminated_block_comment_carries_state_to_the_next_line() {
        let (tokens, exit) = tokenize_line("/* starts here", Language::Rust, LexState::Normal);
        assert_eq!(exit, LexState::BlockComment);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, TokenKind::Comment);
        assert_eq!(tokens[0].end_column_chars, "/* starts here".chars().count());
    }

    #[test]
    fn a_line_entirely_inside_a_carried_over_comment_stays_a_comment_and_state_persists() {
        let (tokens, exit) =
            tokenize_line("and this continues", Language::Rust, LexState::BlockComment);
        assert_eq!(exit, LexState::BlockComment);
        assert_eq!(
            tokens,
            vec![Token { start_column_chars: 0, end_column_chars: 18, kind: TokenKind::Comment }]
        );
    }

    #[test]
    fn a_comment_that_closes_returns_to_normal_and_resumes_tokenizing() {
        let (tokens, exit) =
            tokenize_line("*/ let x = 10;", Language::Rust, LexState::BlockComment);
        assert_eq!(exit, LexState::Normal);
        let comment = tokens.iter().find(|t| t.kind == TokenKind::Comment).unwrap();
        assert_eq!((comment.start_column_chars, comment.end_column_chars), (0, 2));
        assert!(
            tokens.iter().any(|t| t.kind == TokenKind::Keyword),
            "let after the closer is tokenized"
        );
    }

    #[test]
    fn the_exact_three_line_example_from_the_module_docs_round_trips() {
        let lines = ["/*", "    this is a comment", "    and this continues", "*/", "let x = 10;"];
        let mut state = LexState::Normal;
        let mut states = Vec::new();
        for line in lines {
            let (_, exit) = tokenize_line(line, Language::Rust, state);
            state = exit;
            states.push(state);
        }
        assert_eq!(
            states,
            vec![
                LexState::BlockComment,
                LexState::BlockComment,
                LexState::BlockComment,
                LexState::Normal,
                LexState::Normal,
            ]
        );
    }

    #[test]
    fn languages_without_block_comments_never_enter_the_state() {
        let (_, exit) =
            tokenize_line("x = 1 # not a block comment opener", Language::Python, LexState::Normal);
        assert_eq!(exit, LexState::Normal);
    }
}
