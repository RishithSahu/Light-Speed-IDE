//! `.gitignore`-aware file-tree filtering.
//!
//! The explorer used to show everything a directory actually contains,
//! `bin/`, `obj/`, `node_modules/`, build artifacts and all -- exactly what
//! `git status` would never mention, on the theory that a workspace's own
//! `.gitignore` already says what its author considers noise. This module
//! parses that file and answers one question, `is_ignored`, for whatever
//! walks the tree to filter with (`app/src/app.rs`'s `append_tree_level`).
//!
//! **Scope, stated plainly.** Real `git` merges rules from a `.gitignore` in
//! every ancestor directory down to the one being tested, plus
//! `.git/info/exclude` and the user's global excludes file. This reads only
//! the workspace root's own `.gitignore`, which is where the overwhelming
//! majority of real projects keep every rule that matters. A nested
//! `.gitignore` inside a subdirectory (uncommon outside monorepos) is not
//! consulted -- a real gap, not a silent one, and worth widening if it turns
//! out to matter in practice.
//!
//! `.git` itself is always filtered, independent of any pattern: it is
//! reliably present, reliably irrelevant to browse, and every other real
//! explorer (VS Code included) hides it as a built-in exclude rather than
//! trusting `.gitignore` to say so (most projects' `.gitignore` never
//! mentions it -- `git` already knows not to track itself).

use std::path::Path;

/// One line of a `.gitignore`, parsed into the pieces matching needs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pattern {
    /// `true` for a `!`-prefixed line: a later match re-includes a path an
    /// earlier pattern excluded, the same override order `git` itself uses.
    negated: bool,
    /// `true` for a pattern ending in `/`: matches a directory only, never a
    /// file of the same name.
    directory_only: bool,
    /// `true` when the pattern contains a `/` before its final character
    /// (including a leading `/`): anchored to the root, rather than
    /// matching a basename at any depth.
    anchored: bool,
    /// The glob itself, `/`-stripped of the anchoring and directory-only
    /// markers that produced the two flags above.
    glob: String,
}

/// Every pattern read from a workspace's `.gitignore`, ready to test paths
/// against.
#[derive(Clone, Debug, Default)]
pub struct IgnoreSet {
    patterns: Vec<Pattern>,
}

impl IgnoreSet {
    /// Reads `<root>/.gitignore`. A workspace with none (or one that fails
    /// to read) simply ignores nothing beyond the built-in `.git` rule --
    /// the same "absence is not an error" treatment every other
    /// best-effort read in this codebase gives a missing file.
    pub fn load(root: &Path) -> IgnoreSet {
        let text = std::fs::read_to_string(root.join(".gitignore")).unwrap_or_default();
        IgnoreSet::parse(&text)
    }

    fn parse(text: &str) -> IgnoreSet {
        let patterns = text
            .lines()
            .filter_map(|line| {
                // A `#` can be escaped with a leading backslash to match a
                // literal one; that corner is rare enough to skip rather
                // than complicate every other line's parsing for it.
                let trimmed = line.trim_end();
                if trimmed.trim_start().is_empty() || trimmed.trim_start().starts_with('#') {
                    return None;
                }
                let (negated, rest) = match trimmed.strip_prefix('!') {
                    Some(rest) => (true, rest),
                    None => (false, trimmed),
                };
                let (directory_only, rest) = match rest.strip_suffix('/') {
                    Some(rest) => (true, rest),
                    None => (false, rest),
                };
                if rest.is_empty() {
                    return None;
                }
                let (anchored, glob) = match rest.strip_prefix('/') {
                    Some(rest) => (true, rest),
                    // A `/` anywhere but the end also anchors the pattern
                    // (git's own rule) -- only a bare basename like
                    // `*.log` or `build` matches at every depth.
                    None => (rest.contains('/'), rest),
                };
                Some(Pattern { negated, directory_only, anchored, glob: glob.to_string() })
            })
            .collect();
        IgnoreSet { patterns }
    }

    /// Whether `relative` (a path relative to the workspace root, forward
    /// slashes) should be hidden from the explorer. `is_dir` gates
    /// directory-only patterns (a trailing `/` in the source file).
    ///
    /// Patterns are tested in file order and the last match wins, exactly
    /// as `git check-ignore` resolves a path against multiple rules: a
    /// broad `*.log` followed by `!important.log` has to re-include that
    /// one file, which only works if a later line can override an earlier
    /// one rather than the first match deciding it.
    pub fn is_ignored(&self, relative: &Path, is_dir: bool) -> bool {
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative == ".git" || relative.starts_with(".git/") {
            return true;
        }
        let mut ignored = false;
        for pattern in &self.patterns {
            if pattern.directory_only && !is_dir {
                continue;
            }
            if pattern_matches(pattern, &relative) {
                ignored = !pattern.negated;
            }
        }
        ignored
    }
}

/// Whether `pattern` matches `relative` (both already `/`-separated).
///
/// An anchored pattern is matched against the whole path from the root; an
/// unanchored one (a bare basename glob) is matched against every path
/// segment, so `*.log` reaches `a/b/c.log` the way `git` itself does.
fn pattern_matches(pattern: &Pattern, relative: &str) -> bool {
    if pattern.anchored {
        return glob_match(&pattern.glob, relative);
    }
    relative.split('/').any(|segment| glob_match(&pattern.glob, segment))
        || glob_match(&pattern.glob, relative)
}

/// A small, dependency-free glob matcher: `*` (any run of non-`/`
/// characters), `**` (any run of characters, `/` included), `?` (exactly
/// one non-`/` character), and every other character literal. This is the
/// same "just enough, no catastrophic backtracking risk" scope
/// `regex_lite.rs` deliberately keeps to -- gitignore globs are simple
/// enough that a small recursive matcher never needs to worry about it.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn matches(pattern: &[char], text: &[char]) -> bool {
        match pattern.first() {
            None => text.is_empty(),
            Some('*') => {
                let is_double = pattern.get(1) == Some(&'*');
                // git's own stated special case: a leading "**/" also
                // matches *zero* directories, i.e. "**/foo" matches "foo"
                // itself, not only "anything/foo". Tried first, consuming
                // the slash along with the two stars, before the general
                // split-based attempt below (which requires some text to
                // actually precede that slash).
                if is_double && pattern.get(2) == Some(&'/') && matches(&pattern[3..], text) {
                    return true;
                }
                let rest = if is_double { &pattern[2..] } else { &pattern[1..] };
                for split in 0..=text.len() {
                    if !is_double && text[..split].contains(&'/') {
                        break;
                    }
                    if matches(rest, &text[split..]) {
                        return true;
                    }
                }
                false
            }
            Some('?') => {
                !text.is_empty() && text[0] != '/' && matches(&pattern[1..], &text[1..])
            }
            Some(&ch) => !text.is_empty() && text[0] == ch && matches(&pattern[1..], &text[1..]),
        }
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    matches(&pattern, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(text: &str) -> IgnoreSet {
        IgnoreSet::parse(text)
    }

    #[test]
    fn git_itself_is_always_ignored_even_with_an_empty_gitignore() {
        let set = set("");
        assert!(set.is_ignored(Path::new(".git"), true));
        assert!(set.is_ignored(Path::new(".git/config"), false));
    }

    #[test]
    fn a_bare_name_matches_at_any_depth() {
        let set = set("node_modules\n");
        assert!(set.is_ignored(Path::new("node_modules"), true));
        assert!(set.is_ignored(Path::new("packages/a/node_modules"), true));
    }

    #[test]
    fn a_leading_slash_anchors_to_the_root() {
        let set = set("/target\n");
        assert!(set.is_ignored(Path::new("target"), true));
        assert!(!set.is_ignored(Path::new("crates/core/target"), true), "not anchored elsewhere");
    }

    #[test]
    fn a_star_glob_matches_extensions() {
        let set = set("*.log\n");
        assert!(set.is_ignored(Path::new("output.log"), false));
        assert!(set.is_ignored(Path::new("logs/output.log"), false));
        assert!(!set.is_ignored(Path::new("output.log.txt"), false));
    }

    #[test]
    fn a_directory_only_pattern_never_matches_a_file() {
        let set = set("build/\n");
        assert!(set.is_ignored(Path::new("build"), true));
        assert!(!set.is_ignored(Path::new("build"), false), "a *file* named build is not this rule");
    }

    #[test]
    fn a_later_negation_reincludes_an_earlier_match() {
        let set = set("*.log\n!important.log\n");
        assert!(set.is_ignored(Path::new("debug.log"), false));
        assert!(!set.is_ignored(Path::new("important.log"), false));
    }

    #[test]
    fn pattern_order_decides_the_outcome_not_specificity() {
        // Re-ignoring after a negation is exactly as valid as the reverse --
        // `is_ignored` must not special-case "negation always wins".
        let set = set("!keep.txt\nkeep.txt\n");
        assert!(set.is_ignored(Path::new("keep.txt"), false));
    }

    #[test]
    fn comments_and_blank_lines_are_not_patterns() {
        let set = set("# a comment\n\n   \nnode_modules\n");
        assert_eq!(set.patterns.len(), 1);
    }

    #[test]
    fn a_middle_slash_anchors_even_without_a_leading_one() {
        // git's own rule: any "/" before the last character anchors the
        // pattern to the directory the .gitignore lives in.
        let set = set("src/generated\n");
        assert!(set.is_ignored(Path::new("src/generated"), true));
        assert!(!set.is_ignored(Path::new("other/src/generated"), true));
    }

    #[test]
    fn double_star_crosses_directory_boundaries() {
        let set = set("**/fixtures/*.json\n");
        assert!(set.is_ignored(Path::new("a/b/fixtures/data.json"), false));
        assert!(set.is_ignored(Path::new("fixtures/data.json"), false));
    }

    #[test]
    fn a_missing_gitignore_ignores_nothing_but_git_itself() {
        let dir = std::env::temp_dir().join(format!("ls-gitignore-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let set = IgnoreSet::load(&dir);
        assert!(!set.is_ignored(Path::new("anything.txt"), false));
        assert!(set.is_ignored(Path::new(".git"), true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loading_reads_the_roots_own_gitignore() {
        let dir = std::env::temp_dir().join(format!("ls-gitignore-test-load-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join(".gitignore"), "*.tmp\n").unwrap();
        let set = IgnoreSet::load(&dir);
        assert!(set.is_ignored(Path::new("scratch.tmp"), false));
        assert!(!set.is_ignored(Path::new("keep.rs"), false));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
