//! Language identification (specification section 34).
//!
//! Stage 1 identifies a document's language for display and for future routing.
//! It does not analyze, highlight or index anything: syntax highlighting belongs
//! to the language layer, which arrives in the Foundation Stage. The
//! `analyze(DocumentSnapshot) -> TaskId` half of the contract is deliberately
//! absent until there is a scheduler to run it on.

use std::path::Path;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    PlainText,
    Rust,
    Python,
    C,
    Cpp,
    CSharp,
    Go,
    JavaScript,
    TypeScript,
    Json,
    Toml,
    Yaml,
    Markdown,
    Shell,
}

impl Language {
    /// Every language, so anything keyed on language can be tested for all of
    /// them rather than for whichever few a test author thought of. Adding a
    /// variant without adding it here is caught by
    /// `every_language_is_listed_in_all`.
    pub const ALL: &'static [Language] = &[
        Language::PlainText,
        Language::Rust,
        Language::Python,
        Language::C,
        Language::Cpp,
        Language::CSharp,
        Language::Go,
        Language::JavaScript,
        Language::TypeScript,
        Language::Json,
        Language::Toml,
        Language::Yaml,
        Language::Markdown,
        Language::Shell,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Language::PlainText => "Plain Text",
            Language::Rust => "Rust",
            Language::Python => "Python",
            Language::C => "C",
            Language::Cpp => "C++",
            Language::CSharp => "C#",
            Language::Go => "Go",
            Language::JavaScript => "JavaScript",
            Language::TypeScript => "TypeScript",
            Language::Json => "JSON",
            Language::Toml => "TOML",
            Language::Yaml => "YAML",
            Language::Markdown => "Markdown",
            Language::Shell => "Shell",
        }
    }
}

/// Identifies a document's language from its path.
pub fn detect_language(path: &Path) -> Language {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        match name.to_ascii_lowercase().as_str() {
            "cargo.lock" => return Language::Toml,
            "makefile" | "dockerfile" => return Language::PlainText,
            ".bashrc" | ".zshrc" | ".profile" => return Language::Shell,
            _ => {}
        }
    }
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return Language::PlainText;
    };
    match extension.to_ascii_lowercase().as_str() {
        "rs" => Language::Rust,
        "py" | "pyw" | "pyi" => Language::Python,
        "c" | "h" => Language::C,
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Language::Cpp,
        "cs" => Language::CSharp,
        "go" => Language::Go,
        "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
        "ts" | "tsx" | "mts" | "cts" => Language::TypeScript,
        "json" | "jsonc" => Language::Json,
        "toml" => Language::Toml,
        "yaml" | "yml" => Language::Yaml,
        "md" | "markdown" => Language::Markdown,
        "sh" | "bash" | "zsh" => Language::Shell,
        _ => Language::PlainText,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension() {
        assert_eq!(detect_language(Path::new("src/main.rs")), Language::Rust);
        assert_eq!(detect_language(Path::new("app.py")), Language::Python);
        assert_eq!(detect_language(Path::new("a/b/c.hpp")), Language::Cpp);
        assert_eq!(detect_language(Path::new("Program.cs")), Language::CSharp);
        assert_eq!(detect_language(Path::new("index.TSX")), Language::TypeScript);
    }

    #[test]
    fn detects_by_file_name() {
        assert_eq!(detect_language(Path::new("Cargo.lock")), Language::Toml);
        assert_eq!(detect_language(Path::new("/home/u/.bashrc")), Language::Shell);
    }

    #[test]
    fn every_language_is_listed_in_all() {
        // `ALL` is what the per-language tests iterate, so a variant missing
        // from it is a language that silently stops being tested. The match
        // is exhaustive, so adding a variant without adding it here fails to
        // compile rather than quietly shrinking the coverage.
        for language in Language::ALL {
            let named: &str = match language {
                Language::PlainText => "Plain Text",
                Language::Rust => "Rust",
                Language::Python => "Python",
                Language::C => "C",
                Language::Cpp => "C++",
                Language::CSharp => "C#",
                Language::Go => "Go",
                Language::JavaScript => "JavaScript",
                Language::TypeScript => "TypeScript",
                Language::Json => "JSON",
                Language::Toml => "TOML",
                Language::Yaml => "YAML",
                Language::Markdown => "Markdown",
                Language::Shell => "Shell",
            };
            assert_eq!(language.name(), named);
        }
        assert_eq!(Language::ALL.len(), 14, "a new variant needs adding to ALL");
    }

    #[test]
    fn every_language_has_a_distinct_display_name() {
        let mut seen = std::collections::HashSet::new();
        for language in Language::ALL {
            assert!(seen.insert(language.name()), "{} is named twice", language.name());
        }
    }

    #[test]
    fn every_language_except_plain_text_is_reachable_from_some_extension() {
        // A language nothing detects is a language that can never be opened,
        // however complete its keyword table or server config looks.
        const SAMPLES: &[(&str, Language)] = &[
            ("a.rs", Language::Rust),
            ("a.py", Language::Python),
            ("a.c", Language::C),
            ("a.cpp", Language::Cpp),
            ("a.cs", Language::CSharp),
            ("a.go", Language::Go),
            ("a.js", Language::JavaScript),
            ("a.ts", Language::TypeScript),
            ("a.json", Language::Json),
            ("a.toml", Language::Toml),
            ("a.yaml", Language::Yaml),
            ("a.md", Language::Markdown),
            ("a.sh", Language::Shell),
        ];
        for language in Language::ALL {
            if *language == Language::PlainText {
                continue;
            }
            assert!(
                SAMPLES.iter().any(|(name, expected)| {
                    expected == language && detect_language(Path::new(name)) == *language
                }),
                "{} has no extension that detects it",
                language.name()
            );
        }
    }

    #[test]
    fn unknown_files_are_plain_text() {
        assert_eq!(detect_language(Path::new("notes")), Language::PlainText);
        assert_eq!(detect_language(Path::new("archive.zip")), Language::PlainText);
        assert_eq!(detect_language(Path::new("")), Language::PlainText);
    }
}
