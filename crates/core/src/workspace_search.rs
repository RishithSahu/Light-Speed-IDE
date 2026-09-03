//! Workspace search (item 7): a recursive text search across files under the
//! workspace root.
//!
//! `Workspace::enumerate_children` is lazy by contract -- one directory level
//! per call, because a recursive walk is a scheduler-managed background task
//! (its own doc comment says so). This is that task: it walks the tree on a
//! worker, not the interactive thread, under `SubsystemId::SEARCH`.
//!
//! Matching reuses the same case-insensitive substring scan as in-document
//! find ([`crate::search`]); the only difference is what is being scanned --
//! many files instead of one open buffer.

use std::path::{Path, PathBuf};

/// Skip anything binary-shaped or huge enough that scanning it is pointless.
/// Not a content sniff (that would mean reading every file twice); a
/// conservative extension list is nearly free and catches the common cases.
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "pdf", "zip", "gz", "tar", "7z", "rar",
    "exe", "dll", "so", "dylib", "bin", "wasm", "woff", "woff2", "ttf", "otf", "class", "o", "pdb",
    "lock",
];

/// Directories never worth descending into.
const SKIP_DIRECTORIES: &[&str] = &[".git", "target", "node_modules", ".svn", ".hg"];

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RESULTS: usize = 500;
const MAX_FILES_SCANNED: usize = 20_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub path: PathBuf,
    pub line_number: usize,
    /// The matched line, trimmed, for the results panel to show without
    /// reopening the file.
    pub preview: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceSearchResult {
    pub query: String,
    pub hits: Vec<SearchHit>,
    /// True when the walk stopped early because it hit [`MAX_RESULTS`] or
    /// [`MAX_FILES_SCANNED`] -- the panel should say "more exist" rather than
    /// implying this is exhaustive.
    pub truncated: bool,
}

fn is_binary_shaped(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| BINARY_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn skip_directory(name: &str) -> bool {
    SKIP_DIRECTORIES.contains(&name)
}

/// Walks `root` depth-first, calling `on_file` for every regular file worth
/// scanning. A plain recursive `read_dir` rather than a crate: the workspace
/// trees this runs against are source trees, not filesystems with millions of
/// entries, so nothing here needs to be cleverer than "stop early".
fn walk(root: &Path, files_seen: &mut usize, on_file: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        if *files_seen >= MAX_FILES_SCANNED {
            return;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else { continue };
        if file_type.is_dir() {
            let name = entry.file_name();
            if skip_directory(&name.to_string_lossy()) {
                continue;
            }
            walk(&path, files_seen, on_file);
        } else if file_type.is_file() {
            *files_seen += 1;
            if !is_binary_shaped(&path) {
                on_file(&path);
            }
        }
    }
}

/// Searches every text file under `root` for `query`, case-insensitively.
pub fn search(root: &Path, query: &str) -> WorkspaceSearchResult {
    let mut result = WorkspaceSearchResult { query: query.to_string(), ..Default::default() };
    if query.is_empty() {
        return result;
    }
    let needle = query.to_lowercase();
    let mut files_seen = 0usize;

    walk(root, &mut files_seen, &mut |path| {
        if result.hits.len() >= MAX_RESULTS {
            result.truncated = true;
            return;
        }
        let Ok(metadata) = std::fs::metadata(path) else { return };
        if metadata.len() > MAX_FILE_BYTES {
            return;
        }
        let Ok(text) = std::fs::read_to_string(path) else { return };
        for (index, line) in text.lines().enumerate() {
            if result.hits.len() >= MAX_RESULTS {
                result.truncated = true;
                break;
            }
            if line.to_lowercase().contains(&needle) {
                result.hits.push(SearchHit {
                    path: path.to_path_buf(),
                    line_number: index + 1,
                    preview: line.trim().chars().take(200).collect(),
                });
            }
        }
    });

    if files_seen >= MAX_FILES_SCANNED {
        result.truncated = true;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("lightspeed-wsearch-{name}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn finds_matches_across_multiple_files() {
        let root = scratch("basic");
        std::fs::write(root.join("a.txt"), "needle here\nnothing\n").unwrap();
        std::fs::write(root.join("b.txt"), "nothing\nneedle again\n").unwrap();

        let result = search(&root, "needle");
        assert_eq!(result.hits.len(), 2);
        assert!(!result.truncated);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn descends_into_subdirectories() {
        let root = scratch("nested");
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::write(root.join("sub/deeper/f.txt"), "needle\n").unwrap();

        let result = search(&root, "needle");
        assert_eq!(result.hits.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skips_git_and_target_directories() {
        let root = scratch("skip");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/COMMIT_EDITMSG"), "needle\n").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/needle.txt"), "needle\n").unwrap();
        std::fs::write(root.join("real.txt"), "needle\n").unwrap();

        let result = search(&root, "needle");
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path.file_name().unwrap(), "real.txt");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_search_is_case_insensitive() {
        let root = scratch("case");
        std::fs::write(root.join("a.txt"), "NeeDLe\n").unwrap();
        let result = search(&root, "needle");
        assert_eq!(result.hits.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_query_finds_nothing_and_touches_no_files() {
        let root = scratch("empty");
        std::fs::write(root.join("a.txt"), "anything\n").unwrap();
        let result = search(&root, "");
        assert!(result.hits.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn binary_shaped_files_are_skipped() {
        let root = scratch("binary");
        std::fs::write(root.join("image.png"), "needle\n").unwrap();
        std::fs::write(root.join("real.txt"), "needle\n").unwrap();
        let result = search(&root, "needle");
        assert_eq!(result.hits.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn results_are_capped() {
        let root = scratch("cap");
        let mut content = String::new();
        for _ in 0..(super::MAX_RESULTS + 50) {
            content.push_str("needle\n");
        }
        std::fs::write(root.join("many.txt"), content).unwrap();
        let result = search(&root, "needle");
        assert_eq!(result.hits.len(), super::MAX_RESULTS);
        assert!(result.truncated);
        let _ = std::fs::remove_dir_all(&root);
    }
}
