//! Per-language coverage for the language pipeline: detection, highlighting
//! and workspace search, for every language in [`Language::ALL`].
//!
//! # Two tiers, deliberately
//!
//! Everything in `mod fixtures` is hermetic and runs in the ordinary suite:
//! small snippets written here, no network, no clone, milliseconds. That is
//! what keeps `cargo test` fast enough to run constantly, which is the only
//! reason it gets run at all.
//!
//! `mod real_repos` is the other half of the request -- point the pipeline at
//! *actual* code in each language and see what breaks -- and it is
//! `#[ignore]`d. Cloning repositories inside the default test run would make
//! the suite depend on the network, on GitHub being up, and on several
//! hundred megabytes of disk, and would take it from under a second to
//! minutes. A test suite that slow stops being run, so the corpus tests are
//! opt-in:
//!
//! ```text
//! cargo test -p ls-tests -- --ignored --nocapture real_repos
//! ```
//!
//! They are worth running because synthetic fixtures cannot produce what real
//! code does: files that are megabytes of one line, CRLF mixed with LF,
//! identifiers in Cyrillic and CJK, emoji in string literals, and comment
//! nesting nobody would think to write by hand. The invariants they check are
//! chosen to catch exactly those -- particularly that a highlight token's
//! character columns stay inside the line they belong to, which is the class
//! of bug multi-byte source finds and ASCII fixtures never will.

use ls_core::highlight::{tokenize_line, LexState};
use ls_core::language::{detect_language, Language};
use ls_core::workspace_search;
use std::path::{Path, PathBuf};

/// A representative snippet per language, with the extension it should be
/// detected by and a token that must survive a workspace search.
struct Fixture {
    language: Language,
    file_name: &'static str,
    source: &'static str,
    /// An identifier in `source` that a search must find.
    needle: &'static str,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        language: Language::Rust,
        file_name: "main.rs",
        source: "// a comment\nfn findme_token() -> &'static str {\n    \"a string\"\n}\n",
        needle: "findme_token",
    },
    Fixture {
        language: Language::Python,
        file_name: "app.py",
        source: "# a comment\ndef findme_token():\n    return \"a string\"\n",
        needle: "findme_token",
    },
    Fixture {
        language: Language::C,
        file_name: "main.c",
        source: "/* a comment */\nint findme_token(void) {\n    return 0;\n}\n",
        needle: "findme_token",
    },
    Fixture {
        language: Language::Cpp,
        file_name: "main.cpp",
        source: "// a comment\nclass FindmeToken {\npublic:\n    int findme_token = 1;\n};\n",
        needle: "findme_token",
    },
    Fixture {
        language: Language::CSharp,
        file_name: "Program.cs",
        source: "// a comment\npublic class Program {\n    int findme_token = 1;\n}\n",
        needle: "findme_token",
    },
    Fixture {
        language: Language::Go,
        file_name: "main.go",
        source: "// a comment\npackage main\n\nfunc findme_token() string {\n    return \"s\"\n}\n",
        needle: "findme_token",
    },
    Fixture {
        language: Language::JavaScript,
        file_name: "index.js",
        source: "// a comment\nfunction findme_token() {\n  return \"a string\";\n}\n",
        needle: "findme_token",
    },
    Fixture {
        language: Language::TypeScript,
        file_name: "index.ts",
        source: "// a comment\nfunction findme_token(): string {\n  return \"a string\";\n}\n",
        needle: "findme_token",
    },
    Fixture {
        language: Language::Json,
        file_name: "package.json",
        source: "{\n  \"findme_token\": 1\n}\n",
        needle: "findme_token",
    },
    Fixture {
        language: Language::Toml,
        file_name: "Cargo.toml",
        source: "# a comment\n[package]\nfindme_token = \"1\"\n",
        needle: "findme_token",
    },
    Fixture {
        language: Language::Yaml,
        file_name: "config.yaml",
        source: "# a comment\nfindme_token: 1\n",
        needle: "findme_token",
    },
    Fixture {
        language: Language::Markdown,
        file_name: "README.md",
        source: "# Title\n\nfindme_token in prose.\n",
        needle: "findme_token",
    },
    Fixture {
        language: Language::Shell,
        file_name: "run.sh",
        source: "# a comment\nfindme_token() {\n  echo \"a string\"\n}\n",
        needle: "findme_token",
    },
];

fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir()
        .join(format!("lightspeed-langtest-{name}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Tokenizes a whole document, threading the block-comment state across lines
/// the way the renderer does, and checks the invariant that matters most:
/// every token's character columns lie inside the line it came from.
///
/// This is the check real-world source earns its keep on. Columns are counted
/// in *characters*, so a token whose end column is computed from a byte
/// offset looks perfectly correct until the first line containing an accent,
/// a CJK identifier or an emoji in a string literal.
fn tokenize_document(text: &str, language: Language) -> Result<usize, String> {
    let mut state = LexState::default();
    let mut total = 0usize;
    for (index, line) in text.lines().enumerate() {
        let char_count = line.chars().count();
        let (tokens, next) = tokenize_line(line, language, state);
        state = next;
        for token in &tokens {
            if token.start_column_chars > token.end_column_chars {
                return Err(format!(
                    "line {}: token runs backwards ({} > {})",
                    index + 1,
                    token.start_column_chars,
                    token.end_column_chars
                ));
            }
            if token.end_column_chars > char_count {
                return Err(format!(
                    "line {}: token ends at char {} but the line is {char_count} chars \
                     (a byte offset leaking into a character column)",
                    index + 1,
                    token.end_column_chars
                ));
            }
        }
        total += tokens.len();
    }
    Ok(total)
}

mod fixtures {
    use super::*;

    #[test]
    fn every_language_has_a_fixture() {
        // Without this, adding a language quietly adds an untested one: the
        // per-language tests below only cover what is in FIXTURES.
        for language in Language::ALL {
            if *language == Language::PlainText {
                continue;
            }
            assert!(
                FIXTURES.iter().any(|fixture| fixture.language == *language),
                "{} has no fixture in tests/tests/languages.rs",
                language.name()
            );
        }
    }

    #[test]
    fn each_fixture_is_detected_as_its_own_language() {
        for fixture in FIXTURES {
            assert_eq!(
                detect_language(Path::new(fixture.file_name)),
                fixture.language,
                "{} was not detected from {}",
                fixture.language.name(),
                fixture.file_name
            );
        }
    }

    #[test]
    fn each_fixture_tokenizes_within_its_own_lines() {
        for fixture in FIXTURES {
            match tokenize_document(fixture.source, fixture.language) {
                Ok(_) => {}
                Err(problem) => panic!("{}: {problem}", fixture.language.name()),
            }
        }
    }

    #[test]
    fn languages_with_a_highlighter_actually_produce_tokens() {
        // JSON and Markdown have no highlighter by design (see
        // `highlight::config_for`); everything else claiming one should emit
        // something for a snippet that is deliberately full of keywords,
        // comments and strings.
        for fixture in FIXTURES {
            let produced = tokenize_document(fixture.source, fixture.language).unwrap();
            let expected = !matches!(fixture.language, Language::Json | Language::Markdown);
            assert_eq!(
                produced > 0,
                expected,
                "{} produced {produced} tokens",
                fixture.language.name()
            );
        }
    }

    #[test]
    fn workspace_search_finds_a_symbol_in_every_language() {
        // One tree containing a file of every language at once, which is also
        // the realistic case: a project is not written in one language.
        let root = scratch("all-languages");
        for fixture in FIXTURES {
            std::fs::write(root.join(fixture.file_name), fixture.source).unwrap();
        }

        let result = workspace_search::search(&root, "findme_token");
        for fixture in FIXTURES {
            assert!(
                result.hits.iter().any(|hit| hit.path.file_name().unwrap() == fixture.file_name),
                "search missed {} ({})",
                fixture.file_name,
                fixture.language.name()
            );
            assert!(fixture.source.contains(fixture.needle));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_search_hit_points_at_a_line_that_really_contains_the_query() {
        // The byte scan reports line numbers by counting newlines between
        // matches; this is the end-to-end check that the number it reports is
        // the line the text is actually on, per language and per file.
        let root = scratch("hit-lines");
        for fixture in FIXTURES {
            std::fs::write(root.join(fixture.file_name), fixture.source).unwrap();
        }

        let result = workspace_search::search(&root, "findme_token");
        assert!(!result.hits.is_empty());
        for hit in &result.hits {
            let text = std::fs::read_to_string(&hit.path).unwrap();
            let line = text
                .lines()
                .nth(hit.line_number - 1)
                .unwrap_or_else(|| panic!("{:?} has no line {}", hit.path, hit.line_number));
            assert!(
                line.to_lowercase().contains("findme_token"),
                "{:?} line {} does not contain the query: {line:?}",
                hit.path,
                hit.line_number
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn multibyte_source_does_not_shift_highlight_columns() {
        // The bug this exists to catch, written out explicitly: identifiers
        // and comments in non-ASCII scripts, where a byte offset and a
        // character offset diverge on the very first token.
        let cases = [
            (Language::Rust, "// комментарий на русском\nfn функция() {}\n"),
            (Language::Python, "# 日本語のコメント\ndef 関数():\n    return \"文字列\"\n"),
            (Language::JavaScript, "// emoji in a string\nconst s = \"🎉 party\";\n"),
            (Language::Go, "// éàü accents\nfunc café() string { return \"naïve\" }\n"),
        ];
        for (language, source) in cases {
            if let Err(problem) = tokenize_document(source, language) {
                panic!("{}: {problem}", language.name());
            }
        }
    }
}

/// Opt-in corpus tests against real repositories.
///
/// Run with:
/// `cargo test -p ls-tests -- --ignored --nocapture real_repos`
///
/// Each repository is shallow-cloned once into `target/language-corpus` and
/// reused on later runs. If `git` is missing or the network is unavailable,
/// the test reports what it skipped and passes rather than failing: these
/// checks are here to find bugs in the pipeline, not to assert that the
/// machine running them has internet.
mod real_repos {
    use super::*;

    /// One repository per language, chosen to be small, popular and
    /// idiomatic -- real code the pipeline can be pointed at, not a curated
    /// sample that avoids the hard cases.
    const CORPUS: &[(Language, &str, &str)] = &[
        (Language::Rust, "https://github.com/BurntSushi/memchr", "rs"),
        (Language::Python, "https://github.com/psf/requests", "py"),
        (Language::Go, "https://github.com/gorilla/mux", "go"),
        (Language::C, "https://github.com/antirez/sds", "c"),
        (Language::Cpp, "https://github.com/nlohmann/json", "hpp"),
        (Language::TypeScript, "https://github.com/sindresorhus/got", "ts"),
        (Language::JavaScript, "https://github.com/expressjs/express", "js"),
        (Language::Shell, "https://github.com/dylanaraps/pure-bash-bible", "sh"),
    ];

    fn corpus_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/language-corpus")
    }

    /// Clones `url` shallowly if it is not already cached. `None` means the
    /// clone could not happen (no git, no network), which is a skip.
    fn ensure_cloned(url: &str) -> Option<PathBuf> {
        let name = url.rsplit('/').next()?;
        let root = corpus_root();
        let path = root.join(name);
        if path.join(".git").exists() {
            return Some(path);
        }
        std::fs::create_dir_all(&root).ok()?;
        let status = std::process::Command::new("git")
            .args(["clone", "--depth", "1", "--quiet", url])
            .arg(&path)
            .status()
            .ok()?;
        status.success().then_some(path)
    }

    /// Walks a tree, applying `visit` to every file with `extension`, up to
    /// `limit` files.
    fn for_each_file(root: &Path, extension: &str, limit: usize, visit: &mut impl FnMut(&Path)) {
        fn walk(
            dir: &Path,
            extension: &str,
            seen: &mut usize,
            limit: usize,
            visit: &mut impl FnMut(&Path),
        ) {
            if *seen >= limit {
                return;
            }
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                if *seen >= limit {
                    return;
                }
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else { continue };
                if file_type.is_dir() {
                    if entry.file_name() == ".git" {
                        continue;
                    }
                    walk(&path, extension, seen, limit, visit);
                } else if path.extension().and_then(|e| e.to_str()) == Some(extension) {
                    *seen += 1;
                    visit(&path);
                }
            }
        }
        let mut seen = 0usize;
        walk(root, extension, &mut seen, limit, visit);
    }

    #[test]
    #[ignore = "clones repositories; run with --ignored"]
    fn real_repos_tokenize_without_columns_escaping_their_lines() {
        let mut checked = 0usize;
        let mut skipped = Vec::new();

        for (language, url, extension) in CORPUS {
            let Some(repo) = ensure_cloned(url) else {
                skipped.push(*url);
                continue;
            };
            let mut files = 0usize;
            let mut problems = Vec::new();
            for_each_file(&repo, extension, 400, &mut |path| {
                let Ok(text) = std::fs::read_to_string(path) else { return };
                files += 1;
                // Detection has to agree with the corpus, or the highlighter
                // is being handed the wrong grammar for the file.
                let detected = detect_language(path);
                if detected != *language {
                    problems.push(format!("{path:?} detected as {}", detected.name()));
                    return;
                }
                if let Err(problem) = tokenize_document(&text, *language) {
                    problems.push(format!("{path:?}: {problem}"));
                }
            });
            assert!(problems.is_empty(), "{} problems: {problems:#?}", language.name());
            println!("{:<12} {files} files checked", language.name());
            checked += files;
        }

        if !skipped.is_empty() {
            println!("skipped (no git or no network): {skipped:#?}");
        }
        if checked == 0 {
            println!("nothing was cloned; corpus checks skipped entirely");
        }
    }

    #[test]
    #[ignore = "clones repositories; run with --ignored"]
    fn real_repos_search_reports_line_numbers_that_exist() {
        // Searching real trees is where the walk meets vendored minified
        // JavaScript, CRLF files, generated code and files with no trailing
        // newline -- every shape the line counter can get wrong.
        let mut skipped = Vec::new();
        for (language, url, _) in CORPUS {
            let Some(repo) = ensure_cloned(url) else {
                skipped.push(*url);
                continue;
            };
            // A term common enough in any codebase to produce many hits
            // across many files.
            for query in ["return", "if", "the"] {
                let result = workspace_search::search(&repo, query);
                for hit in &result.hits {
                    let Ok(text) = std::fs::read_to_string(&hit.path) else { continue };
                    let line = text.lines().nth(hit.line_number - 1);
                    let line = line.unwrap_or_else(|| {
                        panic!(
                            "{}: {:?} reported line {} but the file has {} lines",
                            language.name(),
                            hit.path,
                            hit.line_number,
                            text.lines().count()
                        )
                    });
                    assert!(
                        line.to_lowercase().contains(query),
                        "{}: {:?} line {} does not contain {query:?}",
                        language.name(),
                        hit.path,
                        hit.line_number
                    );
                }
                println!("{:<12} {query:<8} {} hits verified", language.name(), result.hits.len());
            }
        }
        if !skipped.is_empty() {
            println!("skipped (no git or no network): {skipped:#?}");
        }
    }
}
