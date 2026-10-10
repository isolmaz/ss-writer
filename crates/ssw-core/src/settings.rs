//! The settings model (spec §10), stored as human-editable `settings.json`.
//!
//! Every section is `#[serde(default)]`, so a partial file loads with defaults for the rest.
//! Keys this version does not know — at the top level or inside any section — are kept in
//! the `extra` maps and written back unchanged (`unknown keys preserved`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::keymap::{Keymap, Overrides};

/// Keys not modelled by this version of the app, preserved verbatim.
pub type Extra = BTreeMap<String, Value>;

/// Failure to read `settings.json` content. (Writing cannot fail: every value of the model is
/// a string-keyed JSON value, which always serializes.)
#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("settings.json is not valid: {0}")]
    Parse(#[source] serde_json::Error),
}

/// The settings pages, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SettingsSection {
    General,
    Appearance,
    Editor,
    Sidebar,
    Attachments,
    Export,
    Shortcuts,
    Advanced,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 8] = [
        SettingsSection::General,
        SettingsSection::Appearance,
        SettingsSection::Editor,
        SettingsSection::Sidebar,
        SettingsSection::Attachments,
        SettingsSection::Export,
        SettingsSection::Shortcuts,
        SettingsSection::Advanced,
    ];

    /// The JSON key of the section in `settings.json`.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            SettingsSection::General => "general",
            SettingsSection::Appearance => "appearance",
            SettingsSection::Editor => "editor",
            SettingsSection::Sidebar => "sidebar",
            SettingsSection::Attachments => "attachments",
            SettingsSection::Export => "export",
            SettingsSection::Shortcuts => "shortcuts",
            SettingsSection::Advanced => "advanced",
        }
    }

    /// Fluent message id of the section title.
    #[must_use]
    pub const fn label_id(self) -> &'static str {
        match self {
            SettingsSection::General => "settings-section-general",
            SettingsSection::Appearance => "settings-section-appearance",
            SettingsSection::Editor => "settings-section-editor",
            SettingsSection::Sidebar => "settings-section-sidebar",
            SettingsSection::Attachments => "settings-section-attachments",
            SettingsSection::Export => "settings-section-export",
            SettingsSection::Shortcuts => "settings-section-shortcuts",
            SettingsSection::Advanced => "settings-section-advanced",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Enumerations
// ---------------------------------------------------------------------------------------------

/// What to open when the app starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupBehavior {
    #[default]
    RestoreSession,
    NewDocument,
    DefaultFolder,
}

/// Where `File → New` creates a file when a workspace is open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewFileLocation {
    /// The folder selected in the sidebar (workspace root when none).
    #[default]
    SelectedFolder,
    /// The default folder from the General settings.
    DefaultFolder,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccentMode {
    /// Follow the Windows accent color.
    #[default]
    System,
    /// The built-in blue `#1874D3`.
    Blue,
    /// `appearance.accent_custom`.
    Custom,
}

/// Indent unit for list items.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ListIndent {
    /// Align to the content of the parent item.
    #[default]
    #[serde(rename = "align")]
    Align,
    #[serde(rename = "2")]
    Two,
    #[serde(rename = "4")]
    Four,
    #[serde(rename = "tab")]
    Tab,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortBy {
    #[default]
    Name,
    DateModified,
    DateCreated,
    Kind,
}

/// Where pasted images are stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentLocation {
    /// Next to the file.
    #[default]
    NextToFile,
    /// An `assets/` folder next to the file.
    AssetsFolder,
    /// A `<file>_assets/` folder next to the file.
    FileAssetsFolder,
    /// `attachments.custom_folder`.
    Custom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageSize {
    #[default]
    A4,
    Letter,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    #[default]
    Pdf,
    Png,
    Jpg,
    Html,
    Epub,
    Docx,
    Odt,
    Rtf,
    Txt,
    MarkdownWithAttachments,
    Textbundle,
}

// ---------------------------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------------------------

/// **General**.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// BCP 47 tag overriding the OS UI language; `None` follows the OS.
    pub language: Option<String>,
    pub on_startup: StartupBehavior,
    pub default_folder: Option<String>,
    pub autosave: bool,
    /// Idle time after typing before autosave, milliseconds.
    pub autosave_delay_ms: u32,
    pub new_file_location: NewFileLocation,
    pub sync_file_name_with_heading: bool,
    pub check_for_updates: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for General {
    fn default() -> Self {
        Self {
            language: None,
            on_startup: StartupBehavior::default(),
            default_folder: None,
            autosave: true,
            autosave_delay_ms: 1000,
            new_file_location: NewFileLocation::default(),
            sync_file_name_with_heading: false,
            check_for_updates: true,
            extra: Extra::new(),
        }
    }
}

/// **Appearance**.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: Theme,
    pub accent: AccentMode,
    /// `#RRGGBB`, used when `accent` is `custom`.
    pub accent_custom: Option<String>,
    /// Mica Alt backdrop.
    pub mica: bool,
    /// Sidebar font size in DIPs.
    pub sidebar_font_size: f32,
    pub show_status_bar: bool,
    /// The B I U pill at the top right of the editor.
    pub show_format_pill: bool,
    /// App-wide zoom in percent (`Ctrl+=` / `Ctrl+-` / `Ctrl+0`), persisted.
    pub zoom_percent: u32,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            accent: AccentMode::default(),
            accent_custom: None,
            mica: true,
            sidebar_font_size: 13.0,
            show_status_bar: true,
            show_format_pill: true,
            zoom_percent: 100,
            extra: Extra::new(),
        }
    }
}

/// **Editor**.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Editor {
    pub text_font: String,
    pub heading_font: String,
    pub code_font: String,
    /// Text size in DIPs.
    pub font_size: f32,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
    /// Extra space between paragraphs, in em.
    pub paragraph_spacing: f32,
    /// Maximum text column width in DIPs.
    pub line_width: u32,
    pub list_indent: ListIndent,
    pub auto_hide_markdown: bool,
    /// Typewriter caret line position, fraction of the viewport height (0.0–1.0).
    pub typewriter_position: f32,
    /// Typewriter dead zone, fraction of the viewport height on each side of the position.
    pub typewriter_dead_zone: f32,
    pub spell_check: bool,
    /// BCP 47 tags; empty = OS language.
    pub spell_languages: Vec<String>,
    pub sort_completed_tasks: bool,
    pub syntax_highlighting: bool,
    pub format_tables_on_exit: bool,
    /// Hide the YAML front matter card.
    pub hide_front_matter: bool,
    /// Reading speed for the reading-time estimate, words per minute.
    pub reading_speed_wpm: u32,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            text_font: "Segoe UI Variable Text".to_owned(),
            heading_font: "Segoe UI Variable Text".to_owned(),
            code_font: "Cascadia Mono".to_owned(),
            font_size: 16.0,
            line_height: 1.75,
            paragraph_spacing: 0.5,
            line_width: 720,
            list_indent: ListIndent::default(),
            auto_hide_markdown: true,
            typewriter_position: 0.5,
            typewriter_dead_zone: 0.10,
            spell_check: true,
            spell_languages: Vec::new(),
            sort_completed_tasks: true,
            syntax_highlighting: true,
            format_tables_on_exit: true,
            hide_front_matter: false,
            reading_speed_wpm: 200,
            extra: Extra::new(),
        }
    }
}

/// **Sidebar**.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidebarSettings {
    pub sort_by: SortBy,
    pub sort_descending: bool,
    pub folders_first: bool,
    pub show_extensions: bool,
    pub show_hidden_items: bool,
    pub hide_images: bool,
    pub hide_non_markdown: bool,
    /// Show the modified date under names.
    pub show_modified_date: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for SidebarSettings {
    fn default() -> Self {
        Self {
            sort_by: SortBy::default(),
            sort_descending: false,
            folders_first: true,
            show_extensions: false,
            show_hidden_items: false,
            hide_images: false,
            hide_non_markdown: false,
            show_modified_date: false,
            extra: Extra::new(),
        }
    }
}

/// **Attachments**.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Attachments {
    pub location: AttachmentLocation,
    /// Folder used when `location` is `custom`.
    pub custom_folder: String,
    /// File name template; `{n}` is the running number.
    pub naming_template: String,
    pub load_remote_images: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for Attachments {
    fn default() -> Self {
        Self {
            location: AttachmentLocation::default(),
            custom_folder: String::new(),
            naming_template: "image-{n}".to_owned(),
            load_remote_images: true,
            extra: Extra::new(),
        }
    }
}

/// **Export**.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportSettings {
    pub page_size: PageSize,
    pub margin_mm: f32,
    pub base_font_size_pt: f32,
    pub page_numbers: bool,
    pub black_text: bool,
    pub light_theme: bool,
    pub include_title: bool,
    pub default_format: ExportFormat,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            page_size: PageSize::default(),
            margin_mm: 20.0,
            base_font_size_pt: 11.0,
            page_numbers: true,
            black_text: false,
            light_theme: true,
            include_title: true,
            default_format: ExportFormat::default(),
            extra: Extra::new(),
        }
    }
}

/// **Advanced**.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Advanced {
    /// Upper bound of files the workspace index will hold.
    pub index_max_files: u32,
    /// Version-history retention in days.
    pub history_retention_days: u32,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for Advanced {
    fn default() -> Self {
        Self {
            index_max_files: 100_000,
            history_retention_days: 30,
            extra: Extra::new(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Root
// ---------------------------------------------------------------------------------------------

/// The whole of `settings.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub general: General,
    pub appearance: Appearance,
    pub editor: Editor,
    pub sidebar: SidebarSettings,
    pub attachments: Attachments,
    pub export: ExportSettings,
    /// Shortcut overrides: command id → chords (see [`crate::keymap::Overrides`]).
    pub shortcuts: Overrides,
    pub advanced: Advanced,
    /// Top-level keys unknown to this version.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Settings {
    /// Parse `settings.json` content. Empty or whitespace-only text yields the defaults.
    ///
    /// # Errors
    /// [`SettingsError::Parse`] when the text is not valid JSON or a known key has a value of
    /// the wrong type.
    pub fn from_json_str(text: &str) -> Result<Self, SettingsError> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(text).map_err(SettingsError::Parse)
    }

    /// Pretty-printed JSON with a trailing newline.
    ///
    /// Serialization cannot fail: every map is keyed by strings and a non-finite float is
    /// written as `null`.
    #[must_use]
    pub fn to_json_string_pretty(&self) -> String {
        let mut text =
            serde_json::to_string_pretty(self).expect("settings serialize to string-keyed JSON");
        text.push('\n');
        text
    }

    /// The effective keymap: spec §11 defaults with the `shortcuts` overrides applied.
    #[must_use]
    pub fn keymap(&self) -> Keymap {
        Keymap::with_overrides(&self.shortcuts)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::command::CommandId;
    use crate::keymap::{KeyChord, vk};

    const MAIN_FTL: &str = include_str!("../../../assets/i18n/en/main.ftl");

    #[test]
    fn defaults_follow_the_spec() {
        let s = Settings::default();
        assert!(s.general.autosave);
        assert_eq!(s.general.autosave_delay_ms, 1000);
        assert_eq!(s.general.on_startup, StartupBehavior::RestoreSession);
        assert!(!s.general.sync_file_name_with_heading);
        assert!(s.general.check_for_updates);
        assert_eq!(s.appearance.theme, Theme::System);
        assert_eq!(s.appearance.accent, AccentMode::System);
        assert!(s.appearance.mica);
        assert!(s.appearance.show_status_bar);
        assert!(s.appearance.show_format_pill);
        assert_eq!(s.appearance.zoom_percent, 100);
        assert_eq!(s.editor.text_font, "Segoe UI Variable Text");
        assert_eq!(s.editor.code_font, "Cascadia Mono");
        assert_eq!(s.editor.font_size, 16.0);
        assert_eq!(s.editor.line_height, 1.75);
        assert_eq!(s.editor.list_indent, ListIndent::Align);
        assert!(s.editor.auto_hide_markdown);
        assert_eq!(s.editor.typewriter_position, 0.5);
        assert_eq!(s.editor.typewriter_dead_zone, 0.10);
        assert!(s.editor.sort_completed_tasks);
        assert!(s.editor.format_tables_on_exit);
        assert_eq!(s.editor.reading_speed_wpm, 200);
        assert!(s.sidebar.folders_first);
        assert_eq!(s.sidebar.sort_by, SortBy::Name);
        assert_eq!(s.attachments.location, AttachmentLocation::NextToFile);
        assert_eq!(s.attachments.naming_template, "image-{n}");
        assert!(s.attachments.load_remote_images);
        assert_eq!(s.export.page_size, PageSize::A4);
        assert_eq!(s.advanced.index_max_files, 100_000);
        assert_eq!(s.advanced.history_retention_days, 30);
        assert!(s.shortcuts.is_empty());
        assert!(s.extra.is_empty());
        assert_eq!(s.keymap(), Keymap::default());
    }

    #[test]
    fn empty_and_partial_documents_fill_in_defaults() {
        assert_eq!(
            Settings::from_json_str("").expect("empty"),
            Settings::default()
        );
        assert_eq!(
            Settings::from_json_str(" \n").expect("blank"),
            Settings::default()
        );
        assert_eq!(
            Settings::from_json_str("{}").expect("object"),
            Settings::default()
        );

        let s = Settings::from_json_str(
            r#"{"editor":{"font_size":18,"list_indent":"4"},"general":{"autosave":false}}"#,
        )
        .expect("partial");
        assert_eq!(s.editor.font_size, 18.0);
        assert_eq!(s.editor.list_indent, ListIndent::Four);
        assert!(!s.general.autosave);
        assert_eq!(s.editor.line_height, 1.75);
        assert_eq!(s.general.autosave_delay_ms, 1000);
        assert_eq!(s.sidebar, SidebarSettings::default());
    }

    #[test]
    fn invalid_documents_are_errors() {
        let SettingsError::Parse(truncated) = Settings::from_json_str("{").unwrap_err();
        assert!(truncated.is_eof(), "{truncated}");
        let SettingsError::Parse(unknown_theme) =
            Settings::from_json_str(r#"{"appearance":{"theme":"sepia"}}"#).unwrap_err();
        assert!(unknown_theme.is_data(), "{unknown_theme}");
        assert!(unknown_theme.to_string().contains("sepia"));
    }

    #[test]
    fn unknown_keys_survive_a_round_trip_at_every_level() {
        let source = json!({
            "future_top_level": {"a": [1, 2, {"b": null}]},
            "general": {"autosave": false, "future_general": "x"},
            "appearance": {"future_appearance": 1.5},
            "editor": {"font_size": 20, "nested_future": {"deep": {"deeper": true}}},
            "sidebar": {"future_sidebar": [1]},
            "attachments": {"future_attachments": "y"},
            "export": {"future_export": false},
            "advanced": {"future_advanced": 7},
            "shortcuts": {"format.bold": [{"vk": 66, "ctrl": true, "shift": true}], "from.the_future": []}
        });
        let settings = Settings::from_json_str(&source.to_string()).expect("parse");
        assert_eq!(
            settings.extra["future_top_level"],
            json!({"a": [1, 2, {"b": null}]})
        );
        assert_eq!(
            settings.editor.extra["nested_future"],
            json!({"deep": {"deeper": true}})
        );
        assert_eq!(settings.general.extra["future_general"], json!("x"));
        assert!(!settings.general.autosave);
        assert_eq!(settings.editor.font_size, 20.0);

        let text = settings.to_json_string_pretty();
        assert!(text.ends_with("}\n"));
        assert!(text.contains('\n'), "pretty printed");
        let out: Value = serde_json::from_str(&text).expect("valid json");

        // Everything that was in the source is still there, unchanged.
        fn contains(superset: &Value, subset: &Value, path: &str) {
            match (superset, subset) {
                (Value::Object(sup), Value::Object(sub)) => {
                    for (k, v) in sub {
                        assert!(sup.contains_key(k), "lost key {path}/{k}");
                        contains(&sup[k], v, &format!("{path}/{k}"));
                    }
                }
                (Value::Array(sup), Value::Array(sub)) if sup.len() == sub.len() => {
                    for (n, (got, want)) in sup.iter().zip(sub).enumerate() {
                        contains(got, want, &format!("{path}[{n}]"));
                    }
                }
                // Known numeric settings are typed floats, so `20` comes back as `20.0`.
                (Value::Number(a), Value::Number(b)) => {
                    assert_eq!(a.as_f64(), b.as_f64(), "{path}")
                }
                _ => assert_eq!(superset, subset, "{path}"),
            }
        }
        contains(&out, &source, "");

        // And the whole thing is stable.
        let again = Settings::from_json_str(&text).expect("reparse");
        assert_eq!(again, settings);
        assert_eq!(again.to_json_string_pretty(), text);
    }

    #[test]
    fn defaults_serialize_and_reload_identically() {
        let text = Settings::default().to_json_string_pretty();
        assert_eq!(
            Settings::from_json_str(&text).expect("reparse"),
            Settings::default()
        );
        let value: Value = serde_json::from_str(&text).expect("json");
        for section in SettingsSection::ALL {
            let key = section.key();
            assert!(value.get(key).is_some(), "{key}");
        }
        assert_eq!(value["editor"]["list_indent"], json!("align"));
    }

    #[test]
    fn shortcut_overrides_feed_the_keymap() {
        let mut settings = Settings::default();
        let chord = KeyChord::new(vk::F2).with_ctrl();
        settings
            .shortcuts
            .insert("view.fold_all".to_owned(), vec![chord]);
        let text = settings.to_json_string_pretty();
        let reloaded = Settings::from_json_str(&text).expect("reparse");
        assert_eq!(
            reloaded.keymap().lookup(chord),
            Some(CommandId::ViewFoldAll)
        );
    }

    #[test]
    fn every_enum_value_round_trips() {
        for indent in [
            ListIndent::Align,
            ListIndent::Two,
            ListIndent::Four,
            ListIndent::Tab,
        ] {
            let json = serde_json::to_string(&indent).expect("serialize");
            assert_eq!(
                serde_json::from_str::<ListIndent>(&json).expect("parse"),
                indent
            );
        }
        assert_eq!(
            serde_json::to_string(&ListIndent::Two).expect("serialize"),
            r#""2""#
        );
        assert_eq!(
            serde_json::to_string(&ExportFormat::MarkdownWithAttachments).expect("serialize"),
            r#""markdown_with_attachments""#
        );
    }

    #[test]
    fn section_titles_exist_in_english() {
        for section in SettingsSection::ALL {
            let id = section.label_id();
            let prefix = format!("{id} =");
            assert!(
                MAIN_FTL.lines().any(|l| l.starts_with(&prefix)),
                "missing ftl message {id}"
            );
        }
    }
}
