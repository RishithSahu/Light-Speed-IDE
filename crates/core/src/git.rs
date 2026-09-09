//! Git status, history, and commit support (item 11, since extended: status
//! was originally read-only -- no staging, no commit -- but the Source
//! Control panel now offers both, so this module parses `git log` too and
//! `crates/core/src/editor.rs` shells out to `git add`/`git commit`).
//!
//! `git status --porcelain=v1 -b` and `git log --pretty=format:...` are
//! parsed rather than using libgit2 or a from-scratch pack-file reader: the
//! CLI is already present on any machine that has Git, parsing its stable
//! machine-readable formats is a couple dozen lines each, and it avoids a
//! heavyweight dependency for what is still fundamentally a thin read/write
//! wrapper around a handful of `git` subcommands. The process itself is
//! bounded, one-shot work, so it runs as an ordinary scheduler task under
//! `SubsystemId::GIT` -- no new background thread.
//!
//! Staging is deliberately not per-file: the Commit button stages everything
//! (`git add -A`) and commits it in one step, the same fallback VS Code's own
//! Source Control view uses when nothing has been staged by hand. A
//! checkbox-per-file staging model is a real feature this does not attempt to
//! be -- it would need its own click-to-toggle UI and a way to represent a
//! file being staged while a request is still in flight, neither of which
//! exists here yet.

use std::path::PathBuf;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GitFileState {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitFileStatus {
    pub path: PathBuf,
    pub state: GitFileState,
    pub staged: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitStatus {
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<GitFileStatus>,
}

impl GitStatus {
    pub fn is_clean(&self) -> bool {
        self.files.is_empty()
    }
}

fn file_state(code: u8) -> Option<GitFileState> {
    match code {
        b'M' => Some(GitFileState::Modified),
        b'A' => Some(GitFileState::Added),
        b'D' => Some(GitFileState::Deleted),
        b'R' => Some(GitFileState::Renamed),
        b'U' => Some(GitFileState::Conflicted),
        _ => None,
    }
}

/// Parses `git status --porcelain=v1 -b` output.
///
/// Porcelain v1 is a stable, documented format (unlike plain `git status`,
/// which is meant for a human terminal and can change across Git versions),
/// which is why this is the flag used even though it is less readable.
pub fn parse_porcelain(output: &str) -> GitStatus {
    let mut status = GitStatus::default();
    for line in output.lines() {
        if let Some(branch_line) = line.strip_prefix("## ") {
            let name = branch_line.split(['.', ' ', '[']).next().unwrap_or(branch_line);
            if !name.is_empty() && name != "HEAD" {
                status.branch = Some(name.to_string());
            }
            if let Some(start) = branch_line.find('[') {
                let tracking =
                    &branch_line[start + 1..branch_line.len().saturating_sub(1).max(start + 1)];
                for part in tracking.split(", ") {
                    if let Some(n) = part.strip_prefix("ahead ") {
                        status.ahead = n.trim_end_matches(']').parse().unwrap_or(0);
                    } else if let Some(n) = part.strip_prefix("behind ") {
                        status.behind = n.trim_end_matches(']').parse().unwrap_or(0);
                    }
                }
            }
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        let bytes = line.as_bytes();
        let (index_char, worktree_char) = (bytes[0], bytes[1]);
        let path = PathBuf::from(&line[3..]);

        if index_char == b'?' && worktree_char == b'?' {
            status.files.push(GitFileStatus {
                path,
                state: GitFileState::Untracked,
                staged: false,
            });
            continue;
        }
        if let Some(state) = file_state(index_char) {
            status.files.push(GitFileStatus { path: path.clone(), state, staged: true });
        }
        if let Some(state) = file_state(worktree_char) {
            status.files.push(GitFileStatus { path, state, staged: false });
        }
    }
    status
}

/// One entry from `git log`, as parsed by [`parse_log`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitLogEntry {
    pub hash: String,
    pub short_hash: String,
    pub author: String,
    pub date: String,
    pub summary: String,
    /// This commit's parents, full hashes, in `git`'s own order (first
    /// parent first). Empty for a root commit, one entry for an ordinary
    /// commit, two or more for a merge -- exactly the shape
    /// [`crate::graph::lanes`] needs to lay real branch/merge topology out
    /// as more than a single straight line.
    pub parents: Vec<String>,
}

/// Parses `git log --pretty=format:%H%x1f%h%x1f%an%x1f%ad%x1f%s%x1f%P`
/// output, one commit per line.
///
/// Fields are separated by `\x1f` (ASCII unit separator) rather than a comma
/// or a space, the same reasoning `parse_porcelain` above has no need for but
/// this does: a commit summary is free-form text a human wrote, and can
/// contain either of those without anything stopping it. A control character
/// no commit message would ever type is the one separator that cannot be
/// mistaken for content. `%P` (parent hashes) trails every other field
/// because it is the only one that can itself be empty (a root commit has no
/// parents) or contain the field separator's counterpart, a plain space --
/// putting it last means a short line still parses every field before it.
pub fn parse_log(output: &str) -> Vec<CommitLogEntry> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\u{1f}');
            let hash = fields.next()?.to_string();
            let short_hash = fields.next()?.to_string();
            let author = fields.next()?.to_string();
            let date = fields.next()?.to_string();
            let summary = fields.next().unwrap_or_default().to_string();
            let parents = fields
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_string)
                .collect();
            Some(CommitLogEntry { hash, short_hash, author, date, summary, parents })
        })
        .collect()
}

/// The result of a `git add -A && git commit -m <message>` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCommitOutcome {
    pub success: bool,
    /// A short status line to show the user: "Committed" on success, or
    /// `git`'s own stderr (trimmed) on failure -- a merge conflict, nothing
    /// staged, no user.name/user.email configured, and so on are all real
    /// reasons this can fail, and are worth showing verbatim rather than
    /// flattened into a generic "commit failed".
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_repository_has_no_files() {
        let status = parse_porcelain("## main...origin/main\n");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert!(status.is_clean());
    }

    #[test]
    fn modified_and_untracked_files_are_reported() {
        let status = parse_porcelain("## main\n M src/lib.rs\n?? new_file.txt\n");
        assert_eq!(status.files.len(), 2);
        assert_eq!(status.files[0].state, GitFileState::Modified);
        assert!(!status.files[0].staged);
        assert_eq!(status.files[1].state, GitFileState::Untracked);
    }

    #[test]
    fn a_staged_and_further_modified_file_reports_both() {
        // "MM" -> staged modification plus a newer unstaged one.
        let status = parse_porcelain("## main\nMM src/lib.rs\n");
        assert_eq!(status.files.len(), 2);
        assert!(status.files[0].staged);
        assert!(!status.files[1].staged);
    }

    #[test]
    fn ahead_and_behind_counts_are_parsed() {
        let status = parse_porcelain("## main...origin/main [ahead 2, behind 1]\n");
        assert_eq!(status.ahead, 2);
        assert_eq!(status.behind, 1);
    }

    #[test]
    fn a_detached_head_has_no_branch_name() {
        let status = parse_porcelain("## HEAD (no branch)\n");
        assert_eq!(status.branch, None);
    }

    #[test]
    fn deleted_and_renamed_files_are_recognized() {
        let status = parse_porcelain("## main\n D gone.txt\nR  old.txt -> new.txt\n");
        assert_eq!(status.files[0].state, GitFileState::Deleted);
        assert_eq!(status.files[1].state, GitFileState::Renamed);
    }

    #[test]
    fn a_single_commit_line_is_parsed_into_its_five_fields() {
        let log = parse_log("abc123full\u{1f}abc123\u{1f}Jane Doe\u{1f}2026-01-05\u{1f}Fix the thing\u{1f}\n");
        assert_eq!(
            log,
            vec![CommitLogEntry {
                hash: "abc123full".to_string(),
                short_hash: "abc123".to_string(),
                author: "Jane Doe".to_string(),
                date: "2026-01-05".to_string(),
                summary: "Fix the thing".to_string(),
                parents: Vec::new(),
            }]
        );
    }

    #[test]
    fn a_field_missing_entirely_still_parses_as_an_empty_parent_list() {
        // A short, malformed line (older data, a truncated pipe read) must
        // not panic just because %P never showed up at all.
        let log = parse_log("h\u{1f}s\u{1f}A\u{1f}2026-01-01\u{1f}no parents field\n");
        assert_eq!(log[0].parents, Vec::<String>::new());
    }

    #[test]
    fn a_merge_commits_two_parents_are_both_parsed() {
        let log =
            parse_log("h\u{1f}s\u{1f}A\u{1f}2026-01-01\u{1f}Merge branch 'x'\u{1f}p1full p2full\n");
        assert_eq!(log[0].parents, vec!["p1full".to_string(), "p2full".to_string()]);
    }

    #[test]
    fn multiple_commits_are_parsed_in_order() {
        let log = parse_log(
            "h2\u{1f}s2\u{1f}A\u{1f}2026-01-02\u{1f}second\nh1\u{1f}s1\u{1f}A\u{1f}2026-01-01\u{1f}first\n",
        );
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].summary, "second", "git log lists newest first, unchanged here");
        assert_eq!(log[1].summary, "first");
    }

    #[test]
    fn a_summary_containing_the_word_comma_is_kept_intact() {
        // The whole reason the parser splits on the unit separator instead
        // of a comma or a space: a real commit message can contain either.
        let log = parse_log("h\u{1f}s\u{1f}A\u{1f}2026-01-01\u{1f}fix: a, b, and c\n");
        assert_eq!(log[0].summary, "fix: a, b, and c");
    }

    #[test]
    fn empty_log_output_parses_to_no_commits() {
        assert_eq!(parse_log(""), Vec::new());
    }

    #[test]
    fn a_malformed_line_missing_fields_is_skipped_rather_than_panicking() {
        let log = parse_log("only\u{1f}two\nh\u{1f}s\u{1f}A\u{1f}2026-01-01\u{1f}ok\n");
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].summary, "ok");
    }
}
