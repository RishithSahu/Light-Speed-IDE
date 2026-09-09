//! A Markdown source file, rendered into readable text.
//!
//! **Scoped deliberately, the same way `lsp.rs` and `terminal.rs` say so up
//! front.** This is not a browser-grade Markdown renderer: no variable font
//! sizes (this editor's whole text pipeline is one fixed line-height grid --
//! see `layout.rs`'s own doc comment -- and a heading twice the height of a
//! paragraph would need a different one), no images, no tables, no nested
//! blockquotes. What it does: strip the punctuation Markdown uses to mean
//! something (`**bold**`, `` `code` ``, `# heading`, `- list item`,
//! `[text](url)`) and recolor what is left, the same "read the source,
//! don't hide it" trade every other view in this editor makes for its own
//! content (the sidebar's file tree, the dependency graph's labels). Good
//! enough to read a README without the asterisks getting in the way; not a
//! reason to stop reading raw Markdown for anything more demanding.
//!
//! `view.toggle_markdown_preview` (Ctrl+Shift+V) swaps `Region::Editor` for
//! `Region::MarkdownPreview`, built here, the same way `dependency_view`
//! swaps it for `Region::DependencyGraph`.

use crate::text::RichText;
use crate::theme::Theme;

/// Renders `source` (a Markdown document's raw text) into a colored,
/// read-only `RichText` -- headings, bold/italic emphasis, inline and
/// fenced code, list bullets, and link text all recolored or reshaped;
/// everything else passes through untouched.
pub fn render(source: &str, theme: &Theme) -> RichText {
    let mut rich = RichText::new();
    let mut first = true;
    let mut in_fence = false;

    for line in source.lines() {
        if !first {
            rich.newline();
        }
        first = false;

        let trimmed_start = line.trim_start();
        if trimmed_start.starts_with("```") {
            in_fence = !in_fence;
            // The fence markers themselves are punctuation, not content --
            // a blank line in their place keeps every later line number in
            // the source lined up with a row in the preview, which matters
            // for `view.scroll_y` staying roughly where the reader left it
            // when they toggle back and forth.
            continue;
        }
        if in_fence {
            rich.colored(line, theme.syntax_string);
            continue;
        }

        if let Some(heading) = heading_text(trimmed_start) {
            rich.colored(heading, theme.syntax_keyword);
            continue;
        }

        let indent = line.len() - trimmed_start.len();
        if let Some(rest) = list_item_text(trimmed_start) {
            rich.plain(&line[..indent]);
            rich.colored("\u{2022} ", theme.dim_text);
            push_inline(&mut rich, rest, theme);
            continue;
        }

        push_inline(&mut rich, line, theme);
    }

    rich
}

/// `line` past its `#` markers and the space after them, if it is a heading
/// line at all (1-6 `#` characters, a space, then something).
fn heading_text(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    line[hashes..].strip_prefix(' ')
}

/// The text after a list marker (`-`, `*`, `+`, or `N.`/`N)`), if `line`
/// (already start-trimmed) is one.
fn list_item_text(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).or_else(|| line.strip_prefix("+ ")) {
        return Some(rest);
    }
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let rest = &line[digits..];
    rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))
}

/// Inline emphasis, code spans and links within one line -- `**bold**`,
/// `*italic*`/`_italic_`, `` `code` ``, `[text](url)` (the url is dropped;
/// this is a read-only preview with nothing to click through to yet).
///
/// A small hand-rolled scanner rather than a regular expression, the same
/// choice `regex_lite.rs` and `gitignore.rs` already made for this
/// codebase: Markdown's inline syntax is a handful of paired delimiters,
/// not a language worth a general pattern matcher for.
fn push_inline(rich: &mut RichText, line: &str, theme: &Theme) {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut plain_start = 0;

    macro_rules! flush_plain {
        ($end:expr) => {
            if $end > plain_start {
                let text: String = chars[plain_start..$end].iter().collect();
                rich.colored(&text, theme.text);
            }
        };
    }

    while i < chars.len() {
        // `**bold**` (checked before single `*` so it is not read as two
        // empty-italic runs).
        if chars[i] == '*' && chars.get(i + 1) == Some(&'*') {
            if let Some(end) = find_delim(&chars, i + 2, "**") {
                flush_plain!(i);
                let text: String = chars[i + 2..end].iter().collect();
                rich.colored(&text, theme.text);
                i = end + 2;
                plain_start = i;
                continue;
            }
        }
        // `*italic*` / `_italic_`.
        if chars[i] == '*' || chars[i] == '_' {
            let marker = chars[i];
            if let Some(end) = find_delim(&chars, i + 1, &marker.to_string()) {
                flush_plain!(i);
                let text: String = chars[i + 1..end].iter().collect();
                rich.colored(&text, theme.dim_text);
                i = end + 1;
                plain_start = i;
                continue;
            }
        }
        // `` `code` ``.
        if chars[i] == '`' {
            if let Some(end) = find_delim(&chars, i + 1, "`") {
                flush_plain!(i);
                let text: String = chars[i + 1..end].iter().collect();
                rich.colored(&text, theme.syntax_string);
                i = end + 1;
                plain_start = i;
                continue;
            }
        }
        // `[text](url)` -- only the bracketed text is kept.
        if chars[i] == '[' {
            if let Some(close) = find_delim(&chars, i + 1, "]") {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(paren_end) = find_delim(&chars, close + 2, ")") {
                        flush_plain!(i);
                        let text: String = chars[i + 1..close].iter().collect();
                        rich.colored(&text, theme.activity_icon_active);
                        i = paren_end + 1;
                        plain_start = i;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    flush_plain!(chars.len());
}

/// The index of `delim` (a short literal, 1-2 characters) starting at or
/// after `from`, or `None` if it never appears again on this line -- inline
/// Markdown spans never cross a line break.
fn find_delim(chars: &[char], from: usize, delim: &str) -> Option<usize> {
    let needle: Vec<char> = delim.chars().collect();
    let mut i = from;
    while i + needle.len() <= chars.len() {
        if chars[i..i + needle.len()] == needle[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::dark()
    }

    #[test]
    fn a_heading_loses_its_hashes_but_keeps_its_text() {
        let rich = render("# Title\n\nBody", &theme());
        assert_eq!(rich.text, "Title\n\nBody");
    }

    #[test]
    fn a_heading_up_to_level_six_is_recognised() {
        assert_eq!(heading_text("###### Deep"), Some("Deep"));
        assert_eq!(heading_text("####### Too deep"), None);
    }

    #[test]
    fn a_hash_with_no_space_after_it_is_not_a_heading() {
        // `#hashtag` in prose, not a heading -- CommonMark's own rule.
        assert_eq!(heading_text("#hashtag"), None);
    }

    #[test]
    fn bold_and_italic_markers_are_stripped_from_the_text() {
        let rich = render("**bold** and *italic* and _also italic_", &theme());
        assert_eq!(rich.text, "bold and italic and also italic");
    }

    #[test]
    fn inline_code_markers_are_stripped() {
        let rich = render("call `foo()` now", &theme());
        assert_eq!(rich.text, "call foo() now");
    }

    #[test]
    fn a_link_keeps_only_its_visible_text() {
        let rich = render("see [the docs](https://example.com/docs) for more", &theme());
        assert_eq!(rich.text, "see the docs for more");
    }

    #[test]
    fn list_markers_become_a_bullet() {
        let rich = render("- first\n* second\n1. third\n2) fourth", &theme());
        assert_eq!(rich.text, "\u{2022} first\n\u{2022} second\n\u{2022} third\n\u{2022} fourth");
    }

    #[test]
    fn an_unclosed_delimiter_is_left_as_plain_text_rather_than_eating_the_rest_of_the_line() {
        let rich = render("this *never closes", &theme());
        assert_eq!(rich.text, "this *never closes");
    }

    #[test]
    fn a_fenced_code_block_is_shown_without_its_fence_markers() {
        let rich = render("```rust\nfn main() {}\n```", &theme());
        assert_eq!(rich.text, "\nfn main() {}\n");
    }

    #[test]
    fn code_inside_a_fence_is_not_inline_parsed() {
        // `**not bold**` inside a fence is source code, not emphasis --
        // the whole point of a fence is "stop interpreting this".
        let rich = render("```\n**not bold**\n```", &theme());
        assert_eq!(rich.text, "\n**not bold**\n");
    }

    #[test]
    fn an_empty_document_renders_as_empty() {
        assert_eq!(render("", &theme()).text, "");
    }
}
