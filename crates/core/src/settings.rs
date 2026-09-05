//! Every knob the application exposes, and the value each one currently has.
//!
//! # One table, not a struct per feature
//!
//! [`EffectiveConfig`](crate::config) is a fixed schema read once at startup:
//! good for "what did this build boot with", useless for a settings screen,
//! which has to enumerate what exists, describe it in a sentence, search it,
//! say whether a value is still the default, and write a changed one back.
//! So the settings live in one table of [`SettingDescriptor`]s. Adding a
//! setting is one row; the screen, the search, the file format and the
//! validation all pick it up without another line of code.
//!
//! # Nothing here can break the application
//!
//! That is the whole reason values go through [`Settings::set`] rather than
//! into a free-form map. Every setting declares a [`SettingKind`] that bounds
//! it: a number states its range and is clamped into it, a choice states its
//! options and refuses anything else, text states a length. A settings file
//! hand-edited into nonsense, or a field someone typed 900000 into, yields a
//! usable application rather than a broken one.
//!
//! The other half of that promise is what is *absent*. Word wrap, for one:
//! the editor maps one document line to one laid-out row and hit-testing
//! depends on it, so a wrap toggle would be a setting whose only effect is
//! to put the caret in the wrong place. It is better not to offer a knob than
//! to offer one that breaks the thing it is attached to.

use std::collections::BTreeMap;

/// What a setting may hold, and the bounds it is held within.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SettingKind {
    Bool,
    /// A whole number, clamped to `min..=max`.
    Integer { min: i64, max: i64 },
    /// A fraction, clamped to `min..=max`.
    Float { min: f64, max: f64 },
    /// Free text, cut to `max_len` characters.
    Text { max_len: usize },
    /// One of a fixed list. Anything else is refused.
    Choice(&'static [&'static str]),
}

/// A setting's current value.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingValue {
    Bool(bool),
    Integer(i64),
    Float(f64),
    Text(String),
}

impl SettingValue {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            SettingValue::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_integer(&self) -> Option<i64> {
        match self {
            SettingValue::Integer(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            SettingValue::Float(value) => Some(*value),
            SettingValue::Integer(value) => Some(*value as f64),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            SettingValue::Text(value) => Some(value),
            _ => None,
        }
    }

    /// How the value is written to the settings file and shown in a field.
    pub fn to_text(&self) -> String {
        match self {
            SettingValue::Bool(value) => value.to_string(),
            SettingValue::Integer(value) => value.to_string(),
            SettingValue::Float(value) => {
                // Trimmed so 1.4 does not read back as "1.4000000000000001".
                let text = format!("{value:.4}");
                text.trim_end_matches('0').trim_end_matches('.').to_string()
            }
            SettingValue::Text(value) => value.clone(),
        }
    }
}

/// Whether a change shows up straight away.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Applies {
    /// Takes effect on the next frame.
    Immediately,
    /// Read when the thing it configures is next created.
    OnRestart,
}

/// One knob: what it is called, what it means, and what it may be.
#[derive(Clone, Copy, Debug)]
pub struct SettingDescriptor {
    /// Dotted identifier, as written in the settings file.
    pub key: &'static str,
    /// The screen's left-hand grouping.
    pub section: &'static str,
    /// Shown in bold, after the section.
    pub title: &'static str,
    /// The sentence under the title.
    pub description: &'static str,
    pub kind: SettingKind,
    /// The value used when the file says nothing, written the same way the
    /// file writes it. Kept as text so the table stays a plain constant and
    /// one parser serves both.
    pub default: &'static str,
    pub applies: Applies,
}

impl SettingDescriptor {
    /// The default, parsed. Every entry in [`SETTINGS`] is checked by
    /// `every_default_is_valid_for_its_own_kind`, so this cannot be `None`
    /// for a table that compiles and passes its tests.
    pub fn default_value(&self) -> SettingValue {
        parse(self.kind, self.default).unwrap_or(SettingValue::Bool(false))
    }
}

/// Sections, in the order the screen lists them.
pub const SECTIONS: &[&str] =
    &["Text Editor", "Workbench", "Terminal", "Dependency View", "Performance"];

/// Every setting the application exposes.
pub const SETTINGS: &[SettingDescriptor] = &[
    // --- Text Editor ---
    SettingDescriptor {
        key: "editor.fontSize",
        section: "Text Editor",
        title: "Font Size",
        description: "Controls the font size in pixels.",
        kind: SettingKind::Integer { min: 6, max: 48 },
        default: "14",
        applies: Applies::Immediately,
    },
    SettingDescriptor {
        key: "editor.fontFamily",
        section: "Text Editor",
        title: "Font Family",
        description: "Controls the font family. A name the system does not have falls back to the default monospace face.",
        kind: SettingKind::Text { max_len: 120 },
        default: "Cascadia Mono",
        applies: Applies::Immediately,
    },
    SettingDescriptor {
        key: "editor.lineHeight",
        section: "Text Editor",
        title: "Line Height",
        description: "Height of a line as a multiple of the font size.",
        kind: SettingKind::Float { min: 1.0, max: 2.5 },
        default: "1.4",
        applies: Applies::Immediately,
    },
    SettingDescriptor {
        key: "editor.tabWidth",
        section: "Text Editor",
        title: "Tab Width",
        description: "How many spaces a tab is shown as, and inserted as when Insert Spaces is on.",
        kind: SettingKind::Integer { min: 1, max: 16 },
        default: "4",
        applies: Applies::Immediately,
    },
    SettingDescriptor {
        key: "editor.insertSpaces",
        section: "Text Editor",
        title: "Insert Spaces",
        description: "Insert spaces when Tab is pressed, rather than a tab character.",
        kind: SettingKind::Bool,
        default: "true",
        applies: Applies::Immediately,
    },
    SettingDescriptor {
        key: "editor.showLineNumbers",
        section: "Text Editor",
        title: "Show Line Numbers",
        description: "Show the line number gutter to the left of the document.",
        kind: SettingKind::Bool,
        default: "true",
        applies: Applies::Immediately,
    },
    SettingDescriptor {
        key: "editor.caretBlink",
        section: "Text Editor",
        title: "Caret Blink",
        description: "Blink the caret. Turning this off also stops the timer that wakes the window to blink it.",
        kind: SettingKind::Bool,
        default: "true",
        applies: Applies::Immediately,
    },
    // --- Workbench ---
    SettingDescriptor {
        key: "workbench.showStatusBar",
        section: "Workbench",
        title: "Show Status Bar",
        description: "Show the status line along the bottom of the window.",
        kind: SettingKind::Bool,
        default: "true",
        applies: Applies::Immediately,
    },
    SettingDescriptor {
        key: "workbench.sidebarWidth",
        section: "Workbench",
        title: "Sidebar Width",
        description: "Width of the explorer and search panel, in pixels. Dragging its edge changes this too.",
        kind: SettingKind::Integer { min: 150, max: 800 },
        default: "260",
        applies: Applies::Immediately,
    },
    SettingDescriptor {
        key: "workbench.restoreLastFolder",
        section: "Workbench",
        title: "Restore Last Folder",
        description: "Reopen the folder that was open when the application last closed.",
        kind: SettingKind::Bool,
        default: "true",
        applies: Applies::OnRestart,
    },
    // --- Terminal ---
    SettingDescriptor {
        key: "terminal.scrollbackBytes",
        section: "Terminal",
        title: "Scrollback",
        description: "How much terminal output is kept, in bytes. Older output is dropped; the process is never stopped.",
        kind: SettingKind::Integer { min: 16_384, max: 16_777_216 },
        default: "524288",
        applies: Applies::Immediately,
    },
    SettingDescriptor {
        key: "terminal.shell",
        section: "Terminal",
        title: "Shell",
        description: "Interpreter to run. Leave as auto to pick the best one the system has.",
        kind: SettingKind::Choice(&["auto", "pwsh", "powershell", "cmd", "bash", "sh"]),
        default: "auto",
        applies: Applies::OnRestart,
    },
    // --- Dependency View ---
    SettingDescriptor {
        key: "depgraph.labelLength",
        section: "Dependency View",
        title: "Label Length",
        description: "Longest filename drawn on a node before it is cut short.",
        kind: SettingKind::Integer { min: 5, max: 40 },
        default: "14",
        applies: Applies::Immediately,
    },
    SettingDescriptor {
        key: "depgraph.nodeSize",
        section: "Dependency View",
        title: "Node Size",
        description: "How large a circle is drawn, as a share of the distance to its nearest neighbour. Above about 0.4 the circles grow into each other and the lines between them stop being drawn.",
        kind: SettingKind::Float { min: 0.1, max: 0.45 },
        default: "0.34",
        applies: Applies::Immediately,
    },
    SettingDescriptor {
        key: "depgraph.zoomStep",
        section: "Dependency View",
        title: "Zoom Step",
        description: "How much one notch of the wheel zooms the graph.",
        kind: SettingKind::Float { min: 1.05, max: 2.0 },
        default: "1.25",
        applies: Applies::Immediately,
    },
    SettingDescriptor {
        key: "depgraph.simulationSteps",
        section: "Dependency View",
        title: "Layout Quality",
        description: "Steps of the force simulation. More is tidier and slower; the result is saved, so this is paid once per folder.",
        kind: SettingKind::Integer { min: 100, max: 3000 },
        default: "600",
        applies: Applies::OnRestart,
    },
    // --- Performance ---
    SettingDescriptor {
        key: "performance.showOverlay",
        section: "Performance",
        title: "Show Performance Overlay",
        description: "Show the frame and memory overlay (F12).",
        kind: SettingKind::Bool,
        default: "false",
        applies: Applies::Immediately,
    },
    SettingDescriptor {
        key: "performance.instrumentation",
        section: "Performance",
        title: "Record Latency",
        description: "Measure input-to-frame latency. Turning this off removes the measurement itself, not just the display.",
        kind: SettingKind::Bool,
        default: "true",
        applies: Applies::Immediately,
    },
];

/// The descriptor for `key`, if there is one.
pub fn descriptor(key: &str) -> Option<&'static SettingDescriptor> {
    SETTINGS.iter().find(|setting| setting.key == key)
}

/// Reads `text` as a value of this kind, bringing it inside the kind's
/// bounds rather than refusing it.
///
/// Clamping rather than rejecting is deliberate for numbers: someone who
/// types 900 into Font Size has said "as big as possible", and a window they
/// cannot read is a worse answer than 48. Choices are the exception --
/// there is no nearest option to a word that is not in the list, so that is
/// a `None` and the caller keeps what it had.
pub fn parse(kind: SettingKind, text: &str) -> Option<SettingValue> {
    let text = text.trim();
    match kind {
        SettingKind::Bool => match text {
            "true" | "yes" | "on" | "1" => Some(SettingValue::Bool(true)),
            "false" | "no" | "off" | "0" => Some(SettingValue::Bool(false)),
            _ => None,
        },
        SettingKind::Integer { min, max } => {
            // A float typed into an integer field is rounded rather than
            // rejected: "16.0" plainly means 16.
            let parsed = text
                .parse::<i64>()
                .ok()
                .or_else(|| text.parse::<f64>().ok().map(|value| value.round() as i64))?;
            Some(SettingValue::Integer(parsed.clamp(min, max)))
        }
        SettingKind::Float { min, max } => {
            let parsed = text.parse::<f64>().ok()?;
            if !parsed.is_finite() {
                return None;
            }
            Some(SettingValue::Float(parsed.clamp(min, max)))
        }
        SettingKind::Text { max_len } => {
            Some(SettingValue::Text(text.chars().take(max_len).collect()))
        }
        SettingKind::Choice(options) => options
            .iter()
            .find(|option| option.eq_ignore_ascii_case(text))
            .map(|option| SettingValue::Text((*option).to_string())),
    }
}

/// The current value of every setting.
///
/// Defaults are not stored: only what differs from them is, which is what
/// makes the settings file small, readable, and forward-compatible when a
/// default changes in a later build.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    changed: BTreeMap<String, SettingValue>,
}

impl Settings {
    pub fn new() -> Self {
        Settings::default()
    }

    /// The value of `key`, or its default. An unknown key reads as `None`.
    pub fn get(&self, key: &str) -> Option<SettingValue> {
        let descriptor = descriptor(key)?;
        Some(self.changed.get(key).cloned().unwrap_or_else(|| descriptor.default_value()))
    }

    /// Sets `key` from text, reporting whether anything actually changed.
    ///
    /// The value is brought inside its declared bounds on the way in, so a
    /// caller cannot store something the rest of the application would have
    /// to defend against.
    pub fn set(&mut self, key: &str, text: &str) -> bool {
        let Some(descriptor) = descriptor(key) else { return false };
        let Some(value) = parse(descriptor.kind, text) else { return false };
        let previous = self.get(key);
        if previous.as_ref() == Some(&value) {
            return false;
        }
        if value == descriptor.default_value() {
            self.changed.remove(key);
        } else {
            self.changed.insert(key.to_string(), value);
        }
        true
    }

    /// Puts `key` back to its default, reporting whether it had moved.
    pub fn reset(&mut self, key: &str) -> bool {
        self.changed.remove(key).is_some()
    }

    /// Whether `key` still holds its default.
    pub fn is_default(&self, key: &str) -> bool {
        !self.changed.contains_key(key)
    }

    /// How many settings have been changed from their defaults.
    pub fn changed_count(&self) -> usize {
        self.changed.len()
    }

    // --- typed reads, for the code that consumes settings ---

    pub fn bool(&self, key: &str) -> bool {
        self.get(key).and_then(|value| value.as_bool()).unwrap_or(false)
    }

    pub fn integer(&self, key: &str) -> i64 {
        self.get(key).and_then(|value| value.as_integer()).unwrap_or(0)
    }

    pub fn float(&self, key: &str) -> f64 {
        self.get(key).and_then(|value| value.as_float()).unwrap_or(0.0)
    }

    pub fn text(&self, key: &str) -> String {
        self.get(key).map(|value| value.to_text()).unwrap_or_default()
    }

    // --- the settings file ---

    /// Writes the changed settings out.
    ///
    /// Only what differs from a default is written, so the file reads as a
    /// list of decisions someone made rather than a dump of everything.
    pub fn encode(&self) -> String {
        let mut out = String::from(
            "# LightSpeed settings. Lines are `key = value`; anything not listed\n\
             # here is at its default. Values outside a setting's range are\n\
             # brought inside it when read, so this file cannot break the editor.\n",
        );
        for (key, value) in &self.changed {
            out.push_str(key);
            out.push_str(" = ");
            out.push_str(&value.to_text());
            out.push('\n');
        }
        out
    }

    /// Reads settings back.
    ///
    /// Unknown keys and unreadable values are skipped rather than failing the
    /// whole file: a settings file written by a later build, or edited by
    /// hand into something odd, should cost the reader the one line that is
    /// wrong and not every preference they have ever set.
    pub fn decode(text: &str) -> Settings {
        let mut settings = Settings::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            settings.set(key.trim(), value.trim());
        }
        settings
    }

    /// Lays another set of settings over this one.
    ///
    /// Only what `over` actually changed is copied, which is what makes the
    /// layering work: a workspace file states the handful of things that
    /// project decided, and everything it is silent about keeps whatever the
    /// person chose for themselves.
    pub fn overlay(&mut self, over: &Settings) {
        for (key, value) in &over.changed {
            self.changed.insert(key.clone(), value.clone());
        }
    }

    /// Settings whose key, title, section or description mention `query`.
    ///
    /// Matched case-insensitively over words, so "font size" finds
    /// `editor.fontSize` and "wrap" finds nothing rather than everything.
    pub fn search(query: &str) -> Vec<&'static SettingDescriptor> {
        let query = query.trim().to_ascii_lowercase();
        if query.is_empty() {
            return SETTINGS.iter().collect();
        }
        let words: Vec<&str> = query.split_whitespace().collect();
        SETTINGS
            .iter()
            .filter(|setting| {
                let haystack = format!(
                    "{} {} {} {}",
                    setting.key, setting.section, setting.title, setting.description
                )
                .to_ascii_lowercase();
                words.iter().all(|word| haystack.contains(word))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_is_valid_for_its_own_kind() {
        // The table is a constant, so nothing else checks it. A default that
        // does not parse would mean a setting silently reading as `false`.
        for setting in SETTINGS {
            assert!(
                parse(setting.kind, setting.default).is_some(),
                "{}: default {:?} is not a valid {:?}",
                setting.key,
                setting.default,
                setting.kind
            );
        }
    }

    #[test]
    fn every_default_survives_a_round_trip_through_its_own_text() {
        for setting in SETTINGS {
            let value = setting.default_value();
            let again = parse(setting.kind, &value.to_text());
            assert_eq!(again.as_ref(), Some(&value), "{} does not round-trip", setting.key);
        }
    }

    #[test]
    fn keys_are_unique_and_sections_are_all_declared() {
        let mut seen: Vec<&str> = Vec::new();
        for setting in SETTINGS {
            assert!(!seen.contains(&setting.key), "duplicate key {}", setting.key);
            seen.push(setting.key);
            assert!(
                SECTIONS.contains(&setting.section),
                "{} is in section {:?}, which the screen does not list",
                setting.key,
                setting.section
            );
            assert!(
                setting.description.ends_with('.'),
                "{}: the description should read as a sentence",
                setting.key
            );
        }
    }

    #[test]
    fn a_number_outside_its_range_is_brought_inside_it() {
        // The promise the whole module exists for: nothing a user types can
        // leave the application in a state it cannot draw.
        let mut settings = Settings::new();
        settings.set("editor.fontSize", "900000");
        assert_eq!(settings.integer("editor.fontSize"), 48);
        settings.set("editor.fontSize", "-40");
        assert_eq!(settings.integer("editor.fontSize"), 6);
        settings.set("editor.lineHeight", "0.01");
        assert!((settings.float("editor.lineHeight") - 1.0).abs() < 0.001);
    }

    #[test]
    fn nonsense_is_refused_and_the_old_value_kept() {
        let mut settings = Settings::new();
        assert!(settings.set("editor.fontSize", "18"));
        assert!(!settings.set("editor.fontSize", "banana"), "not a number");
        assert_eq!(settings.integer("editor.fontSize"), 18, "the good value survived");

        assert!(!settings.set("editor.lineHeight", "inf"), "not finite");
        assert!(!settings.set("terminal.shell", "fish"), "not one of the options");
        assert!(!settings.set("nothing.likeThis", "true"), "unknown key");
    }

    #[test]
    fn a_choice_is_matched_regardless_of_case() {
        let mut settings = Settings::new();
        assert!(settings.set("terminal.shell", "PowerShell"));
        assert_eq!(settings.text("terminal.shell"), "powershell", "stored as the option itself");
    }

    #[test]
    fn a_float_typed_into_an_integer_field_is_rounded() {
        let mut settings = Settings::new();
        settings.set("editor.tabWidth", "8.0");
        assert_eq!(settings.integer("editor.tabWidth"), 8);
    }

    #[test]
    fn text_is_cut_to_its_declared_length() {
        let mut settings = Settings::new();
        let long = "x".repeat(500);
        settings.set("editor.fontFamily", &long);
        assert_eq!(settings.text("editor.fontFamily").chars().count(), 120);
    }

    #[test]
    fn an_unset_setting_reads_as_its_default() {
        let settings = Settings::new();
        assert_eq!(settings.integer("editor.fontSize"), 14);
        assert!(settings.bool("editor.showLineNumbers"));
        assert_eq!(settings.text("editor.fontFamily"), "Cascadia Mono");
        assert!(settings.is_default("editor.fontSize"));
    }

    #[test]
    fn setting_a_value_back_to_its_default_forgets_it() {
        // What keeps the file a list of decisions rather than a dump, and
        // what lets a later build change a default without being overridden
        // by a value the user never chose.
        let mut settings = Settings::new();
        settings.set("editor.fontSize", "20");
        assert!(!settings.is_default("editor.fontSize"));
        settings.set("editor.fontSize", "14");
        assert!(settings.is_default("editor.fontSize"));
        assert_eq!(settings.changed_count(), 0);
        assert!(!settings.encode().contains("editor.fontSize"));
    }

    #[test]
    fn reset_puts_a_setting_back() {
        let mut settings = Settings::new();
        settings.set("editor.tabWidth", "2");
        assert!(settings.reset("editor.tabWidth"));
        assert_eq!(settings.integer("editor.tabWidth"), 4);
        assert!(!settings.reset("editor.tabWidth"), "already default");
    }

    #[test]
    fn only_what_changed_is_written_and_it_reads_back() {
        let mut settings = Settings::new();
        settings.set("editor.fontSize", "18");
        settings.set("editor.fontFamily", "JetBrains Mono");
        settings.set("editor.showLineNumbers", "false");

        let text = settings.encode();
        assert_eq!(text.lines().filter(|line| !line.starts_with('#')).count(), 3);
        assert_eq!(Settings::decode(&text), settings);
    }

    #[test]
    fn a_font_family_with_spaces_and_commas_survives_the_file() {
        let mut settings = Settings::new();
        settings.set("editor.fontFamily", "Consolas, 'Courier New', monospace");
        let read = Settings::decode(&settings.encode());
        assert_eq!(read.text("editor.fontFamily"), "Consolas, 'Courier New', monospace");
    }

    #[test]
    fn a_damaged_settings_file_costs_only_the_lines_that_are_wrong() {
        let text = "# a comment\n\
                    editor.fontSize = 22\n\
                    this line has no equals sign\n\
                    editor.tabWidth = banana\n\
                    nothing.likeThis = 5\n\
                    \n\
                    editor.insertSpaces = false\n";
        let settings = Settings::decode(text);
        assert_eq!(settings.integer("editor.fontSize"), 22, "the good lines still land");
        assert!(!settings.bool("editor.insertSpaces"));
        assert_eq!(settings.integer("editor.tabWidth"), 4, "the bad line is skipped");
        assert_eq!(settings.changed_count(), 2);
    }

    #[test]
    fn an_empty_search_lists_everything_and_a_query_narrows_it() {
        assert_eq!(Settings::search("").len(), SETTINGS.len());
        assert_eq!(Settings::search("   ").len(), SETTINGS.len());

        let hits = Settings::search("font size");
        assert!(hits.iter().any(|setting| setting.key == "editor.fontSize"));
        assert!(
            !hits.iter().any(|setting| setting.key == "editor.tabWidth"),
            "both words have to match"
        );
    }

    #[test]
    fn search_finds_a_setting_by_its_key_its_title_or_its_description() {
        assert!(Settings::search("scrollback").iter().any(|s| s.key == "terminal.scrollbackBytes"));
        assert!(Settings::search("TERMINAL").iter().any(|s| s.section == "Terminal"));
        assert!(Settings::search("gutter").iter().any(|s| s.key == "editor.showLineNumbers"));
        assert!(Settings::search("zqx").is_empty(), "a query that matches nothing finds nothing");
    }

    #[test]
    fn a_workspace_overlays_only_what_it_actually_states() {
        // The layering the two settings files exist for: a project states a
        // tab width, and says nothing about the font someone likes.
        let mut user = Settings::new();
        user.set("editor.fontSize", "18");
        user.set("editor.tabWidth", "8");

        let mut workspace = Settings::new();
        workspace.set("editor.tabWidth", "2");

        let mut effective = user.clone();
        effective.overlay(&workspace);
        assert_eq!(effective.integer("editor.tabWidth"), 2, "the project wins where it spoke");
        assert_eq!(effective.integer("editor.fontSize"), 18, "and is silent elsewhere");
    }

    #[test]
    fn overlaying_nothing_changes_nothing() {
        let mut user = Settings::new();
        user.set("editor.fontSize", "18");
        let before = user.clone();
        user.overlay(&Settings::new());
        assert_eq!(user, before);
    }

    #[test]
    fn word_wrap_is_deliberately_not_offered() {
        // Not an oversight. The editor maps one document line to one laid-out
        // row and hit-testing depends on it, so a wrap toggle would only
        // succeed in putting the caret in the wrong place. If wrapping is
        // ever implemented, this test is the thing to delete.
        assert!(
            !SETTINGS.iter().any(|setting| setting.key.to_ascii_lowercase().contains("wrap")),
            "a wrap setting was added without the layout to back it"
        );
    }
}
