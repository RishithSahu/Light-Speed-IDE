//! Read-only Git status (item 11: status only, no graph, no staging, no
//! commit -- those stay explicitly out of scope).
//!
//! `git status --porcelain=v1 -b` is parsed rather than libgit2 or a
//! from-scratch pack-file reader: the CLI is already present on any machine
//! that has Git, parsing its stable machine-readable format is a couple dozen
//! lines, and it avoids a heavyweight dependency for a read-only status list.
//! The process itself is bounded, one-shot work, so it runs as an ordinary
//! scheduler task under `SubsystemId::GIT` -- no new background thread.

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
}
