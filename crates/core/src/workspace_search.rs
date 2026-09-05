//! Workspace search (item 7): a recursive text search across files under the
//! workspace root.
//!
//! `Workspace::enumerate_children` is lazy by contract -- one directory level
//! per call, because a recursive walk is a scheduler-managed background task
//! (its own doc comment says so). This is that task: it walks the tree on a
//! worker, not the interactive thread, under `SubsystemId::SEARCH`.
//!
//! # Why this is written the way it is
//!
//! The shell re-runs this on a debounce as the user types (see the search
//! panel in `app::sidebar_rows`), so a search is not a rare, deliberate act
//! any more -- it is something that happens every few hundred milliseconds
//! against the whole workspace, and gets thrown away when the next keystroke
//! lands. Two consequences shape everything below:
//!
//! - **It has to stop the moment it is superseded.** [`search_cancellable`]
//!   checks the caller's cancellation flag once per directory entry and once
//!   per file, so a search that is no longer wanted stops within one file's
//!   worth of work instead of walking the rest of the tree to produce a
//!   result nobody will read.
//! - **It has to allocate almost nothing.** The obvious implementation --
//!   `line.to_lowercase().contains(needle)` -- allocates a fresh `String` for
//!   *every line of every file*, which on a real repository is millions of
//!   allocations per keystroke. Instead an ASCII query (very nearly all of
//!   them) is matched directly against the file's bytes with
//!   [`find_ascii_case_insensitive`], and one read buffer is reused across
//!   every file in the walk rather than allocating per file. A non-ASCII
//!   query still takes the allocating path, because correct Unicode case
//!   folding is not a byte operation -- it is just rare enough not to matter.
//!
//! Matching semantics are unchanged from the original per-line scan and match
//! in-document find ([`crate::search`]): case-insensitive substring, at most
//! one hit per line.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Skip anything binary-shaped or huge enough that scanning it is pointless.
/// Not a content sniff (that would mean reading every file twice); a
/// conservative extension list is nearly free and catches the common cases.
/// Files that slip through and turn out not to be UTF-8 are skipped when they
/// are read, so this list is an optimization, not the correctness boundary.
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

/// The read buffer is reused across files, so it naturally grows to the size
/// of the largest file in the tree and stays there. Past this, it is shrunk
/// back down after the file that grew it: holding megabytes of buffer for the
/// rest of a walk to save a handful of reallocations is the wrong trade for a
/// search that runs on every keystroke.
const BUFFER_KEEP_BYTES: usize = 256 * 1024;

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
    /// True when the caller cancelled the search partway through, so the hits
    /// here are whatever had been found by then. The shell drops these rather
    /// than showing a half-finished list (a cancelled search is one whose
    /// query is already stale), but the distinction has to survive the return
    /// trip to be actionable at all.
    pub cancelled: bool,
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

/// Finds `needle` (already lowercased, and ASCII) in `haystack`,
/// case-insensitively, without allocating.
///
/// Safe to run against arbitrary UTF-8: every byte of a multi-byte UTF-8
/// sequence has its high bit set, so an ASCII needle can never match part of
/// one, and the returned offset is always a character boundary.
fn find_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let first_lower = needle[0];
    let first_upper = first_lower.to_ascii_uppercase();
    let last_start = haystack.len() - needle.len();
    let mut at = 0;
    while at <= last_start {
        let byte = haystack[at];
        if (byte == first_lower || byte == first_upper)
            && haystack[at..at + needle.len()].eq_ignore_ascii_case(needle)
        {
            return Some(at);
        }
        at += 1;
    }
    None
}

fn count_newlines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
}

fn preview_of(line: &[u8]) -> String {
    // The buffer was validated as UTF-8 before scanning and lines are split on
    // ASCII newlines, so this only fails on a slice that cannot occur; falling
    // back to lossy conversion keeps that impossible case from being a panic.
    match std::str::from_utf8(line) {
        Ok(text) => text.trim().chars().take(200).collect(),
        Err(_) => String::from_utf8_lossy(line).trim().chars().take(200).collect(),
    }
}

/// Whether the result is full. Separated out because both scan paths need the
/// same "stop and mark truncated" decision.
fn is_full(result: &mut WorkspaceSearchResult) -> bool {
    if result.hits.len() >= MAX_RESULTS {
        result.truncated = true;
        return true;
    }
    false
}

/// Scans one file's text for an ASCII query, over the raw bytes, allocating
/// only for the previews of lines that actually match.
///
/// Walks match-to-match rather than line-to-line: after a hit, the scan
/// resumes at the start of the *next* line, so lines without a match are
/// never examined individually and a line with several matches still reports
/// once. Line numbers come from counting newlines between consecutive match
/// positions, which totals one pass over the file regardless of hit count.
fn scan_ascii(bytes: &[u8], needle: &[u8], path: &Path, result: &mut WorkspaceSearchResult) {
    let mut scan_from = 0usize;
    // Byte offset where the line containing `counted_to` begins, and that
    // line's 1-based number. Both only ever move forward.
    let mut line_start = 0usize;
    let mut line_number = 1usize;

    while scan_from < bytes.len() {
        let Some(relative) = find_ascii_case_insensitive(&bytes[scan_from..], needle) else {
            return;
        };
        let at = scan_from + relative;

        // Advance the line counter over everything between the last line we
        // resolved and this match.
        let newlines = count_newlines(&bytes[line_start..at]);
        if newlines > 0 {
            line_number += newlines;
            line_start = bytes[line_start..at]
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map(|offset| line_start + offset + 1)
                .unwrap_or(line_start);
        }

        let line_end =
            bytes[at..].iter().position(|byte| *byte == b'\n').map(|o| at + o).unwrap_or(bytes.len());
        // Trim a trailing carriage return so a CRLF file's previews match an
        // LF file's.
        let mut visible_end = line_end;
        if visible_end > line_start && bytes[visible_end - 1] == b'\r' {
            visible_end -= 1;
        }

        result.hits.push(SearchHit {
            path: path.to_path_buf(),
            line_number,
            preview: preview_of(&bytes[line_start..visible_end]),
        });
        if is_full(result) {
            return;
        }

        // Resume on the next line: at most one hit per line.
        scan_from = line_end + 1;
        line_start = scan_from;
        line_number += 1;
    }
}

/// The Unicode path, for the rare query that is not pure ASCII. Correct case
/// folding is not a byte operation, so this is the original allocating scan.
fn scan_unicode(text: &str, needle: &str, path: &Path, result: &mut WorkspaceSearchResult) {
    for (index, line) in text.lines().enumerate() {
        if line.to_lowercase().contains(needle) {
            result.hits.push(SearchHit {
                path: path.to_path_buf(),
                line_number: index + 1,
                preview: line.trim().chars().take(200).collect(),
            });
            if is_full(result) {
                return;
            }
        }
    }
}

/// Owns the one read buffer shared by every file in a walk.
#[derive(Default)]
struct Scanner {
    buffer: Vec<u8>,
}

impl Scanner {
    /// Reads `path` into the shared buffer. `None` means "skip this file":
    /// unreadable, or not UTF-8 (which is the real binary check -- the
    /// extension list is only a shortcut that avoids reading the obvious
    /// cases at all).
    fn read(&mut self, path: &Path) -> Option<&str> {
        self.buffer.clear();
        let mut file = std::fs::File::open(path).ok()?;
        file.read_to_end(&mut self.buffer).ok()?;
        std::str::from_utf8(&self.buffer).ok()
    }

    fn release_if_oversized(&mut self) {
        if self.buffer.capacity() > BUFFER_KEEP_BYTES {
            self.buffer = Vec::with_capacity(BUFFER_KEEP_BYTES);
        }
    }
}

/// Walks `root` depth-first, calling `on_file` for every regular file worth
/// scanning, with the size the directory listing already reported so no
/// second `stat` is needed. A plain recursive `read_dir` rather than a crate:
/// the workspace trees this runs against are source trees, not filesystems
/// with millions of entries, so nothing here needs to be cleverer than "stop
/// early".
///
/// Returns `false` if the walk was cancelled, so callers can distinguish "no
/// more files" from "stop now".
fn walk(
    root: &Path,
    files_seen: &mut usize,
    is_cancelled: &dyn Fn() -> bool,
    on_file: &mut impl FnMut(&Path, u64) -> bool,
) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else { return true };
    for entry in entries.flatten() {
        if *files_seen >= MAX_FILES_SCANNED || is_cancelled() {
            return !is_cancelled();
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else { continue };
        if file_type.is_dir() {
            let name = entry.file_name();
            if skip_directory(&name.to_string_lossy()) {
                continue;
            }
            if !walk(&path, files_seen, is_cancelled, on_file) {
                return false;
            }
        } else if file_type.is_file() {
            *files_seen += 1;
            if is_binary_shaped(&path) {
                continue;
            }
            // The listing already knows the size on every platform this runs
            // on; asking it here avoids a `stat` per file.
            let len = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            if !on_file(&path, len) {
                return true;
            }
        }
    }
    true
}

/// Searches every text file under `root` for `query`, case-insensitively.
pub fn search(root: &Path, query: &str) -> WorkspaceSearchResult {
    search_cancellable(root, query, &|| false)
}

/// [`search`], but stopping as soon as `is_cancelled` returns true.
///
/// The flag is polled once per directory entry and once per file, so the
/// worst case between "no longer wanted" and "stopped" is a single file's
/// scan -- bounded by [`MAX_FILE_BYTES`] rather than by the size of the
/// workspace.
pub fn search_cancellable(
    root: &Path,
    query: &str,
    is_cancelled: &dyn Fn() -> bool,
) -> WorkspaceSearchResult {
    let mut result = WorkspaceSearchResult { query: query.to_string(), ..Default::default() };
    if query.is_empty() {
        return result;
    }
    let needle = query.to_lowercase();
    let ascii_needle = needle.is_ascii().then(|| needle.as_bytes().to_vec());
    let mut files_seen = 0usize;
    let mut scanner = Scanner::default();

    let completed = walk(root, &mut files_seen, is_cancelled, &mut |path, len| {
        if is_full(&mut result) {
            return false;
        }
        if len > MAX_FILE_BYTES {
            return true;
        }
        if let Some(text) = scanner.read(path) {
            match &ascii_needle {
                Some(bytes) => {
                    // Borrowed from the buffer rather than the &str, so the
                    // scan works in bytes throughout.
                    scan_ascii(text.as_bytes(), bytes, path, &mut result)
                }
                None => scan_unicode(text, &needle, path, &mut result),
            }
        }
        scanner.release_if_oversized();
        !is_cancelled()
    });

    if !completed || is_cancelled() {
        result.cancelled = true;
        return result;
    }
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

    #[test]
    fn line_numbers_are_the_lines_the_matches_are_actually_on() {
        // The byte scan tracks line numbers by counting newlines between
        // matches rather than by iterating lines, so getting this wrong is a
        // silent, plausible-looking off-by-N rather than an obvious break.
        let root = scratch("lines");
        std::fs::write(root.join("a.txt"), "one\ntwo\nneedle\nfour\nneedle\n").unwrap();
        let result = search(&root, "needle");
        assert_eq!(
            result.hits.iter().map(|hit| hit.line_number).collect::<Vec<_>>(),
            vec![3, 5]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_line_matching_twice_is_reported_once() {
        let root = scratch("twice");
        std::fs::write(root.join("a.txt"), "needle and needle again\nplain\n").unwrap();
        let result = search(&root, "needle");
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].line_number, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_match_on_the_last_line_without_a_trailing_newline_is_found() {
        let root = scratch("noeol");
        std::fs::write(root.join("a.txt"), "first\nneedle").unwrap();
        let result = search(&root, "needle");
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].line_number, 2);
        assert_eq!(result.hits[0].preview, "needle");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn crlf_previews_do_not_keep_the_carriage_return() {
        let root = scratch("crlf");
        std::fs::write(root.join("a.txt"), "one\r\nneedle here\r\nthree\r\n").unwrap();
        let result = search(&root, "needle");
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].line_number, 2);
        assert_eq!(result.hits[0].preview, "needle here");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_non_ascii_query_still_matches_case_insensitively() {
        // Falls through to the allocating Unicode path; the point is that the
        // fast path never silently swallows a query it cannot fold.
        let root = scratch("unicode");
        std::fs::write(root.join("a.txt"), "Grüße aus Köln\n").unwrap();
        let result = search(&root, "GRÜSSE").hits.len() + search(&root, "grüße").hits.len();
        assert_eq!(result, 1, "the exact-fold query matches, the eszett-folded one need not");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_ascii_query_matches_inside_a_file_full_of_multibyte_text() {
        // A UTF-8 continuation byte can never be mistaken for ASCII, so the
        // byte scan must find this without tripping over the surrounding
        // characters or reporting a position mid-character.
        let root = scratch("mixed");
        std::fs::write(root.join("a.txt"), "日本語のtextとneedleが混ざる\n").unwrap();
        let result = search(&root, "NEEDLE");
        assert_eq!(result.hits.len(), 1);
        assert!(result.hits[0].preview.contains("needle"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn files_that_are_not_utf8_are_skipped_rather_than_mangled() {
        let root = scratch("invalid");
        // `.dat` is not on the extension skip list, so this only gets skipped
        // if the read itself rejects it.
        std::fs::write(root.join("blob.dat"), [0xff, 0xfe, b'n', b'e', b'e', b'd', b'l', b'e'])
            .unwrap();
        std::fs::write(root.join("real.txt"), "needle\n").unwrap();
        let result = search(&root, "needle");
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path.file_name().unwrap(), "real.txt");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cancelled_search_stops_early_and_says_so() {
        let root = scratch("cancel");
        for index in 0..64 {
            std::fs::write(root.join(format!("f{index}.txt")), "needle\n").unwrap();
        }
        // Cancel after the very first file is scanned.
        let scanned = std::cell::Cell::new(0usize);
        let result = search_cancellable(&root, "needle", &|| {
            let seen = scanned.get();
            scanned.set(seen + 1);
            seen > 2
        });
        assert!(result.cancelled, "a cancelled walk must report itself cancelled");
        assert!(
            result.hits.len() < 64,
            "cancelling must actually stop the walk, not just flag the result"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_search_that_runs_to_completion_is_not_marked_cancelled() {
        let root = scratch("uncancelled");
        std::fs::write(root.join("a.txt"), "needle\n").unwrap();
        let result = search_cancellable(&root, "needle", &|| false);
        assert!(!result.cancelled);
        assert_eq!(result.hits.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A/B measurement of the byte scan against the per-line
    /// `to_lowercase().contains()` it replaced, on a tree shaped like a real
    /// project. Ignored by default -- it builds a few thousand files and is
    /// a measurement, not an assertion about this machine's speed. Run with:
    /// `cargo test -p ls-core --release -- --ignored --nocapture scan_throughput`
    #[test]
    #[ignore = "perf measurement, not a correctness check"]
    fn scan_throughput_against_the_allocating_scan_it_replaced() {
        let root = scratch("throughput");
        let mut bytes = 0u64;
        for module in 0..8 {
            let dir = root.join(format!("module_{module}"));
            std::fs::create_dir_all(&dir).unwrap();
            for index in 0..250 {
                let mut content = String::new();
                for line in 0..120 {
                    if line == 60 && index % 7 == 0 {
                        content.push_str("    let value = \"findme_marker_token\";\n");
                    } else {
                        content.push_str(
                            "    // an ordinary line of source that does not match anything\n",
                        );
                    }
                }
                bytes += content.len() as u64;
                std::fs::write(dir.join(format!("file_{index}.rs")), content).unwrap();
            }
        }

        let time = |label: &str, run: &dyn Fn() -> usize| {
            let started = std::time::Instant::now();
            let hits = run();
            let elapsed = started.elapsed();
            let mb = bytes as f64 / (1024.0 * 1024.0);
            println!(
                "{label}: {:?} for {mb:.1} MiB ({:.0} MiB/s), {hits} hits",
                elapsed,
                mb / elapsed.as_secs_f64()
            );
            elapsed
        };

        let fast = time("byte scan       ", &|| search(&root, "findme_marker_token").hits.len());
        let slow = time("per-line lowercase", &|| {
            // The implementation this replaced, reproduced exactly.
            let mut hits = 0usize;
            let needle = "findme_marker_token".to_lowercase();
            let mut files_seen = 0usize;
            walk(&root, &mut files_seen, &|| false, &mut |path, _| {
                if let Ok(text) = std::fs::read_to_string(path) {
                    for line in text.lines() {
                        if line.to_lowercase().contains(&needle) {
                            hits += 1;
                        }
                    }
                }
                true
            });
            hits
        });
        println!("speedup: {:.1}x", slow.as_secs_f64() / fast.as_secs_f64());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_case_insensitive_byte_search_finds_what_it_should_and_nothing_else() {
        assert_eq!(find_ascii_case_insensitive(b"hello world", b"world"), Some(6));
        assert_eq!(find_ascii_case_insensitive(b"HELLO WORLD", b"world"), Some(6));
        assert_eq!(find_ascii_case_insensitive(b"hello", b"nothing"), None);
        assert_eq!(find_ascii_case_insensitive(b"", b"x"), None);
        assert_eq!(find_ascii_case_insensitive(b"abc", b""), None);
        assert_eq!(find_ascii_case_insensitive(b"aab", b"ab"), Some(1), "restarts after a near miss");
    }
}
