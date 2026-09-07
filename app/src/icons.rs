//! Icon glyphs.
//!
//! LightSpeed's chrome is a copy of Lapce's, and Lapce's chrome is icons: an
//! activity bar of them, one per file in the explorer, one per tab, several in
//! the status bar. This renderer draws solid rectangles and shaped text and
//! nothing else -- there is no image or SVG pipeline -- so icons arrive the
//! only way they can without inventing one: as glyphs in a bundled icon font,
//! shaped by the same cosmic-text path every other string in the window takes.
//!
//! Two fonts are bundled, for two different jobs:
//! - [Codicons](https://github.com/microsoft/vscode-codicons) (CC-BY-4.0,
//!   `app/assets/codicon.ttf`) draws the chrome itself: the activity bar,
//!   chevrons, tab close buttons, status bar glyphs -- the icon set VS Code
//!   draws and the one Lapce's own SVGs are visually derived from.
//! - [Material Design Icons](https://materialdesignicons.com) (Apache-2.0,
//!   `app/assets/material-icons.ttf`) draws file-type glyphs by extension --
//!   the same visual language `material-icon-theme` (the extension Lapce and
//!   VS Code both actually use for file icons) draws, since that extension
//!   ships its icons as SVGs this renderer has no pipeline to draw.
//!
//! Every codepoint below is taken from the font's own `materialdesignicons.css`
//! or `codicon.css` -- they are stable, and are what the glyphs are actually
//! mapped to inside the font.
//!
//! Note the one thing this file cannot do: it cannot make a glyph *mean*
//! something the shell has not built. `Extensions` and `Debug` exist here for
//! the activity bar's layout, not because there is a plugin host or a debug
//! adapter behind them.

/// Family name the chrome icon font registers itself under, used to select
/// it for a span of text (see `TextEngine::set_rich_text`).
pub const ICON_FAMILY: &str = "codicon";

/// The raw bytes of the bundled chrome icon font, compiled into the binary so
/// a missing file on disk can never leave the chrome iconless.
pub const ICON_FONT: &[u8] = include_bytes!("../assets/codicon.ttf");

/// Family name the Material Design Icons font registers itself under.
pub const MATERIAL_ICON_FAMILY: &str = "Material Design Icons";

/// The raw bytes of the bundled Material Design Icons font, used for
/// file-type glyphs.
pub const MATERIAL_ICON_FONT: &[u8] = include_bytes!("../assets/material-icons.ttf");

/// One drawable glyph: a codepoint plus which of the two bundled fonts it
/// lives in. `Icon` and `FileIcon` each convert into this, so call sites that
/// place a glyph (`RichText::icon`, the `ListRow`/`Tab` icon fields) do not
/// need to know or care which font any particular glyph came from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Glyph {
    pub ch: char,
    pub family: &'static str,
}

impl From<Icon> for Glyph {
    fn from(icon: Icon) -> Self {
        Glyph { ch: icon.glyph(), family: ICON_FAMILY }
    }
}

impl From<FileIcon> for Glyph {
    fn from(icon: FileIcon) -> Self {
        Glyph { ch: icon.glyph(), family: MATERIAL_ICON_FAMILY }
    }
}

/// Every icon the chrome draws.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Icon {
    // Activity bar, in Lapce's own order.
    Files,
    Search,
    SourceControl,
    Extensions,
    Debug,
    /// The dependency view -- which file takes which as input.
    TypeHierarchy,
    // Title bar.
    Menu,
    SettingsGear,
    Play,
    ArrowLeft,
    ArrowRight,
    // Explorer rows.
    ChevronRight,
    ChevronDown,
    Folder,
    FolderOpened,
    // Tabs.
    Close,
    CircleFilled,
    // Panels and status bar.
    Terminal,
    GitBranch,
    Error,
    Warning,
    LayoutSidebarLeft,
    LayoutPanel,
    SplitHorizontal,
    // The Search panel's own header and field controls.
    Refresh,
    ClearAll,
    NewFile,
    CollapseAll,
    Ellipsis,
    CaseSensitive,
    WholeWord,
    Regex,
    Replace,
}

/// Every icon, for the tests that check the whole table against the font.
#[cfg(test)]
pub const ALL_ICONS: [Icon; 33] = [
    Icon::Files,
    Icon::Search,
    Icon::SourceControl,
    Icon::Extensions,
    Icon::Debug,
    Icon::TypeHierarchy,
    Icon::Menu,
    Icon::SettingsGear,
    Icon::Play,
    Icon::ArrowLeft,
    Icon::ArrowRight,
    Icon::ChevronRight,
    Icon::ChevronDown,
    Icon::Folder,
    Icon::FolderOpened,
    Icon::Close,
    Icon::CircleFilled,
    Icon::Terminal,
    Icon::GitBranch,
    Icon::Error,
    Icon::Warning,
    Icon::LayoutSidebarLeft,
    Icon::LayoutPanel,
    Icon::SplitHorizontal,
    Icon::Refresh,
    Icon::ClearAll,
    Icon::NewFile,
    Icon::CollapseAll,
    Icon::Ellipsis,
    Icon::CaseSensitive,
    Icon::WholeWord,
    Icon::Regex,
    Icon::Replace,
];

impl Icon {
    /// The character this icon is drawn as, in [`ICON_FAMILY`].
    pub const fn glyph(self) -> char {
        match self {
            Icon::Files => '\u{eaf0}',
            Icon::Search => '\u{ea6d}',
            Icon::SourceControl => '\u{ea68}',
            Icon::Extensions => '\u{eae6}',
            Icon::Debug => '\u{eb91}',
            Icon::TypeHierarchy => '\u{ebb9}',
            Icon::Menu => '\u{eb94}',
            Icon::SettingsGear => '\u{eb51}',
            Icon::Play => '\u{eb2c}',
            Icon::ArrowLeft => '\u{ea9b}',
            Icon::ArrowRight => '\u{ea9c}',
            Icon::ChevronRight => '\u{eab6}',
            Icon::ChevronDown => '\u{eab4}',
            Icon::Folder => '\u{ea83}',
            Icon::FolderOpened => '\u{eaf7}',
            Icon::Close => '\u{ea76}',
            Icon::CircleFilled => '\u{ea71}',
            Icon::Terminal => '\u{ea85}',
            Icon::GitBranch => '\u{ec6f}',
            Icon::Error => '\u{ea87}',
            Icon::Warning => '\u{ea6c}',
            Icon::LayoutSidebarLeft => '\u{ebf3}',
            Icon::LayoutPanel => '\u{ebf2}',
            Icon::SplitHorizontal => '\u{eb56}',
            Icon::Refresh => '\u{eb37}',
            Icon::ClearAll => '\u{eabf}',
            Icon::NewFile => '\u{ea7f}',
            Icon::CollapseAll => '\u{eac5}',
            Icon::Ellipsis => '\u{ea7c}',
            Icon::CaseSensitive => '\u{eab1}',
            Icon::WholeWord => '\u{eb7e}',
            Icon::Regex => '\u{eb38}',
            Icon::Replace => '\u{eb3d}',
        }
    }
}

/// A file-type glyph, drawn from the bundled Material Design Icons font --
/// the visual language `material-icon-theme` (what Lapce and VS Code both
/// actually use for file icons) draws, mapped by extension the same way that
/// theme's own `icons.json` does.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FileIcon {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    React,
    Vue,
    Go,
    Java,
    Kotlin,
    Swift,
    C,
    Cpp,
    CSharp,
    Php,
    Ruby,
    Haskell,
    R,
    Fortran,
    Lua,
    Html,
    Css,
    Sass,
    Json,
    Xml,
    Markdown,
    Shell,
    Docker,
    Git,
    License,
    Database,
    Config,
    Lock,
    Text,
    Image,
    Svg,
    Pdf,
    Archive,
    Word,
    Excel,
    PowerPoint,
    /// No more specific glyph applies -- the generic document icon.
    Generic,
}

/// Every file icon, for the tests that check the whole table against the
/// font.
#[cfg(test)]
pub const ALL_FILE_ICONS: [FileIcon; 41] = [
    FileIcon::Rust,
    FileIcon::Python,
    FileIcon::JavaScript,
    FileIcon::TypeScript,
    FileIcon::React,
    FileIcon::Vue,
    FileIcon::Go,
    FileIcon::Java,
    FileIcon::Kotlin,
    FileIcon::Swift,
    FileIcon::C,
    FileIcon::Cpp,
    FileIcon::CSharp,
    FileIcon::Php,
    FileIcon::Ruby,
    FileIcon::Haskell,
    FileIcon::R,
    FileIcon::Fortran,
    FileIcon::Lua,
    FileIcon::Html,
    FileIcon::Css,
    FileIcon::Sass,
    FileIcon::Json,
    FileIcon::Xml,
    FileIcon::Markdown,
    FileIcon::Shell,
    FileIcon::Docker,
    FileIcon::Git,
    FileIcon::License,
    FileIcon::Database,
    FileIcon::Config,
    FileIcon::Lock,
    FileIcon::Text,
    FileIcon::Image,
    FileIcon::Svg,
    FileIcon::Pdf,
    FileIcon::Archive,
    FileIcon::Word,
    FileIcon::Excel,
    FileIcon::PowerPoint,
    FileIcon::Generic,
];

impl FileIcon {
    /// The character this icon is drawn as, in [`MATERIAL_ICON_FAMILY`].
    /// Codepoints taken from `@mdi/font`'s `materialdesignicons.css`.
    pub const fn glyph(self) -> char {
        match self {
            FileIcon::Rust => '\u{F1617}',
            FileIcon::Python => '\u{F0320}',
            FileIcon::JavaScript => '\u{F031E}',
            FileIcon::TypeScript => '\u{F06E6}',
            FileIcon::React => '\u{F0708}',
            FileIcon::Vue => '\u{F0844}',
            FileIcon::Go => '\u{F07D3}',
            FileIcon::Java => '\u{F0B37}',
            FileIcon::Kotlin => '\u{F1219}',
            FileIcon::Swift => '\u{F06E5}',
            FileIcon::C => '\u{F0671}',
            FileIcon::Cpp => '\u{F0672}',
            FileIcon::CSharp => '\u{F031B}',
            FileIcon::Php => '\u{F031F}',
            FileIcon::Ruby => '\u{F0D2D}',
            FileIcon::Haskell => '\u{F0C92}',
            FileIcon::R => '\u{F07D4}',
            FileIcon::Fortran => '\u{F121A}',
            FileIcon::Lua => '\u{F08B1}',
            FileIcon::Html => '\u{F031D}',
            FileIcon::Css => '\u{F031C}',
            FileIcon::Sass => '\u{F07EC}',
            FileIcon::Json => '\u{F0626}',
            FileIcon::Xml => '\u{F05C0}',
            FileIcon::Markdown => '\u{F0354}',
            FileIcon::Shell => '\u{F018D}',
            FileIcon::Docker => '\u{F0868}',
            FileIcon::Git => '\u{F02A2}',
            FileIcon::License => '\u{F0FC3}',
            FileIcon::Database => '\u{F01BC}',
            FileIcon::Config => '\u{F107C}',
            FileIcon::Lock => '\u{F1030}',
            FileIcon::Text => '\u{F09ED}',
            FileIcon::Image => '\u{F021F}',
            FileIcon::Svg => '\u{F0721}',
            FileIcon::Pdf => '\u{F0226}',
            FileIcon::Archive => '\u{F05C4}',
            FileIcon::Word => '\u{F022C}',
            FileIcon::Excel => '\u{F021B}',
            FileIcon::PowerPoint => '\u{F0227}',
            FileIcon::Generic => '\u{F0224}',
        }
    }

    /// This icon's characteristic color -- the whole point of
    /// `material-icon-theme`'s file icons is that a tree full of them reads
    /// by color and shape before anyone reads a single filename. Values are
    /// each language's own brand color where one is well established
    /// (matching GitHub's linguist colors, which `material-icon-theme` also
    /// tracks), a themed accent otherwise.
    pub const fn color(self) -> crate::theme::Color {
        use crate::theme::Color;
        match self {
            FileIcon::Rust => Color::rgb(0xde, 0xa5, 0x84),
            FileIcon::Python => Color::rgb(0x35, 0x72, 0xa5),
            FileIcon::JavaScript => Color::rgb(0xf1, 0xe0, 0x5a),
            FileIcon::TypeScript => Color::rgb(0x31, 0x78, 0xc6),
            FileIcon::React => Color::rgb(0x61, 0xda, 0xfb),
            FileIcon::Vue => Color::rgb(0x41, 0xb8, 0x83),
            FileIcon::Go => Color::rgb(0x00, 0xad, 0xd8),
            FileIcon::Java => Color::rgb(0xb0, 0x72, 0x19),
            FileIcon::Kotlin => Color::rgb(0xa9, 0x7b, 0xff),
            FileIcon::Swift => Color::rgb(0xf0, 0x51, 0x38),
            FileIcon::C => Color::rgb(0x55, 0x55, 0x55),
            FileIcon::Cpp => Color::rgb(0xf3, 0x4b, 0x7d),
            FileIcon::CSharp => Color::rgb(0x17, 0x86, 0x00),
            FileIcon::Php => Color::rgb(0x4f, 0x5d, 0x95),
            FileIcon::Ruby => Color::rgb(0xe3, 0x8c, 0x00),
            FileIcon::Haskell => Color::rgb(0x5e, 0x50, 0x86),
            FileIcon::R => Color::rgb(0x19, 0x8c, 0xe7),
            FileIcon::Fortran => Color::rgb(0x73, 0x4f, 0x96),
            FileIcon::Lua => Color::rgb(0x00, 0x00, 0x80),
            FileIcon::Html => Color::rgb(0xe3, 0x4c, 0x26),
            FileIcon::Css => Color::rgb(0x56, 0x3d, 0x7c),
            FileIcon::Sass => Color::rgb(0xcc, 0x66, 0x99),
            FileIcon::Json => Color::rgb(0xcb, 0xcb, 0x41),
            FileIcon::Xml => Color::rgb(0xe3, 0x7e, 0x00),
            FileIcon::Markdown => Color::rgb(0x51, 0x9a, 0xba),
            FileIcon::Shell => Color::rgb(0x89, 0xe0, 0x51),
            FileIcon::Docker => Color::rgb(0x0d, 0xb7, 0xed),
            FileIcon::Git => Color::rgb(0xf1, 0x50, 0x2f),
            FileIcon::License => Color::rgb(0xdc, 0xc6, 0x6a),
            FileIcon::Database => Color::rgb(0x4d, 0xb3, 0x3e),
            FileIcon::Config => Color::rgb(0x8f, 0x8f, 0x8f),
            FileIcon::Lock => Color::rgb(0x8f, 0x8f, 0x8f),
            FileIcon::Text => Color::rgb(0xc9, 0xc9, 0xc9),
            FileIcon::Image => Color::rgb(0xa0, 0x74, 0xc4),
            FileIcon::Svg => Color::rgb(0xff, 0xb1, 0x3b),
            FileIcon::Pdf => Color::rgb(0xe0, 0x3e, 0x3e),
            FileIcon::Archive => Color::rgb(0xc9, 0xa2, 0x27),
            FileIcon::Word => Color::rgb(0x41, 0xa5, 0xee),
            FileIcon::Excel => Color::rgb(0x21, 0xa3, 0x66),
            FileIcon::PowerPoint => Color::rgb(0xd2, 0x49, 0x25),
            FileIcon::Generic => Color::rgb(0x8f, 0x8f, 0x8f),
        }
    }
}

/// The icon a file gets in the explorer and on its tab, by extension --
/// `material-icon-theme`'s own mapping at the scale the shell can currently
/// justify: a language- or format-shaped glyph where one exists, the generic
/// file glyph otherwise.
pub fn icon_for_file(name: &str) -> FileIcon {
    // Whole-name matches first: a dotfile like `.gitignore` would otherwise
    // be parsed as extension `gitignore` of an empty stem, and a name with no
    // dot at all (`Dockerfile`, `LICENSE`) has no extension to match on.
    match name.to_ascii_lowercase().as_str() {
        "dockerfile" => return FileIcon::Docker,
        ".gitignore" | ".gitattributes" | ".gitmodules" => return FileIcon::Git,
        "license" | "license.txt" | "licence" => return FileIcon::License,
        _ => {}
    }
    let extension = name.rsplit_once('.').map(|(_, extension)| extension).unwrap_or("");
    match extension.to_ascii_lowercase().as_str() {
        "rs" => FileIcon::Rust,
        "py" | "pyw" | "pyi" => FileIcon::Python,
        "js" | "mjs" | "cjs" => FileIcon::JavaScript,
        "jsx" => FileIcon::React,
        "ts" | "mts" | "cts" => FileIcon::TypeScript,
        "tsx" => FileIcon::React,
        "vue" => FileIcon::Vue,
        "go" => FileIcon::Go,
        "java" => FileIcon::Java,
        "kt" | "kts" => FileIcon::Kotlin,
        "swift" => FileIcon::Swift,
        "c" | "h" => FileIcon::C,
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => FileIcon::Cpp,
        "cs" => FileIcon::CSharp,
        "php" => FileIcon::Php,
        "rb" => FileIcon::Ruby,
        "hs" => FileIcon::Haskell,
        "r" => FileIcon::R,
        "f" | "f90" | "f95" => FileIcon::Fortran,
        "lua" => FileIcon::Lua,
        "html" | "htm" => FileIcon::Html,
        "css" => FileIcon::Css,
        "scss" | "sass" | "less" => FileIcon::Sass,
        "json" | "jsonc" => FileIcon::Json,
        "xml" => FileIcon::Xml,
        "md" | "markdown" => FileIcon::Markdown,
        "sh" | "bash" | "zsh" | "fish" | "ps1" => FileIcon::Shell,
        "sql" => FileIcon::Database,
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" => FileIcon::Config,
        "lock" => FileIcon::Lock,
        "txt" | "log" => FileIcon::Text,
        "png" | "jpg" | "jpeg" | "gif" | "ico" | "bmp" | "webp" => FileIcon::Image,
        "svg" => FileIcon::Svg,
        "pdf" => FileIcon::Pdf,
        "zip" | "tar" | "gz" | "rar" | "7z" | "bz2" | "xz" => FileIcon::Archive,
        "doc" | "docx" => FileIcon::Word,
        "xls" | "xlsx" | "csv" => FileIcon::Excel,
        "ppt" | "pptx" => FileIcon::PowerPoint,
        _ => FileIcon::Generic,
    }
}

/// Font size for an icon that is the *entire* content of a `cell`-sized
/// button or rail slot -- the activity bar, a panel's own icon rail, the
/// title bar's menu/run/settings buttons.
///
/// About half the cell, which is close to how large VS Code and Lapce draw
/// their own activity-bar icons relative to the button around them; sizing
/// these icons at the ordinary UI text size (as if they were inline with a
/// filename) is what makes an icon-only cell look like it has a stray
/// character in it rather than an icon.
pub fn cell_icon_font_size(cell: f32) -> f32 {
    (cell * 0.5).max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_font_is_a_real_truetype_file() {
        // A 404 page saved to `codicon.ttf` would compile just as happily as
        // a font and then silently draw nothing, so check the magic number
        // rather than trusting the download that produced it.
        assert!(ICON_FONT.len() > 10_000, "the icon font is suspiciously small");
        assert_eq!(&ICON_FONT[..4], &[0x00, 0x01, 0x00, 0x00], "not a TrueType header");
    }

    #[test]
    fn every_icon_is_in_the_private_use_area() {
        // Codicon glyphs all live in the Unicode private use area; anything
        // outside it would be a transcription slip that renders as some
        // unrelated character from the text font instead.
        for icon in ALL_ICONS {
            let code = icon.glyph() as u32;
            assert!(
                (0xE000..=0xF8FF).contains(&code),
                "{icon:?} maps to U+{code:04X}, outside the private use area"
            );
        }
    }

    #[test]
    fn icons_are_distinct_where_they_need_to_be() {
        assert_ne!(Icon::ChevronRight.glyph(), Icon::ChevronDown.glyph());
        assert_ne!(Icon::Folder.glyph(), Icon::FolderOpened.glyph());
        assert_ne!(Icon::Files.glyph(), Icon::Search.glyph());
    }

    #[test]
    fn file_types_get_their_own_icons() {
        assert_eq!(icon_for_file("main.rs"), FileIcon::Rust);
        assert_eq!(icon_for_file("component.tsx"), FileIcon::React);
        assert_eq!(icon_for_file("app.vue"), FileIcon::Vue);
        assert_eq!(icon_for_file("Cargo.toml"), FileIcon::Config);
        assert_eq!(icon_for_file("README.md"), FileIcon::Markdown);
        assert_eq!(icon_for_file("package.json"), FileIcon::Json);
        assert_eq!(icon_for_file("Dockerfile"), FileIcon::Docker);
        assert_eq!(icon_for_file(".gitignore"), FileIcon::Git);
        assert_eq!(icon_for_file("LICENSE"), FileIcon::License, "no extension, matched by name");
        assert_eq!(icon_for_file("notes.unknownext"), FileIcon::Generic);
        assert_eq!(icon_for_file("Makefile"), FileIcon::Generic, "no extension, no special name");
    }

    #[test]
    fn a_cell_icon_is_a_real_fraction_of_its_cell_not_ordinary_text_size() {
        // Regression test: icon-only cells (the activity bar, panel rails,
        // title buttons) used to shape their glyph at the shared 14px UI
        // text size regardless of how big the cell around it was, so a 50px
        // activity-bar cell held what looked like a stray character rather
        // than an icon.
        assert_eq!(cell_icon_font_size(50.0), 25.0);
        assert_eq!(cell_icon_font_size(28.0), 14.0);
        assert!(cell_icon_font_size(1.0) >= 1.0, "never collapses to zero");
    }

    #[test]
    fn the_font_registers_under_the_family_name_the_shell_asks_for() {
        // If the bundled font's internal family name is not `ICON_FAMILY`,
        // every icon span silently falls back to the text font and the whole
        // activity bar draws as empty boxes -- an app that still passes every
        // other test in this file. This is the check that would catch it.
        let mut db = glyphon::fontdb::Database::new();
        db.load_font_data(ICON_FONT.to_vec());
        let families: Vec<String> = db
            .faces()
            .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
            .collect();
        assert!(
            families.iter().any(|family| family.eq_ignore_ascii_case(ICON_FAMILY)),
            "the bundled font calls itself {families:?}, not {ICON_FAMILY}"
        );
    }

    #[test]
    fn every_icon_shapes_to_a_real_glyph_rather_than_a_missing_one() {
        // A codepoint that is not actually mapped in the font shapes to glyph
        // 0 (.notdef) and draws as nothing or as a box. Shaping each icon the
        // same way the renderer does is the difference between "the icon
        // table compiles" and "the icons appear".
        let mut font_system = glyphon::FontSystem::new();
        font_system.db_mut().load_font_data(ICON_FONT.to_vec());
        let metrics = glyphon::Metrics::new(14.0, 20.0);
        let attrs = glyphon::Attrs::new().family(glyphon::Family::Name(ICON_FAMILY));

        for icon in ALL_ICONS {
            let mut buffer = glyphon::Buffer::new(&mut font_system, metrics);
            buffer.set_size(Some(256.0), Some(64.0));
            buffer.set_text(&icon.glyph().to_string(), &attrs, glyphon::Shaping::Advanced, None);
            buffer.shape_until_scroll(&mut font_system, false);
            let glyphs: Vec<_> = buffer.layout_runs().flat_map(|run| run.glyphs.to_vec()).collect();
            assert_eq!(glyphs.len(), 1, "{icon:?} should shape to exactly one glyph");
            assert_ne!(
                glyphs[0].glyph_id,
                0,
                "{icon:?} (U+{:04X}) shaped to .notdef -- it is not in the bundled font",
                icon.glyph() as u32
            );
            assert!(glyphs[0].w > 0.0, "{icon:?} shaped to a zero-width glyph");
        }
    }

    #[test]
    fn the_material_font_is_a_real_truetype_file() {
        assert!(
            MATERIAL_ICON_FONT.len() > 10_000,
            "the material icon font is suspiciously small"
        );
        assert_eq!(&MATERIAL_ICON_FONT[..4], &[0x00, 0x01, 0x00, 0x00], "not a TrueType header");
    }

    #[test]
    fn every_file_icon_is_a_supplementary_private_use_codepoint() {
        // Material Design Icons lives in the supplementary private use area
        // (U+F0000-FFFFD), not the BMP private use area Codicons uses --
        // a transcription slip that landed a codepoint in the wrong PUA would
        // shape against the wrong font (or nothing) at render time.
        for icon in ALL_FILE_ICONS {
            let code = icon.glyph() as u32;
            assert!(
                (0xF0000..=0xFFFFD).contains(&code),
                "{icon:?} maps to U+{code:04X}, outside the supplementary private use area"
            );
        }
    }

    #[test]
    fn the_material_font_registers_under_the_family_name_the_shell_asks_for() {
        let mut db = glyphon::fontdb::Database::new();
        db.load_font_data(MATERIAL_ICON_FONT.to_vec());
        let families: Vec<String> = db
            .faces()
            .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
            .collect();
        assert!(
            families.iter().any(|family| family.eq_ignore_ascii_case(MATERIAL_ICON_FAMILY)),
            "the bundled font calls itself {families:?}, not {MATERIAL_ICON_FAMILY}"
        );
    }

    #[test]
    fn every_file_icon_shapes_to_a_real_glyph_rather_than_a_missing_one() {
        let mut font_system = glyphon::FontSystem::new();
        font_system.db_mut().load_font_data(MATERIAL_ICON_FONT.to_vec());
        let metrics = glyphon::Metrics::new(14.0, 20.0);
        let attrs = glyphon::Attrs::new().family(glyphon::Family::Name(MATERIAL_ICON_FAMILY));

        for icon in ALL_FILE_ICONS {
            let mut buffer = glyphon::Buffer::new(&mut font_system, metrics);
            buffer.set_size(Some(256.0), Some(64.0));
            buffer.set_text(&icon.glyph().to_string(), &attrs, glyphon::Shaping::Advanced, None);
            buffer.shape_until_scroll(&mut font_system, false);
            let glyphs: Vec<_> = buffer.layout_runs().flat_map(|run| run.glyphs.to_vec()).collect();
            assert_eq!(glyphs.len(), 1, "{icon:?} should shape to exactly one glyph");
            assert_ne!(
                glyphs[0].glyph_id,
                0,
                "{icon:?} (U+{:04X}) shaped to .notdef -- it is not in the bundled font",
                icon.glyph() as u32
            );
            assert!(glyphs[0].w > 0.0, "{icon:?} shaped to a zero-width glyph");
        }
    }

    #[test]
    fn glyph_conversion_carries_the_right_font_family() {
        let chrome: Glyph = Icon::Search.into();
        assert_eq!(chrome.family, ICON_FAMILY);
        let file: Glyph = FileIcon::Rust.into();
        assert_eq!(file.family, MATERIAL_ICON_FAMILY);
    }
}
