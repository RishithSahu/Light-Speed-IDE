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
    JavaScript,
    TypeScript,
    Json,
    Toml,
    Yaml,
    Markdown,
    Shell,
}

impl Language {
    pub const fn name(self) -> &'static str {
        match self {
            Language::PlainText => "Plain Text",
            Language::Rust => "Rust",
            Language::Python => "Python",
            Language::C => "C",
            Language::Cpp => "C++",
            Language::CSharp => "C#",
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
    fn unknown_files_are_plain_text() {
        assert_eq!(detect_language(Path::new("notes")), Language::PlainText);
        assert_eq!(detect_language(Path::new("archive.zip")), Language::PlainText);
        assert_eq!(detect_language(Path::new("")), Language::PlainText);
    }
}
