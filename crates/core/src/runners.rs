//! Run support: detecting whether a language's toolchain is installed,
//! naming the winget package that installs it, and generating the shell
//! command(s) that compile and run a file.
//!
//! Deliberately independent of [`crate::language::Language`]: that enum
//! drives LSP server selection and is exhaustively matched by the tokenizer
//! and several tests, so it only carries languages a language *server*
//! exists for. Running code has no such dependency -- a [`RunLanguage`]
//! needs nothing more than "which extension", "which binary says this is
//! installed", "what to install if it is not", and "what to type into a
//! shell" -- so it is its own small, independent table rather than a
//! reason to widen `Language` for languages that would never get one.

use std::path::Path;

/// A language this editor can compile-and-run, given its toolchain.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum RunLanguage {
    Rust,
    Python,
    C,
    Cpp,
    Java,
    Go,
    JavaScript,
    TypeScript,
    CSharp,
    Ruby,
    Php,
    Perl,
    Zig,
    Lua,
    PowerShell,
}

impl RunLanguage {
    /// Every language, so the Run panel can list them all and nothing here
    /// can go stale by omission -- mirrors `Language::ALL`'s own reasoning.
    pub const ALL: &'static [RunLanguage] = &[
        RunLanguage::Rust,
        RunLanguage::Python,
        RunLanguage::C,
        RunLanguage::Cpp,
        RunLanguage::Java,
        RunLanguage::Go,
        RunLanguage::JavaScript,
        RunLanguage::TypeScript,
        RunLanguage::CSharp,
        RunLanguage::Ruby,
        RunLanguage::Php,
        RunLanguage::Perl,
        RunLanguage::Zig,
        RunLanguage::Lua,
        RunLanguage::PowerShell,
    ];

    pub const fn display_name(self) -> &'static str {
        match self {
            RunLanguage::Rust => "Rust",
            RunLanguage::Python => "Python",
            RunLanguage::C => "C",
            RunLanguage::Cpp => "C++",
            RunLanguage::Java => "Java",
            RunLanguage::Go => "Go",
            RunLanguage::JavaScript => "JavaScript",
            RunLanguage::TypeScript => "TypeScript",
            RunLanguage::CSharp => "C#",
            RunLanguage::Ruby => "Ruby",
            RunLanguage::Php => "PHP",
            RunLanguage::Perl => "Perl",
            RunLanguage::Zig => "Zig",
            RunLanguage::Lua => "Lua",
            RunLanguage::PowerShell => "PowerShell",
        }
    }

    /// The binary this language's toolchain is checked -- and, for most of
    /// them, actually run -- through. Answering `--version` at all (exit
    /// code aside; several tools return non-zero for it) is treated as
    /// "installed", the same tolerant bar `lsp.rs` uses for a server binary.
    pub const fn check_binary(self) -> &'static str {
        match self {
            RunLanguage::Rust => "rustc",
            RunLanguage::Python => "python",
            RunLanguage::C => "gcc",
            RunLanguage::Cpp => "g++",
            RunLanguage::Java => "javac",
            RunLanguage::Go => "go",
            // TypeScript's own dependency is Node -- there is no separate
            // `tsc`/`ts-node` toolchain to install; run_steps reaches it via
            // `npx`, which Node's install already provides.
            RunLanguage::JavaScript | RunLanguage::TypeScript => "node",
            RunLanguage::CSharp => "dotnet",
            RunLanguage::Ruby => "ruby",
            RunLanguage::Php => "php",
            RunLanguage::Perl => "perl",
            RunLanguage::Zig => "zig",
            RunLanguage::Lua => "lua",
            RunLanguage::PowerShell => "pwsh",
        }
    }

    /// The winget package id that installs this toolchain, run as
    /// `winget install --id <id> -e ...` -- `-e` for an exact match, so a
    /// short, common name like "python" cannot resolve to the wrong one of
    /// several similarly named packages in the catalog.
    pub const fn winget_id(self) -> &'static str {
        match self {
            RunLanguage::Rust => "Rustlang.Rustup",
            RunLanguage::Python => "Python.Python.3.12",
            // One compiler suite covers both: `gcc`/`g++` are the same
            // MinGW-w64 toolchain under two front-end names.
            RunLanguage::C | RunLanguage::Cpp => "BrechtSanders.WinLibs.POSIX.UCRT",
            RunLanguage::Java => "EclipseAdoptium.Temurin.21.JDK",
            RunLanguage::Go => "GoLang.Go",
            RunLanguage::JavaScript | RunLanguage::TypeScript => "OpenJS.NodeJS.LTS",
            RunLanguage::CSharp => "Microsoft.DotNet.SDK.8",
            RunLanguage::Ruby => "RubyInstallerTeam.Ruby.3.4",
            RunLanguage::Php => "PHP.PHP.8.4",
            RunLanguage::Perl => "StrawberryPerl.StrawberryPerl",
            RunLanguage::Zig => "zig.zig",
            RunLanguage::Lua => "DEVCOM.Lua",
            RunLanguage::PowerShell => "Microsoft.PowerShell",
        }
    }

    /// Which language a file runs as, if any -- by extension, the same
    /// signal `language::detect_language` uses, just a narrower table.
    pub fn from_extension(path: &Path) -> Option<RunLanguage> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match extension.as_str() {
            "rs" => RunLanguage::Rust,
            "py" | "pyw" => RunLanguage::Python,
            "c" => RunLanguage::C,
            "cc" | "cpp" | "cxx" => RunLanguage::Cpp,
            "java" => RunLanguage::Java,
            "go" => RunLanguage::Go,
            "js" | "mjs" | "cjs" => RunLanguage::JavaScript,
            "ts" | "mts" | "cts" => RunLanguage::TypeScript,
            "cs" => RunLanguage::CSharp,
            "rb" => RunLanguage::Ruby,
            "php" => RunLanguage::Php,
            "pl" => RunLanguage::Perl,
            "zig" => RunLanguage::Zig,
            "lua" => RunLanguage::Lua,
            "ps1" => RunLanguage::PowerShell,
            _ => return None,
        })
    }

    /// The shell command line(s) that compile and run `path`, one per
    /// terminal line rather than chained with `&&`/`;`.
    ///
    /// Not chaining is deliberate: this editor's own terminal tries `pwsh`,
    /// then Windows PowerShell 5.1, then `cmd` (see `app/src/terminal.rs`),
    /// and 5.1 has no `&&` operator at all. Sending each step as its own
    /// line works unchanged on whichever of the three actually started, at
    /// the cost that a failed compile still "runs" a stale or missing
    /// binary on the next line -- a shell error a person reads in a second,
    /// which is a fair trade for not silently misbehaving on half of
    /// Windows installs.
    pub fn run_steps(self, path: &Path) -> Vec<String> {
        let quoted = quote(path);
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Main");
        let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty());
        let dir_quoted = dir.map(quote).unwrap_or_else(|| ".".to_string());
        match self {
            RunLanguage::Rust => {
                let out = quote(&path.with_extension("exe"));
                vec![format!("rustc {quoted} -o {out}"), out]
            }
            RunLanguage::Python => vec![format!("python {quoted}")],
            RunLanguage::C => {
                let out = quote(&path.with_extension("exe"));
                vec![format!("gcc {quoted} -o {out}"), out]
            }
            RunLanguage::Cpp => {
                let out = quote(&path.with_extension("exe"));
                vec![format!("g++ {quoted} -o {out}"), out]
            }
            // `javac` compiles next to the source; `java` is then run from
            // that same directory so the class file it just wrote is on the
            // classpath. Assumes the public class name matches the file
            // name, which `javac` itself requires -- not an assumption this
            // adds on top of Java's own rule.
            RunLanguage::Java => {
                vec![format!("javac {quoted}"), format!("java -cp {dir_quoted} {stem}")]
            }
            RunLanguage::Go => vec![format!("go run {quoted}")],
            RunLanguage::JavaScript => vec![format!("node {quoted}")],
            // `npx -y tsx` runs a `.ts` file directly, downloading the tiny
            // `tsx` runner itself on first use rather than requiring a
            // separate `npm install -g typescript` step this editor cannot
            // one-click through winget (TypeScript has no winget package of
            // its own -- only Node does, hence `check_binary`/`winget_id`
            // both naming Node's).
            RunLanguage::TypeScript => vec![format!("npx -y tsx {quoted}")],
            // No single-file mode: `dotnet run` needs a `.csproj` in `dir`.
            // A loose `.cs` file with none reports a clear "no project"
            // error from `dotnet` itself rather than this pretending to
            // support something C#'s own tooling does not.
            RunLanguage::CSharp => vec![format!("dotnet run --project {dir_quoted}")],
            RunLanguage::Ruby => vec![format!("ruby {quoted}")],
            RunLanguage::Php => vec![format!("php {quoted}")],
            RunLanguage::Perl => vec![format!("perl {quoted}")],
            RunLanguage::Zig => vec![format!("zig run {quoted}")],
            RunLanguage::Lua => vec![format!("lua {quoted}")],
            RunLanguage::PowerShell => vec![format!("pwsh -NoLogo -File {quoted}")],
        }
    }
}

/// Wraps `path` in double quotes for a shell command line. Not shell
/// escaping in general -- a path containing a literal `"` would still break
/// -- but that is true of every path-quoting call site already in this
/// codebase (see `crates/core/src/editor.rs`'s git commands), and a
/// double-quoted path is what every candidate shell (`pwsh`, Windows
/// PowerShell, `cmd`) agrees on for one containing spaces.
fn quote(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn every_language_is_detected_from_a_real_extension() {
        for language in RunLanguage::ALL {
            let extension = match language {
                RunLanguage::Rust => "rs",
                RunLanguage::Python => "py",
                RunLanguage::C => "c",
                RunLanguage::Cpp => "cpp",
                RunLanguage::Java => "java",
                RunLanguage::Go => "go",
                RunLanguage::JavaScript => "js",
                RunLanguage::TypeScript => "ts",
                RunLanguage::CSharp => "cs",
                RunLanguage::Ruby => "rb",
                RunLanguage::Php => "php",
                RunLanguage::Perl => "pl",
                RunLanguage::Zig => "zig",
                RunLanguage::Lua => "lua",
                RunLanguage::PowerShell => "ps1",
            };
            let path = PathBuf::from(format!("example.{extension}"));
            assert_eq!(
                RunLanguage::from_extension(&path),
                Some(*language),
                "{extension} should detect as {language:?}"
            );
        }
    }

    #[test]
    fn an_unknown_extension_has_no_runner() {
        assert_eq!(RunLanguage::from_extension(Path::new("notes.md")), None);
        assert_eq!(RunLanguage::from_extension(Path::new("no_extension")), None);
    }

    #[test]
    fn every_language_has_a_distinct_display_name() {
        let mut seen = std::collections::HashSet::new();
        for language in RunLanguage::ALL {
            assert!(seen.insert(language.display_name()), "{} named twice", language.display_name());
        }
    }

    #[test]
    fn every_language_names_a_non_empty_winget_package() {
        for language in RunLanguage::ALL {
            assert!(!language.winget_id().is_empty());
        }
    }

    #[test]
    fn rust_compiles_then_runs_the_result() {
        let steps = RunLanguage::Rust.run_steps(Path::new("C:\\proj\\main.rs"));
        assert_eq!(steps.len(), 2);
        assert!(steps[0].starts_with("rustc "), "compiles first: {}", steps[0]);
        assert!(steps[0].contains("main.exe"), "names its own output: {}", steps[0]);
        assert_eq!(steps[1], "\"C:\\proj\\main.exe\"", "then runs exactly that output");
    }

    #[test]
    fn no_run_step_is_chained_with_and_and_or_a_semicolon() {
        // The whole reason each step is sent as its own terminal line: `&&`
        // does not exist in Windows PowerShell 5.1, one of the three shells
        // this editor's terminal may have actually started.
        for language in RunLanguage::ALL {
            for step in language.run_steps(Path::new("a/b.txt")) {
                assert!(!step.contains("&&"), "{language:?} step chains with &&: {step}");
            }
        }
    }

    #[test]
    fn python_and_go_and_javascript_are_a_single_interpret_step() {
        for (language, program) in [
            (RunLanguage::Python, "python"),
            (RunLanguage::Go, "go run"),
            (RunLanguage::JavaScript, "node"),
            (RunLanguage::TypeScript, "npx -y tsx"),
            (RunLanguage::Ruby, "ruby"),
            (RunLanguage::Php, "php"),
            (RunLanguage::Perl, "perl"),
            (RunLanguage::Zig, "zig run"),
            (RunLanguage::Lua, "lua"),
            (RunLanguage::PowerShell, "pwsh"),
        ] {
            let steps = language.run_steps(Path::new("script.ext"));
            assert_eq!(steps.len(), 1);
            assert!(steps[0].starts_with(program), "{language:?}: {}", steps[0]);
        }
    }

    #[test]
    fn typescript_and_javascript_share_nodes_toolchain_but_stay_distinct_languages() {
        assert_eq!(RunLanguage::TypeScript.check_binary(), RunLanguage::JavaScript.check_binary());
        assert_eq!(RunLanguage::TypeScript.winget_id(), RunLanguage::JavaScript.winget_id());
        assert_ne!(RunLanguage::TypeScript.display_name(), RunLanguage::JavaScript.display_name());
    }

    #[test]
    fn java_runs_from_the_files_own_directory_with_the_files_own_stem() {
        let steps = RunLanguage::Java.run_steps(Path::new("src/Hello.java"));
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], "javac \"src/Hello.java\"");
        assert!(steps[1].ends_with("Hello"), "runs the class named after the file: {}", steps[1]);
        assert!(steps[1].contains("-cp"), "on the compiled file's own directory: {}", steps[1]);
    }

    #[test]
    fn a_path_with_no_parent_directory_runs_from_the_current_one() {
        let steps = RunLanguage::CSharp.run_steps(Path::new("Program.cs"));
        assert_eq!(steps, vec!["dotnet run --project .".to_string()]);
    }
}
