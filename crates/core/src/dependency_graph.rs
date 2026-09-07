//! Which file pulls in which, read straight out of the source.
//!
//! # Why this is hand-written and the layout is not
//!
//! The *drawing* of this graph is `layout-rs`'s job (see
//! `app/src/depgraph.rs`): hierarchical layout with routed, arrow-headed
//! edges is a genuinely hard, well-solved problem, and re-deriving it here
//! would be the wrong kind of work.
//!
//! Reading imports is the opposite. The library answer is Tree-sitter, and it
//! was measured rather than assumed: `tree-sitter` plus grammars for three
//! languages pulls in 53 dependency lines and needs a C toolchain to build.
//! What it buys over the code below is the ability to parse import syntax
//! that spans lines, hides inside macros, or is generated -- none of which
//! changes the answer for the overwhelmingly common case, which is a single
//! line at the top of a file with a literal path in it:
//!
//! ```text
//! #include "raster.h"        mod parser;
//! import ./widget            from .models import User
//! ```
//!
//! So: a line scanner, no dependency, no C compiler, and the same honesty
//! about its limits that `crate::highlight` has about not being a parser.
//!
//! # What it does not do
//!
//! No module resolution beyond "does a plausible file exist next to this
//! one". A reference that names a package (`serde`, `numpy`, `react`) rather
//! than a path in this workspace is dropped, not guessed at -- an edge to a
//! file that is not there would be worse than a missing one. Nothing here
//! knows about build systems, include paths, aliases (`@/components`),
//! re-exports, or conditional compilation.

use crate::language::{detect_language, Language};
use std::path::{Path, PathBuf};

/// Directories never worth walking, matching [`crate::workspace_search`]'s
/// list so "what counts as source" has one answer in this crate.
const SKIP_DIRECTORIES: &[&str] = &[".git", "target", "node_modules", ".svn", ".hg", "dist"];

/// Ceiling on files visited, so pointing this at a huge tree cannot turn one
/// click into an unbounded walk. The graph stops being readable long before
/// this anyway.
const MAX_FILES: usize = 4_000;

/// A file that imports, and a file it imports, both relative to the
/// workspace root so nothing downstream has to carry absolute paths around.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// Every source file found, and every resolved reference between them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DependencyGraph {
    /// Files that take part in at least one edge, plus every scanned file
    /// that resolved nothing, so an isolated file still appears.
    pub files: Vec<PathBuf>,
    pub edges: Vec<Edge>,
    /// True when the walk stopped at [`MAX_FILES`] rather than running out
    /// of files, so the view can say so instead of implying completeness.
    pub truncated: bool,
}

impl DependencyGraph {
    /// Files nothing else imports: the roots a top-down layout starts from.
    pub fn roots(&self) -> Vec<&PathBuf> {
        self.files
            .iter()
            .filter(|file| !self.edges.iter().any(|edge| &&edge.to == file))
            .collect()
    }

    /// Files that import nothing themselves -- the leaves of the picture.
    pub fn is_leaf(&self, file: &Path) -> bool {
        !self.edges.iter().any(|edge| edge.from == file)
    }
}

/// Pulls the raw reference strings out of one file's text.
///
/// Returns what the source literally names (`"./widget"`, `"parser"`,
/// `"raster.h"`); turning those into paths is [`resolve`]'s problem.
pub fn extract_references(text: &str, language: Language) -> Vec<String> {
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        match language {
            Language::Rust => {
                // `mod parser;` names a sibling file directly.
                let module = line
                    .strip_prefix("mod ")
                    .or_else(|| line.strip_prefix("pub mod "))
                    .and_then(|rest| rest.strip_suffix(';'));
                if let Some(name) = module {
                    push_clean(&mut found, name);
                    continue;
                }
                // `use crate::parser::Token;` is the edge that actually
                // matters. Taking only `mod` made every Rust graph a star:
                // `mod` appears solely in a crate root, so main.rs pointed at
                // every module and no module pointed at any other, which is
                // the module *declaration* tree, not "which file takes which
                // as input". A `use` path names a module, and in the flat
                // layout Rust files overwhelmingly use, that module is a
                // file beside the one naming it. Where it is not, `resolve`
                // finds nothing and no edge is invented.
                let used = line
                    .strip_prefix("use ")
                    .or_else(|| line.strip_prefix("pub use "))
                    .map(|rest| rest.trim_end_matches(';'));
                if let Some(path) = used {
                    for name in rust_use_modules(path) {
                        push_clean(&mut found, &name);
                    }
                }
            }
            Language::Python => {
                if let Some(rest) = line.strip_prefix("from ") {
                    if let Some((module, imported)) = rest.split_once(" import") {
                        let module = module.trim();
                        if module.chars().all(|c| c == '.') {
                            // `from . import b` -- the package is the current
                            // directory and it is the *imported* name that
                            // picks the module out of it, so the reference is
                            // the two joined: `.b`.
                            for name in imported.split(',') {
                                let name = name.split(" as ").next().unwrap_or(name).trim();
                                if !name.is_empty() {
                                    push_clean(&mut found, &format!("{module}{name}"));
                                }
                            }
                        } else {
                            push_clean(&mut found, module);
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("import ") {
                    // `import a, b` names two modules.
                    for part in rest.split(',') {
                        let part = part.split(" as ").next().unwrap_or(part);
                        push_clean(&mut found, part);
                    }
                }
            }
            Language::JavaScript | Language::TypeScript => {
                // Both `import x from "./y"` and `require("./y")` end in a
                // quoted specifier, which is the only part that matters.
                if line.starts_with("import ") || line.contains("require(") {
                    if let Some(specifier) = quoted(line) {
                        push_clean(&mut found, &specifier);
                    }
                } else if let Some(rest) = line.strip_prefix("export ") {
                    if rest.contains(" from ") {
                        if let Some(specifier) = quoted(line) {
                            push_clean(&mut found, &specifier);
                        }
                    }
                }
            }
            Language::C | Language::Cpp | Language::CSharp => {
                // Only `"..."` includes: `<...>` is a system header by
                // definition and will never be a file in this workspace.
                if line.starts_with("#include") {
                    if let Some(specifier) = quoted(line) {
                        push_clean(&mut found, &specifier);
                    }
                }
            }
            Language::Go => {
                // Go imports name packages (directories), not files, and
                // resolving them needs the module path from `go.mod`.
                // Skipped rather than guessed at.
            }
            _ => {}
        }
    }
    found
}

fn push_clean(found: &mut Vec<String>, raw: &str) {
    let cleaned = raw.trim().trim_matches(|c| c == '"' || c == '\'' || c == ';');
    if !cleaned.is_empty() && !found.iter().any(|existing| existing == cleaned) {
        found.push(cleaned.to_string());
    }
}

/// The module names a Rust `use` path refers to.
///
/// `crate::`, `super::` and `self::` are prefixes, not modules, so the name
/// that matters is the one after them; anything else leads with the module
/// (or external crate) itself. A braced group right after the prefix --
/// `use crate::{layout, theme};` -- names several at once.
///
/// External crates come back too (`std`, `ls_core`), and that is fine:
/// nothing in the workspace resolves to them, so they produce no edge.
fn rust_use_modules(path: &str) -> Vec<String> {
    let path = path.trim();
    let mut segments = path.split("::");
    let Some(first) = segments.next().map(str::trim) else { return Vec::new() };
    let head = match first {
        "crate" | "super" | "self" => match segments.next().map(str::trim) {
            Some(next) => next,
            None => return Vec::new(),
        },
        other => other,
    };
    if let Some(group) = head.strip_prefix('{') {
        return group
            .trim_end_matches('}')
            .split(',')
            .map(|name| name.split("::").next().unwrap_or(name).trim().to_string())
            .filter(|name| !name.is_empty() && name != "self")
            .collect();
    }
    if head.is_empty() {
        return Vec::new();
    }
    vec![head.to_string()]
}

/// The contents of the first `"..."` on a line.
fn quoted(line: &str) -> Option<String> {
    let (open, quote) = line.char_indices().find(|(_, c)| *c == '"' || *c == '\'')?;
    let rest = &line[open + quote.len_utf8()..];
    let close = rest.find(quote)?;
    Some(rest[..close].to_string())
}

/// Turns one reference into a workspace-relative path, if it names a file
/// that is actually there.
///
/// `from` is the importing file, relative to the root; the returned path is
/// relative to the root too.
pub fn resolve(root: &Path, from: &Path, reference: &str, language: Language) -> Option<PathBuf> {
    let directory = from.parent().unwrap_or(Path::new(""));

    // Candidate stems, in the order they should win.
    let mut stems: Vec<PathBuf> = Vec::new();
    match language {
        Language::Python => {
            // `.models` / `..models` are relative; each leading dot climbs a
            // level. Dots inside the name are package separators.
            let dots = reference.chars().take_while(|c| *c == '.').count();
            let tail = reference.trim_start_matches('.').replace('.', "/");
            let mut base = directory.to_path_buf();
            for _ in 1..dots.max(1) {
                base = base.parent().map(Path::to_path_buf).unwrap_or_default();
            }
            if !tail.is_empty() {
                stems.push(base.join(&tail));
            }
            // A bare `import foo` may still be a sibling module.
            if dots == 0 {
                stems.push(directory.join(&tail));
            }
        }
        _ => {
            let cleaned = reference.trim_start_matches("./");
            if let Some(up) = cleaned.strip_prefix("../") {
                let base = directory.parent().map(Path::to_path_buf).unwrap_or_default();
                stems.push(base.join(up));
            } else {
                stems.push(directory.join(cleaned));
            }
        }
    }

    let extensions: &[&str] = match language {
        Language::Rust => &["rs"],
        Language::Python => &["py", "pyi"],
        Language::JavaScript => &["js", "jsx", "mjs", "cjs", "ts", "tsx"],
        Language::TypeScript => &["ts", "tsx", "d.ts", "js", "jsx"],
        Language::C | Language::Cpp | Language::CSharp => &["h", "hpp", "hh", "c", "cpp", "cs"],
        _ => &[],
    };

    for stem in stems {
        // The reference may already carry its own extension (`raster.h`).
        if root.join(&stem).is_file() {
            return Some(stem);
        }
        for extension in extensions {
            let candidate = stem.with_extension(extension);
            if root.join(&candidate).is_file() {
                return Some(candidate);
            }
            // `mod parser;` can mean `parser/mod.rs`; `./widget` can mean
            // `widget/index.ts`; `import pkg` can mean `pkg/__init__.py`.
            let inner = match language {
                Language::Rust => stem.join("mod").with_extension(extension),
                Language::Python => stem.join("__init__").with_extension(extension),
                _ => stem.join("index").with_extension(extension),
            };
            if root.join(&inner).is_file() {
                return Some(inner);
            }
        }
    }
    None
}

/// Walks `root` and builds the graph.
///
/// Runs on a worker (see `EditorCore::request_dependency_graph`), not the
/// interactive thread: this reads every source file in the workspace.
pub fn build(root: &Path) -> DependencyGraph {
    let mut graph = DependencyGraph::default();
    let mut sources: Vec<PathBuf> = Vec::new();
    collect(root, root, &mut sources, &mut graph.truncated);
    sources.sort();

    let mut buffer = String::new();
    for relative in &sources {
        let language = detect_language(relative);
        buffer.clear();
        let Ok(text) = std::fs::read_to_string(root.join(relative)) else { continue };
        buffer.push_str(&text);

        for reference in extract_references(&buffer, language) {
            let Some(target) = resolve(root, relative, &reference, language) else { continue };
            if &target == relative {
                // A file importing itself is either a parse artefact or a
                // self-referential module declaration; either way it is not
                // an edge worth drawing.
                continue;
            }
            let edge = Edge { from: relative.clone(), to: target };
            if !graph.edges.contains(&edge) {
                graph.edges.push(edge);
            }
        }
    }

    // Every file that ended up in an edge, plus every scanned source, so an
    // unconnected file is still visible rather than silently absent.
    graph.files = sources;
    for edge in &graph.edges {
        if !graph.files.contains(&edge.to) {
            graph.files.push(edge.to.clone());
        }
    }
    graph.files.sort();
    graph.files.dedup();
    graph
}

/// Depth-first walk collecting workspace-relative source paths.
fn collect(root: &Path, directory: &Path, out: &mut Vec<PathBuf>, truncated: &mut bool) {
    if out.len() >= MAX_FILES {
        *truncated = true;
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    for entry in entries.flatten() {
        if out.len() >= MAX_FILES {
            *truncated = true;
            return;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else { continue };
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIP_DIRECTORIES.contains(&name.as_ref()) || name.starts_with('.') {
                continue;
            }
            collect(root, &path, out, truncated);
        } else if file_type.is_file() {
            // Only languages something can actually be extracted from.
            let language = detect_language(&path);
            let understood = matches!(
                language,
                Language::Rust
                    | Language::Python
                    | Language::JavaScript
                    | Language::TypeScript
                    | Language::C
                    | Language::Cpp
                    | Language::CSharp
            );
            if !understood {
                continue;
            }
            if let Ok(relative) = path.strip_prefix(root) {
                out.push(relative.to_path_buf());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("lightspeed-depgraph-{name}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn rust_module_declarations_are_references() {
        let refs = extract_references("mod parser;\npub mod lexer;\nfn main() {}\n", Language::Rust);
        assert_eq!(refs, vec!["parser", "lexer"]);
    }

    #[test]
    fn rust_use_paths_name_the_module_they_reach_through() {
        // `use` is where a Rust file says what it actually depends on;
        // `mod` alone made every graph a star out of the crate root.
        let refs = extract_references(
            "use crate::parser::Thing;\nuse super::lexer::Token;\nuse self::inner::X;\n",
            Language::Rust,
        );
        assert_eq!(refs, vec!["parser", "lexer", "inner"]);
    }

    #[test]
    fn a_braced_use_names_every_module_in_the_group() {
        let refs = extract_references("use crate::{layout, theme::Color};\n", Language::Rust);
        assert_eq!(refs, vec!["layout", "theme"]);
    }

    #[test]
    fn external_crates_come_back_but_resolve_to_nothing_in_the_workspace() {
        // Harmless by construction: `resolve` only returns a path for a file
        // that exists, so `std` and a dependency crate produce no edge.
        let refs = extract_references("use std::fmt;\nuse ls_core::editor::X;\n", Language::Rust);
        assert_eq!(refs, vec!["std", "ls_core"]);
        let root = std::env::temp_dir();
        assert!(resolve(&root, Path::new("a.rs"), "std", Language::Rust).is_none());
    }

    #[test]
    fn python_imports_of_both_shapes_are_found() {
        let refs = extract_references(
            "from .models import User\nimport helpers\nfrom ..shared import x\n",
            Language::Python,
        );
        assert_eq!(refs, vec![".models", "helpers", "..shared"]);
    }

    #[test]
    fn javascript_import_and_require_specifiers_are_found() {
        let refs = extract_references(
            "import Widget from './widget';\nconst y = require(\"./util\");\n",
            Language::JavaScript,
        );
        assert_eq!(refs, vec!["./widget", "./util"]);
    }

    #[test]
    fn c_quoted_includes_are_found_and_system_headers_are_not() {
        let refs =
            extract_references("#include \"raster.h\"\n#include <stdio.h>\n", Language::C);
        assert_eq!(refs, vec!["raster.h"], "a system header can never be a workspace file");
    }

    #[test]
    fn a_reference_to_a_package_rather_than_a_file_resolves_to_nothing() {
        let root = scratch("package");
        std::fs::write(root.join("app.py"), "").unwrap();
        assert_eq!(
            resolve(&root, Path::new("app.py"), "numpy", Language::Python),
            None,
            "an installed package is not a file in this workspace"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rust_module_resolves_to_either_the_file_or_the_directory_form() {
        let root = scratch("rustmod");
        std::fs::write(root.join("main.rs"), "").unwrap();
        std::fs::write(root.join("parser.rs"), "").unwrap();
        std::fs::create_dir_all(root.join("lexer")).unwrap();
        std::fs::write(root.join("lexer/mod.rs"), "").unwrap();

        assert_eq!(
            resolve(&root, Path::new("main.rs"), "parser", Language::Rust),
            Some(PathBuf::from("parser.rs"))
        );
        assert_eq!(
            resolve(&root, Path::new("main.rs"), "lexer", Language::Rust),
            Some(PathBuf::from("lexer").join("mod.rs"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_relative_javascript_import_resolves_across_directories() {
        let root = scratch("jsrel");
        std::fs::create_dir_all(root.join("src/ui")).unwrap();
        std::fs::write(root.join("src/ui/app.js"), "").unwrap();
        std::fs::write(root.join("src/util.js"), "").unwrap();

        assert_eq!(
            resolve(&root, Path::new("src/ui/app.js"), "../util", Language::JavaScript),
            Some(PathBuf::from("src").join("util.js"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn building_a_small_tree_finds_its_edges_and_its_leaves() {
        let root = scratch("build");
        std::fs::write(root.join("main.rs"), "mod parser;\nmod lexer;\n").unwrap();
        std::fs::write(root.join("parser.rs"), "mod token;\n").unwrap();
        std::fs::write(root.join("lexer.rs"), "").unwrap();
        std::fs::write(root.join("token.rs"), "").unwrap();

        let graph = build(&root);
        assert_eq!(graph.files.len(), 4);
        assert!(graph.edges.contains(&Edge {
            from: PathBuf::from("main.rs"),
            to: PathBuf::from("parser.rs")
        }));
        assert!(graph.edges.contains(&Edge {
            from: PathBuf::from("parser.rs"),
            to: PathBuf::from("token.rs")
        }));

        // `main.rs` is imported by nothing: it is the root of the picture.
        assert_eq!(graph.roots(), vec![&PathBuf::from("main.rs")]);
        // `token.rs` and `lexer.rs` import nothing: they are the leaves.
        assert!(graph.is_leaf(Path::new("token.rs")));
        assert!(graph.is_leaf(Path::new("lexer.rs")));
        assert!(!graph.is_leaf(Path::new("main.rs")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cycle_does_not_hang_or_duplicate_edges() {
        // Mutually importing files are legal in several languages and must
        // not turn the walk into a loop.
        let root = scratch("cycle");
        std::fs::write(root.join("a.py"), "from . import b\n").unwrap();
        std::fs::write(root.join("b.py"), "from . import a\n").unwrap();

        let graph = build(&root);
        assert_eq!(graph.edges.len(), 2, "one edge each way, no more: {:?}", graph.edges);
        assert!(graph.roots().is_empty(), "everything in a cycle has an importer");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skipped_directories_are_not_walked() {
        let root = scratch("skip");
        std::fs::write(root.join("main.rs"), "").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/generated.rs"), "").unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("node_modules/pkg/index.js"), "").unwrap();

        let graph = build(&root);
        assert_eq!(graph.files, vec![PathBuf::from("main.rs")]);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod real_repo {
    use super::*;

    /// Points the extractor at this repository and prints what it found.
    /// `cargo test -p ls-core -- --ignored --nocapture on_this_repo`
    #[test]
    #[ignore = "diagnostic against the real workspace"]
    fn on_this_repo() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let started = std::time::Instant::now();
        let graph = build(&root);
        println!(
            "{} files, {} edges in {:?} (truncated: {})",
            graph.files.len(),
            graph.edges.len(),
            started.elapsed(),
            graph.truncated
        );
        println!("roots: {:?}", graph.roots());
        for edge in graph.edges.iter().take(15) {
            println!("  {} -> {}", edge.from.display(), edge.to.display());
        }
    }
}
