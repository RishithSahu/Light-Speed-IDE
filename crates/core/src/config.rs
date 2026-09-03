//! Configuration (specification section 10).
//!
//! ```text
//! defaults -> user configuration -> workspace configuration
//! ```
//!
//! Each layer overrides the one above it, and the result is an immutable
//! [`EffectiveConfig`] snapshot. Configuration is data (TOML), never code:
//! nothing in a config file is executed, and every value is range-checked with
//! a clear error rather than silently clamped.

use crate::error::ConfigError;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Schema version understood by this build.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct EditorConfig {
    pub tab_width: usize,
    /// Tab key inserts spaces rather than a tab character.
    pub insert_spaces: bool,
    /// Undo coalescing window (specification section 23).
    pub coalesce_window: Duration,
}

impl Default for EditorConfig {
    fn default() -> Self {
        EditorConfig {
            tab_width: 4,
            insert_spaces: true,
            coalesce_window: crate::history::DEFAULT_COALESCE_WINDOW,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppearanceConfig {
    pub font_family: String,
    pub font_size: f32,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
    pub show_line_numbers: bool,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        AppearanceConfig {
            font_family: "Cascadia Mono".to_string(),
            font_size: 14.0,
            line_height: 1.4,
            show_line_numbers: true,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct PerformanceConfig {
    /// Whether the performance overlay starts visible.
    pub overlay_visible: bool,
    /// Whether latency instrumentation records samples at all.
    pub instrumentation: bool,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        PerformanceConfig { overlay_visible: false, instrumentation: true }
    }
}

/// The merged, validated configuration handed to the rest of the application.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveConfig {
    pub schema_version: u32,
    pub editor: EditorConfig,
    pub appearance: AppearanceConfig,
    pub performance: PerformanceConfig,
    /// Files that contributed, in increasing priority order.
    pub sources: Vec<PathBuf>,
}

impl Default for EffectiveConfig {
    fn default() -> Self {
        EffectiveConfig {
            schema_version: SCHEMA_VERSION,
            editor: EditorConfig::default(),
            appearance: AppearanceConfig::default(),
            performance: PerformanceConfig::default(),
            sources: Vec::new(),
        }
    }
}

impl EffectiveConfig {
    /// Document settings derived from this configuration.
    pub fn document_settings(&self) -> crate::document::DocumentSettings {
        crate::document::DocumentSettings {
            tab_width: self.editor.tab_width,
            coalesce_window: self.editor.coalesce_window,
            insert_spaces: self.editor.insert_spaces,
        }
    }
}

/// One configuration file. Every field is optional: a layer only states what it
/// overrides. Unknown fields are rejected so a typo is reported instead of
/// silently doing nothing.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    schema_version: Option<u32>,
    editor: Option<EditorSection>,
    appearance: Option<AppearanceSection>,
    performance: Option<PerformanceSection>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditorSection {
    tab_width: Option<usize>,
    insert_spaces: Option<bool>,
    coalesce_window_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppearanceSection {
    font_family: Option<String>,
    font_size: Option<f32>,
    line_height: Option<f32>,
    show_line_numbers: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PerformanceSection {
    overlay_visible: Option<bool>,
    instrumentation: Option<bool>,
}

/// Loads and merges configuration layers, lowest priority first.
///
/// Missing files are skipped: a user with no config file is not an error.
pub fn load_layered(paths: &[PathBuf]) -> Result<EffectiveConfig, ConfigError> {
    let mut config = EffectiveConfig::default();
    for path in paths {
        if !path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(path)
            .map_err(|source| ConfigError::Io { path: path.clone(), source })?;
        let file: ConfigFile = toml::from_str(&text).map_err(|error| ConfigError::Syntax {
            path: path.clone(),
            message: error.message().to_string(),
        })?;
        apply(&mut config, file, path)?;
        config.sources.push(path.clone());
    }
    Ok(config)
}

/// Parses one configuration layer from text, for tests and for embedded
/// defaults.
pub fn parse_layer(text: &str, source: &Path) -> Result<EffectiveConfig, ConfigError> {
    let mut config = EffectiveConfig::default();
    let file: ConfigFile = toml::from_str(text).map_err(|error| ConfigError::Syntax {
        path: source.to_path_buf(),
        message: error.message().to_string(),
    })?;
    apply(&mut config, file, source)?;
    config.sources.push(source.to_path_buf());
    Ok(config)
}

fn apply(config: &mut EffectiveConfig, file: ConfigFile, path: &Path) -> Result<(), ConfigError> {
    let invalid = |field: &'static str, message: String| ConfigError::Invalid {
        path: path.to_path_buf(),
        field,
        message,
    };

    if let Some(version) = file.schema_version {
        if version != SCHEMA_VERSION {
            return Err(invalid(
                "schema_version",
                format!("this build understands schema version {SCHEMA_VERSION}, found {version}"),
            ));
        }
        config.schema_version = version;
    }

    if let Some(editor) = file.editor {
        if let Some(tab_width) = editor.tab_width {
            if !(1..=16).contains(&tab_width) {
                return Err(invalid("editor.tab_width", "must be between 1 and 16".into()));
            }
            config.editor.tab_width = tab_width;
        }
        if let Some(insert_spaces) = editor.insert_spaces {
            config.editor.insert_spaces = insert_spaces;
        }
        if let Some(window) = editor.coalesce_window_ms {
            if window > 10_000 {
                return Err(invalid("editor.coalesce_window_ms", "must be 10000 or less".into()));
            }
            config.editor.coalesce_window = Duration::from_millis(window);
        }
    }

    if let Some(appearance) = file.appearance {
        if let Some(family) = appearance.font_family {
            if family.trim().is_empty() {
                return Err(invalid("appearance.font_family", "must not be empty".into()));
            }
            config.appearance.font_family = family;
        }
        if let Some(size) = appearance.font_size {
            if !(6.0..=72.0).contains(&size) {
                return Err(invalid("appearance.font_size", "must be between 6 and 72".into()));
            }
            config.appearance.font_size = size;
        }
        if let Some(height) = appearance.line_height {
            if !(1.0..=3.0).contains(&height) {
                return Err(invalid(
                    "appearance.line_height",
                    "must be between 1.0 and 3.0".into(),
                ));
            }
            config.appearance.line_height = height;
        }
        if let Some(show) = appearance.show_line_numbers {
            config.appearance.show_line_numbers = show;
        }
    }

    if let Some(performance) = file.performance {
        if let Some(visible) = performance.overlay_visible {
            config.performance.overlay_visible = visible;
        }
        if let Some(instrumentation) = performance.instrumentation {
            config.performance.instrumentation = instrumentation;
        }
    }

    Ok(())
}

/// Standard configuration locations, lowest priority first.
///
/// * user: `%APPDATA%\LightSpeed\config.toml`, or `$XDG_CONFIG_HOME/lightspeed/config.toml`
/// * workspace: `<root>/.lightspeed/config.toml`
pub fn standard_paths(workspace_root: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(user) = user_config_path() {
        paths.push(user);
    }
    if let Some(root) = workspace_root {
        paths.push(root.join(".lightspeed").join("config.toml"));
    }
    paths
}

fn user_config_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(|base| PathBuf::from(base).join("LightSpeed").join("config.toml"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|base| base.join("lightspeed").join("config.toml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<EffectiveConfig, ConfigError> {
        parse_layer(text, Path::new("test-config.toml"))
    }

    #[test]
    fn defaults_are_usable_without_any_file() {
        let config = EffectiveConfig::default();
        assert_eq!(config.editor.tab_width, 4);
        assert_eq!(config.appearance.font_size, 14.0);
        assert!(config.performance.instrumentation);
        assert!(config.sources.is_empty());
    }

    #[test]
    fn a_layer_overrides_only_what_it_states() {
        let config = parse("[editor]\ntab_width = 2\n").unwrap();
        assert_eq!(config.editor.tab_width, 2);
        assert!(config.editor.insert_spaces, "untouched fields keep their default");
        assert_eq!(config.appearance.font_size, 14.0);
    }

    #[test]
    fn every_section_can_be_configured() {
        let config = parse(
            r#"
schema_version = 1

[editor]
tab_width = 8
insert_spaces = false
coalesce_window_ms = 250

[appearance]
font_family = "Consolas"
font_size = 16.5
line_height = 1.2
show_line_numbers = false

[performance]
overlay_visible = true
instrumentation = false
"#,
        )
        .unwrap();

        assert_eq!(config.editor.tab_width, 8);
        assert!(!config.editor.insert_spaces);
        assert_eq!(config.editor.coalesce_window, Duration::from_millis(250));
        assert_eq!(config.appearance.font_family, "Consolas");
        assert_eq!(config.appearance.font_size, 16.5);
        assert!(!config.appearance.show_line_numbers);
        assert!(config.performance.overlay_visible);
        assert!(!config.performance.instrumentation);
    }

    #[test]
    fn malformed_toml_is_rejected_clearly() {
        let error = parse("[editor\ntab_width = 4").unwrap_err();
        assert!(matches!(error, ConfigError::Syntax { .. }));
        assert!(error.to_string().contains("test-config.toml"));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let error = parse("[editor]\ntab_widht = 4\n").unwrap_err();
        assert!(matches!(error, ConfigError::Syntax { .. }), "got {error:?}");
    }

    #[test]
    fn out_of_range_values_are_rejected_with_the_field_name() {
        let error = parse("[editor]\ntab_width = 99\n").unwrap_err();
        match error {
            ConfigError::Invalid { field, .. } => assert_eq!(field, "editor.tab_width"),
            other => panic!("expected an invalid-value error, got {other:?}"),
        }

        let error = parse("[appearance]\nfont_size = 0.5\n").unwrap_err();
        assert!(matches!(error, ConfigError::Invalid { field: "appearance.font_size", .. }));
    }

    #[test]
    fn a_future_schema_version_is_rejected() {
        let error = parse("schema_version = 99\n").unwrap_err();
        assert!(matches!(error, ConfigError::Invalid { field: "schema_version", .. }));
    }

    #[test]
    fn layers_merge_in_priority_order() {
        let dir = std::env::temp_dir().join("lightspeed-config-layers");
        std::fs::create_dir_all(&dir).unwrap();
        let user = dir.join("user.toml");
        let workspace = dir.join("workspace.toml");
        std::fs::write(&user, "[editor]\ntab_width = 2\ninsert_spaces = false\n").unwrap();
        std::fs::write(&workspace, "[editor]\ntab_width = 8\n").unwrap();

        let config = load_layered(&[user.clone(), workspace.clone()]).unwrap();
        assert_eq!(config.editor.tab_width, 8, "workspace wins over user");
        assert!(!config.editor.insert_spaces, "user layer still applies");
        assert_eq!(config.sources, vec![user, workspace]);
    }

    #[test]
    fn missing_files_are_skipped() {
        let config = load_layered(&[PathBuf::from("no-such-config-file.toml")]).unwrap();
        assert_eq!(config, EffectiveConfig::default());
    }

    #[test]
    fn document_settings_come_from_the_editor_section() {
        let config = parse("[editor]\ntab_width = 3\ncoalesce_window_ms = 100\n").unwrap();
        let settings = config.document_settings();
        assert_eq!(settings.tab_width, 3);
        assert_eq!(settings.coalesce_window, Duration::from_millis(100));
    }

    #[test]
    fn standard_paths_end_with_the_workspace_layer() {
        let paths = standard_paths(Some(Path::new("/proj")));
        assert!(paths.last().unwrap().ends_with("config.toml"));
        assert!(paths.last().unwrap().to_string_lossy().contains(".lightspeed"));
    }
}
