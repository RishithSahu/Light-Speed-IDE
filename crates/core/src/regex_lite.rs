//! A small, hand-rolled regular expression matcher for workspace search's
//! Regex toggle.
//!
//! # Why hand-written
//!
//! This workspace takes no regex crate: it is a heavyweight dependency (a
//! real engine is thousands of lines, often with its own bytecode compiler)
//! for what search actually needs, which is "does this line contain
//! something matching this pattern" over source-code-sized lines, not
//! general-purpose text processing at scale. A compact backtracking matcher
//! covers what people actually type into a code search box -- `TODO\d+`,
//! `fn \w+\(`, `import .*from` -- in a few hundred lines with no build
//! script and no surface area beyond what is used.
//!
//! # What is supported, and what deliberately is not
//!
//! Literals, `.`, the quantifiers `*` `+` `?`, character classes `[abc]`
//! and `[^abc]` with ranges (`[a-z0-9]`), the shorthand classes `\d` `\w`
//! `\s` and their negations, and the anchors `^` `$`. That covers the
//! overwhelming majority of patterns people write for a code search.
//!
//! Not supported: capture groups, alternation (`|`), backreferences, and
//! bounded repetition (`{n,m}`). Adding groups and alternation properly
//! wants a real compiled representation (an NFA) rather than the direct
//! tree-walking backtracker below, and that is a much bigger piece of code
//! to keep correct for a feature that is one toggle among several. A
//! pattern using an unsupported construct is a compile error
//! ([`RegexError`]), not a silent wrong answer.
//!
//! # Why backtracking rather than compiling to an NFA
//!
//! An NFA (Thompson's construction) guarantees linear-time matching and is
//! the right choice for a general-purpose engine. It is also considerably
//! more code, and the inputs here are source-code lines: at most a few
//! hundred characters, scanned once per line already read into memory for
//! the plain-substring path. Backtracking's worst case (catastrophic
//! exponential blowup on a pattern like `(a*)*b` against a non-matching
//! string) is real, but this engine has no groups to nest quantifiers
//! inside in the first place, which is what that failure mode needs -- a
//! bare `a*` against a long line of `a`s is linear, not exponential,
//! because there is nothing for the backtracking to nest into.

/// Why a pattern could not be compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegexError {
    pub message: String,
}

impl std::fmt::Display for RegexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn error(message: impl Into<String>) -> RegexError {
    RegexError { message: message.into() }
}

/// One element of a pattern, already parsed.
#[derive(Clone, Debug, PartialEq)]
enum Node {
    /// A literal character, matched exactly (case folding is handled by the
    /// caller lowercasing both pattern and text before compiling, the same
    /// way the plain-substring path already does).
    Char(char),
    /// `.` -- any single character.
    Any,
    /// `[...]` / `[^...]`: a set of characters or ranges, and whether it is
    /// negated.
    Class { ranges: Vec<(char, char)>, negated: bool },
    /// `^` -- only valid as the first element.
    Start,
    /// `$` -- only valid as the last element.
    End,
}

/// One parsed pattern: a flat sequence of [`Node`]s, each with its own
/// quantifier. Flat rather than a tree because there is nothing to nest --
/// no groups -- so a `Vec` is the whole of what compiling produces.
#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    items: Vec<(Node, Quantifier)>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Quantifier {
    One,
    /// `*`
    ZeroOrMore,
    /// `+`
    OneOrMore,
    /// `?`
    ZeroOrOne,
}

/// Compiles `source` into a matchable [`Pattern`].
pub fn compile(source: &str) -> Result<Pattern, RegexError> {
    let chars: Vec<char> = source.chars().collect();
    let mut items = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let node = match chars[at] {
            '^' if at == 0 => {
                at += 1;
                items.push((Node::Start, Quantifier::One));
                continue;
            }
            '$' if at == chars.len() - 1 => {
                at += 1;
                items.push((Node::End, Quantifier::One));
                continue;
            }
            '^' | '$' => {
                return Err(error("^ and $ are only supported at the very start or end of a pattern"));
            }
            '.' => {
                at += 1;
                Node::Any
            }
            '\\' => {
                at += 1;
                let Some(escaped) = chars.get(at) else {
                    return Err(error("a pattern cannot end with a bare backslash"));
                };
                at += 1;
                shorthand_class(*escaped)?
            }
            '[' => {
                let (node, consumed) = parse_class(&chars[at..])?;
                at += consumed;
                node
            }
            '(' | ')' | '|' | '{' | '}' => {
                return Err(error(format!(
                    "'{}' is not supported -- groups, alternation and {{n,m}} repetition are not part of this pattern language",
                    chars[at]
                )));
            }
            '*' | '+' | '?' => {
                return Err(error(format!("'{}' with nothing before it to repeat", chars[at])));
            }
            other => {
                at += 1;
                Node::Char(other)
            }
        };

        let quantifier = match chars.get(at) {
            Some('*') => {
                at += 1;
                Quantifier::ZeroOrMore
            }
            Some('+') => {
                at += 1;
                Quantifier::OneOrMore
            }
            Some('?') => {
                at += 1;
                Quantifier::ZeroOrOne
            }
            _ => Quantifier::One,
        };
        if quantifier != Quantifier::One && matches!(node, Node::Start | Node::End) {
            return Err(error("^ and $ cannot be quantified"));
        }
        items.push((node, quantifier));
    }
    Ok(Pattern { items })
}

fn shorthand_class(letter: char) -> Result<Node, RegexError> {
    match letter {
        'd' => Ok(Node::Class { ranges: vec![('0', '9')], negated: false }),
        'D' => Ok(Node::Class { ranges: vec![('0', '9')], negated: true }),
        'w' => Ok(Node::Class {
            ranges: vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
            negated: false,
        }),
        'W' => Ok(Node::Class {
            ranges: vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
            negated: true,
        }),
        's' => Ok(Node::Class { ranges: vec![(' ', ' '), ('\t', '\t')], negated: false }),
        'S' => Ok(Node::Class { ranges: vec![(' ', ' '), ('\t', '\t')], negated: true }),
        '.' | '\\' | '*' | '+' | '?' | '[' | ']' | '^' | '$' | '(' | ')' | '|' | '{' | '}' => {
            Ok(Node::Char(letter))
        }
        other => Err(error(format!("\\{other} is not a recognised escape"))),
    }
}

/// Parses a `[...]` class starting at `chars[0]`, returning the node and how
/// many characters (including both brackets) it consumed.
fn parse_class(chars: &[char]) -> Result<(Node, usize), RegexError> {
    debug_assert_eq!(chars.first(), Some(&'['));
    let mut at = 1;
    let negated = chars.get(at) == Some(&'^');
    if negated {
        at += 1;
    }
    let mut ranges = Vec::new();
    let start = at;
    while chars.get(at) != Some(&']') {
        let Some(&low) = chars.get(at) else {
            return Err(error("unterminated character class: missing ]"));
        };
        at += 1;
        if chars.get(at) == Some(&'-') && chars.get(at + 1).is_some_and(|c| *c != ']') {
            let high = chars[at + 1];
            at += 2;
            if high < low {
                return Err(error(format!("a character range must run low to high: [{low}-{high}]")));
            }
            ranges.push((low, high));
        } else {
            ranges.push((low, low));
        }
    }
    if ranges.is_empty() && at == start {
        return Err(error("an empty character class matches nothing"));
    }
    at += 1; // the closing ]
    Ok((Node::Class { ranges, negated }, at))
}

impl Pattern {
    /// The first match in `text`, as a byte range, or `None`.
    ///
    /// Tries an anchor at every starting position in turn -- the standard
    /// "unanchored search is anchored search tried at each offset"
    /// reduction, which is why `^` only makes sense as the pattern's first
    /// element: it rejects every starting position but the real one, same
    /// as any other engine's.
    pub fn find(&self, text: &str) -> Option<(usize, usize)> {
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        for start in 0..=chars.len() {
            if let Some(end) = self.match_from(&chars, start) {
                let start_byte = chars.get(start).map(|(byte, _)| *byte).unwrap_or(text.len());
                let end_byte = chars.get(end).map(|(byte, _)| *byte).unwrap_or(text.len());
                return Some((start_byte, end_byte));
            }
        }
        None
    }

    pub fn is_match(&self, text: &str) -> bool {
        self.find(text).is_some()
    }

    /// Tries to match the whole pattern starting exactly at `chars[start]`,
    /// returning the character index just past the match.
    fn match_from(&self, chars: &[(usize, char)], start: usize) -> Option<usize> {
        self.match_items(&self.items, chars, start)
    }

    fn match_items(&self, items: &[(Node, Quantifier)], chars: &[(usize, char)], at: usize) -> Option<usize> {
        let Some((node, quantifier)) = items.first() else { return Some(at) };
        let rest = &items[1..];
        match quantifier {
            Quantifier::One => {
                let next = match_one(node, chars, at)?;
                self.match_items(rest, chars, next)
            }
            Quantifier::ZeroOrOne => {
                if let Some(next) = match_one(node, chars, at) {
                    if let Some(done) = self.match_items(rest, chars, next) {
                        return Some(done);
                    }
                }
                self.match_items(rest, chars, at)
            }
            Quantifier::ZeroOrMore | Quantifier::OneOrMore => {
                // Greedy: consume as many as possible first, then backtrack
                // one at a time until the rest of the pattern also matches.
                // No group means nothing this recurses into can itself
                // backtrack into more of the same repeat, which is what
                // keeps this linear instead of exponential.
                let mut ends = vec![at];
                let mut cursor = at;
                while let Some(next) = match_one(node, chars, cursor) {
                    if next == cursor {
                        break; // a zero-width match would loop forever
                    }
                    cursor = next;
                    ends.push(cursor);
                }
                // `ends[i]` is the position after matching `i` repetitions;
                // try the greediest count first, backtracking down to the
                // fewest this quantifier allows (1 for `+`, 0 for `*`).
                let floor = if *quantifier == Quantifier::OneOrMore { 1 } else { 0 };
                for &end in ends[floor..].iter().rev() {
                    if let Some(done) = self.match_items(rest, chars, end) {
                        return Some(done);
                    }
                }
                None
            }
        }
    }
}

/// Matches a single (non-quantified) node at `at`, returning the index
/// after it -- for `Start`/`End` that is `at` itself (zero-width).
fn match_one(node: &Node, chars: &[(usize, char)], at: usize) -> Option<usize> {
    match node {
        Node::Start => (at == 0).then_some(at),
        Node::End => (at == chars.len()).then_some(at),
        Node::Any => (at < chars.len()).then_some(at + 1),
        Node::Char(want) => {
            let (_, got) = chars.get(at)?;
            (*got == *want).then_some(at + 1)
        }
        Node::Class { ranges, negated } => {
            let (_, got) = chars.get(at)?;
            let inside = ranges.iter().any(|(low, high)| *low <= *got && *got <= *high);
            (inside != *negated).then_some(at + 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(pattern: &str, text: &str) -> Option<(usize, usize)> {
        compile(pattern).expect("compiles").find(text)
    }

    #[test]
    fn a_literal_matches_itself_and_nothing_else() {
        assert_eq!(find("cat", "the cat sat"), Some((4, 7)));
        assert_eq!(find("dog", "the cat sat"), None);
    }

    #[test]
    fn dot_matches_any_single_character() {
        assert_eq!(find("c.t", "cat"), Some((0, 3)));
        assert_eq!(find("c.t", "ct"), None, "dot still needs a character to match");
    }

    #[test]
    fn star_matches_zero_or_more_greedily_but_still_finds_the_rest_of_the_pattern() {
        assert_eq!(find("ab*c", "ac"), Some((0, 2)));
        assert_eq!(find("ab*c", "abbbc"), Some((0, 5)));
        // Greedy consumes every digit first, then backtracks one at a time
        // until "9x" is found.
        assert_eq!(find(r"\d*9x", "1234599999x"), Some((0, 11)));
    }

    #[test]
    fn plus_requires_at_least_one() {
        assert_eq!(find("ab+c", "ac"), None);
        assert_eq!(find("ab+c", "abc"), Some((0, 3)));
    }

    #[test]
    fn question_mark_makes_the_element_optional() {
        assert_eq!(find("colou?r", "color"), Some((0, 5)));
        assert_eq!(find("colou?r", "colour"), Some((0, 6)));
    }

    #[test]
    fn a_character_class_matches_any_member_and_a_negated_one_matches_the_complement() {
        assert_eq!(find("[abc]", "xbz"), Some((1, 2)));
        assert_eq!(find("[^abc]", "abx"), Some((2, 3)));
    }

    #[test]
    fn a_class_range_covers_every_character_between_its_ends() {
        let digits = compile("[0-9]+").unwrap();
        assert_eq!(digits.find("id42"), Some((2, 4)));
        assert!(compile("[0-9]+").unwrap().is_match("7"));
    }

    #[test]
    fn shorthand_classes_match_what_they_promise() {
        assert_eq!(find(r"\d+", "room 204"), Some((5, 8)));
        assert_eq!(find(r"\w+", "  hello_world!"), Some((2, 13)));
        assert_eq!(find(r"\s", "a b"), Some((1, 2)));
    }

    #[test]
    fn anchors_pin_the_match_to_one_end_of_the_text() {
        assert_eq!(find("^fn", "fn main"), Some((0, 2)));
        assert_eq!(find("^fn", "  fn main"), None);
        assert_eq!(find("end$", "the end"), Some((4, 7)));
        assert_eq!(find("end$", "the end of it"), None);
    }

    #[test]
    fn a_todo_with_a_number_is_the_pattern_this_exists_for() {
        assert_eq!(find(r"TODO\(#\d+\)", "// TODO(#42): fix this"), Some((3, 12)));
    }

    #[test]
    fn unsupported_syntax_is_a_compile_error_not_a_silent_wrong_answer() {
        assert!(compile("(a|b)").is_err(), "groups and alternation are not supported");
        assert!(compile("a{2,4}").is_err(), "bounded repetition is not supported");
        assert!(compile("*abc").is_err(), "a quantifier needs something before it");
        assert!(compile(r"\q").is_err(), "not a recognised escape");
        assert!(compile("[a-").is_err(), "unterminated class");
        assert!(compile("[]").is_err(), "an empty class matches nothing");
    }

    #[test]
    fn a_caret_or_dollar_in_the_middle_of_a_pattern_is_rejected_rather_than_ignored() {
        // Every real engine treats a mid-pattern ^/$ as a literal in some
        // dialects and an anchor in others; refusing it is honest about not
        // trying to guess which this pattern language means.
        assert!(compile("a^b").is_err());
        assert!(compile("a$b").is_err());
    }

    #[test]
    fn a_pattern_with_no_match_returns_none_rather_than_looping() {
        assert_eq!(find("xyz", "abc"), None);
        assert_eq!(find("a+", ""), None);
        assert_eq!(find("a*", "bbb"), Some((0, 0)), "zero-or-more still matches, at width zero");
    }

    #[test]
    fn empty_pattern_matches_at_the_start_of_anything() {
        assert_eq!(find("", "anything"), Some((0, 0)));
        assert_eq!(find("", ""), Some((0, 0)));
    }
}
