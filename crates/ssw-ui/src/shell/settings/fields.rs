//! The table of settings controls (spec §10): one entry per option, binding a control kind to
//! a field of [`Settings`] with its validation.
//!
//! Every field of every section except `shortcuts` (which has its own page) is listed here, so
//! the Settings window cannot forget an option: the page builders iterate this table.

use ssw_core::i18n::{Arg, Localizer};
use ssw_core::settings::{
    AccentMode, AttachmentLocation, ExportFormat, ListIndent, NewFileLocation, PageSize, Settings,
    SettingsSection, SortBy, StartupBehavior, Theme,
};

/// How a field is edited.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum FieldKind {
    /// A switch.
    Toggle,
    /// A drop-down; the strings are the Fluent ids of the options, in order.
    Choice(&'static [&'static str]),
    /// A drop-down of the languages the app ships (`None` = follow the OS).
    Language,
    /// Free text.
    Text,
    /// A path with a Browse button.
    Folder,
    /// A number typed into a text box and range-checked.
    Number { min: f64, max: f64, integer: bool },
    /// A slider from 0 to `max` percent.
    Percent { max: u8 },
}

/// A value moving between a control and [`Settings`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FieldValue {
    Bool(bool),
    /// Index into the options of a choice.
    Choice(usize),
    Text(String),
    Number(f64),
}

impl FieldValue {
    pub(crate) fn flag(&self) -> bool {
        matches!(self, FieldValue::Bool(true))
    }

    pub(crate) fn index(&self) -> usize {
        match self {
            FieldValue::Choice(index) => *index,
            _ => usize::MAX,
        }
    }

    pub(crate) fn text(&self) -> &str {
        match self {
            FieldValue::Text(text) => text,
            _ => "",
        }
    }

    pub(crate) fn number(&self) -> f64 {
        match self {
            FieldValue::Number(number) => *number,
            _ => 0.0,
        }
    }
}

/// Why a typed value was refused. The setting keeps its previous value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Problem {
    /// Not a number.
    NotANumber,
    /// A fractional number where a whole number is needed.
    NotAnInteger,
    /// Outside `min..=max`.
    OutOfRange {
        /// Smallest accepted value.
        min: f64,
        /// Largest accepted value.
        max: f64,
    },
    /// An empty value where one is required.
    Empty,
    /// Not `#RRGGBB`.
    BadColor,
    /// A file-name template without `{n}` or with characters Windows forbids in names.
    BadTemplate,
    /// A path with characters Windows forbids.
    BadPath,
    /// Not a BCP 47 language tag.
    BadLanguageTag(String),
}

impl Problem {
    /// The message shown under the control, in the language of `loc`.
    pub(crate) fn message(&self, loc: &Localizer) -> String {
        match self {
            Problem::NotANumber => loc.text("settings-error-number"),
            Problem::NotAnInteger => loc.text("settings-error-integer"),
            Problem::OutOfRange { min, max } => loc.format(
                "settings-error-range",
                &[
                    ("min", Arg::Str(&display_number(*min))),
                    ("max", Arg::Str(&display_number(*max))),
                ],
            ),
            Problem::Empty => loc.text("settings-error-empty"),
            Problem::BadColor => loc.text("settings-error-color"),
            Problem::BadTemplate => loc.text("settings-error-template"),
            Problem::BadPath => loc.text("settings-error-path"),
            Problem::BadLanguageTag(tag) => {
                loc.format("settings-error-language", &[("tag", Arg::Str(tag))])
            }
        }
    }
}

/// One option of the Settings window.
#[derive(Clone, Copy)]
pub(crate) struct FieldDef {
    pub id: FieldId,
    pub section: SettingsSection,
    /// Fluent id of the label.
    pub label: &'static str,
    /// Fluent id of the explanation under the label.
    pub hint: Option<&'static str>,
    pub kind: FieldKind,
    get: fn(&Settings) -> FieldValue,
    set: fn(&mut Settings, &FieldValue) -> Result<(), Problem>,
}

impl FieldDef {
    /// The current value of the field.
    pub(crate) fn read(&self, settings: &Settings) -> FieldValue {
        (self.get)(settings)
    }

    /// Stores `value`; on a validation failure the settings are left unchanged.
    pub(crate) fn write(&self, settings: &mut Settings, value: &FieldValue) -> Result<(), Problem> {
        (self.set)(settings, value)
    }

    /// Converts the text of a text box into the value of this field.
    pub(crate) fn parse_input(&self, text: &str) -> Result<FieldValue, Problem> {
        match self.kind {
            FieldKind::Number { min, max, integer } => {
                parse_number(text, min, max, integer).map(FieldValue::Number)
            }
            _ => Ok(FieldValue::Text(text.trim().to_owned())),
        }
    }
}

/// The text a number is shown as: up to three decimals, no trailing zeros.
pub(crate) fn display_number(value: f64) -> String {
    let text = format!("{value:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Parses a number typed by the user; `,` is accepted as the decimal separator.
fn parse_number(text: &str, min: f64, max: f64, integer: bool) -> Result<f64, Problem> {
    let normalized = text.trim().replace(',', ".");
    let value: f64 = normalized.parse().map_err(|_| Problem::NotANumber)?;
    if !value.is_finite() {
        return Err(Problem::NotANumber);
    }
    if integer && value.fract() != 0.0 {
        return Err(Problem::NotAnInteger);
    }
    if !(min..=max).contains(&value) {
        return Err(Problem::OutOfRange { min, max });
    }
    Ok(value)
}

/// Characters Windows does not allow in a path.
const PATH_FORBIDDEN: &[char] = &['<', '>', '"', '|', '?', '*'];
/// Characters Windows does not allow in a file name.
const NAME_FORBIDDEN: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

fn required(text: &str) -> Result<String, Problem> {
    let text = text.trim();
    if text.is_empty() {
        Err(Problem::Empty)
    } else {
        Ok(text.to_owned())
    }
}

fn optional_path(text: &str) -> Result<Option<String>, Problem> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if text.contains(PATH_FORBIDDEN) || text.contains(|c: char| c.is_control()) {
        return Err(Problem::BadPath);
    }
    Ok(Some(text.to_owned()))
}

fn optional_color(text: &str) -> Result<Option<String>, Problem> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let digits = text.strip_prefix('#').unwrap_or("");
    if digits.len() == 6 && digits.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(Some(format!("#{}", digits.to_ascii_uppercase())))
    } else {
        Err(Problem::BadColor)
    }
}

fn naming_template(text: &str) -> Result<String, Problem> {
    let text = text.trim();
    if !text.contains("{n}") || text.contains(NAME_FORBIDDEN) {
        return Err(Problem::BadTemplate);
    }
    Ok(text.to_owned())
}

/// A BCP 47 tag: a 2–3 letter language followed by alphanumeric subtags of 1–8 characters.
fn is_language_tag(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let language_ok = parts.next().is_some_and(|language| {
        (2..=3).contains(&language.len()) && language.chars().all(|c| c.is_ascii_alphabetic())
    });
    language_ok
        && parts.all(|part| {
            (1..=8).contains(&part.len()) && part.chars().all(|c| c.is_ascii_alphanumeric())
        })
}

fn language_list(text: &str) -> Result<Vec<String>, Problem> {
    let mut tags = Vec::new();
    for tag in text.split(',').map(str::trim).filter(|tag| !tag.is_empty()) {
        if !is_language_tag(tag) {
            return Err(Problem::BadLanguageTag(tag.to_owned()));
        }
        tags.push(tag.to_owned());
    }
    Ok(tags)
}

/// Whether the control of `field` is usable with the current `settings`.
pub(crate) fn is_enabled(field: FieldId, settings: &Settings) -> bool {
    match field {
        FieldId::AutosaveDelay => settings.general.autosave,
        FieldId::AccentCustom => settings.appearance.accent == AccentMode::Custom,
        FieldId::SpellLanguages => settings.editor.spell_check,
        FieldId::CustomFolder => settings.attachments.location == AttachmentLocation::Custom,
        _ => true,
    }
}

macro_rules! toggle {
    ($id:ident, $section:ident, $label:literal, $hint:expr, $sec:ident . $field:ident) => {
        FieldDef {
            id: FieldId::$id,
            section: SettingsSection::$section,
            label: $label,
            hint: $hint,
            kind: FieldKind::Toggle,
            get: |s| FieldValue::Bool(s.$sec.$field),
            set: |s, v| {
                s.$sec.$field = v.flag();
                Ok(())
            },
        }
    };
}

macro_rules! choice {
    (
        $id:ident, $section:ident, $label:literal, $hint:expr, $sec:ident . $field:ident,
        $ty:ty, [$($variant:expr => $option:literal),+ $(,)?]
    ) => {
        FieldDef {
            id: FieldId::$id,
            section: SettingsSection::$section,
            label: $label,
            hint: $hint,
            kind: FieldKind::Choice(&[$($option),+]),
            get: |s| {
                let all: &[$ty] = &[$($variant),+];
                FieldValue::Choice(all.iter().position(|v| *v == s.$sec.$field).unwrap_or(0))
            },
            set: |s, v| {
                let all: &[$ty] = &[$($variant),+];
                if let Some(value) = all.get(v.index()) {
                    s.$sec.$field = *value;
                }
                Ok(())
            },
        }
    };
}

macro_rules! whole {
    ($id:ident, $section:ident, $label:literal, $hint:expr, $sec:ident . $field:ident, $min:expr, $max:expr) => {
        FieldDef {
            id: FieldId::$id,
            section: SettingsSection::$section,
            label: $label,
            hint: $hint,
            kind: FieldKind::Number {
                min: $min,
                max: $max,
                integer: true,
            },
            get: |s| FieldValue::Number(f64::from(s.$sec.$field)),
            set: |s, v| {
                s.$sec.$field = v.number() as u32;
                Ok(())
            },
        }
    };
}

macro_rules! decimal {
    ($id:ident, $section:ident, $label:literal, $hint:expr, $sec:ident . $field:ident, $min:expr, $max:expr) => {
        FieldDef {
            id: FieldId::$id,
            section: SettingsSection::$section,
            label: $label,
            hint: $hint,
            kind: FieldKind::Number {
                min: $min,
                max: $max,
                integer: false,
            },
            get: |s| FieldValue::Number(f64::from(s.$sec.$field)),
            set: |s, v| {
                s.$sec.$field = v.number() as f32;
                Ok(())
            },
        }
    };
}

macro_rules! percent {
    ($id:ident, $section:ident, $label:literal, $hint:expr, $sec:ident . $field:ident, $max:expr) => {
        FieldDef {
            id: FieldId::$id,
            section: SettingsSection::$section,
            label: $label,
            hint: $hint,
            kind: FieldKind::Percent { max: $max },
            get: |s| FieldValue::Number((f64::from(s.$sec.$field) * 100.0).round()),
            set: |s, v| {
                s.$sec.$field = (v.number() / 100.0) as f32;
                Ok(())
            },
        }
    };
}

macro_rules! font {
    ($id:ident, $label:literal, $field:ident) => {
        FieldDef {
            id: FieldId::$id,
            section: SettingsSection::Editor,
            label: $label,
            hint: None,
            kind: FieldKind::Text,
            get: |s| FieldValue::Text(s.editor.$field.clone()),
            set: |s, v| {
                s.editor.$field = required(v.text())?;
                Ok(())
            },
        }
    };
}

macro_rules! custom {
    (
        $id:ident, $section:ident, $label:literal, $hint:expr, $kind:expr,
        $get:expr, $set:expr
    ) => {
        FieldDef {
            id: FieldId::$id,
            section: SettingsSection::$section,
            label: $label,
            hint: $hint,
            kind: $kind,
            get: $get,
            set: $set,
        }
    };
}

macro_rules! define_fields {
    ($($id:ident : $maker:ident ( $($args:tt)* ) ;)*) => {
        /// An option of the Settings window.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum FieldId {
            $(
                #[doc = stringify!($id)]
                $id,
            )*
        }

        impl FieldId {
            /// Every option, in page order.
            pub const ALL: &'static [FieldId] = &[$(FieldId::$id,)*];
        }

        /// The definitions, parallel to [`FieldId::ALL`].
        pub(crate) static FIELDS: &[FieldDef] = &[$($maker!($id, $($args)*)),*];
    };
}

define_fields! {
    Language: custom(
        General, "settings-general-language", None, FieldKind::Language,
        |s| FieldValue::Text(s.general.language.clone().unwrap_or_default()),
        |s, v| {
            let tag = v.text().trim();
            s.general.language = (!tag.is_empty()).then(|| tag.to_owned());
            Ok(())
        }
    );
    OnStartup: choice(
        General, "settings-general-startup", None, general.on_startup, StartupBehavior,
        [
            StartupBehavior::RestoreSession => "settings-opt-startup-restore",
            StartupBehavior::NewDocument => "settings-opt-startup-new",
            StartupBehavior::DefaultFolder => "settings-opt-startup-folder",
        ]
    );
    DefaultFolder: custom(
        General, "settings-general-default-folder", None, FieldKind::Folder,
        |s| FieldValue::Text(s.general.default_folder.clone().unwrap_or_default()),
        |s, v| {
            s.general.default_folder = optional_path(v.text())?;
            Ok(())
        }
    );
    Autosave: toggle(General, "settings-general-autosave", None, general.autosave);
    AutosaveDelay: whole(
        General, "settings-general-autosave-delay", Some("settings-general-autosave-delay-hint"),
        general.autosave_delay_ms, 100.0, 60000.0
    );
    NewFileLocation: choice(
        General, "settings-general-new-file-location", None, general.new_file_location,
        NewFileLocation,
        [
            NewFileLocation::SelectedFolder => "settings-opt-new-file-selected",
            NewFileLocation::DefaultFolder => "settings-opt-new-file-default",
        ]
    );
    SyncFileName: toggle(General, "settings-general-sync-file-name", None, general.sync_file_name_with_heading);
    CheckForUpdates: toggle(General, "settings-general-check-updates", None, general.check_for_updates);

    Theme: choice(
        Appearance, "settings-appearance-theme", None, appearance.theme, Theme,
        [
            Theme::System => "settings-opt-theme-system",
            Theme::Light => "settings-opt-theme-light",
            Theme::Dark => "settings-opt-theme-dark",
        ]
    );
    Accent: choice(
        Appearance, "settings-appearance-accent", None, appearance.accent, AccentMode,
        [
            AccentMode::System => "settings-opt-accent-system",
            AccentMode::Blue => "settings-opt-accent-blue",
            AccentMode::Custom => "settings-opt-accent-custom",
        ]
    );
    AccentCustom: custom(
        Appearance, "settings-appearance-accent-custom", Some("settings-appearance-accent-custom-hint"),
        FieldKind::Text,
        |s| FieldValue::Text(s.appearance.accent_custom.clone().unwrap_or_default()),
        |s, v| {
            s.appearance.accent_custom = optional_color(v.text())?;
            Ok(())
        }
    );
    Mica: toggle(Appearance, "settings-appearance-mica", None, appearance.mica);
    SidebarFontSize: decimal(
        Appearance, "settings-appearance-sidebar-font-size", Some("settings-hint-dips"),
        appearance.sidebar_font_size, 9.0, 24.0
    );
    ShowStatusBar: toggle(Appearance, "settings-appearance-status-bar", None, appearance.show_status_bar);
    ShowFormatPill: toggle(Appearance, "settings-appearance-format-pill", None, appearance.show_format_pill);
    ZoomPercent: whole(
        Appearance, "settings-appearance-zoom", Some("settings-hint-percent"),
        appearance.zoom_percent, 25.0, 500.0
    );

    TextFont: font("settings-editor-text-font", text_font);
    HeadingFont: font("settings-editor-heading-font", heading_font);
    CodeFont: font("settings-editor-code-font", code_font);
    FontSize: decimal(
        Editor, "settings-editor-font-size", Some("settings-hint-dips"), editor.font_size, 8.0, 48.0
    );
    LineHeight: decimal(
        Editor, "settings-editor-line-height", Some("settings-editor-line-height-hint"),
        editor.line_height, 1.0, 3.0
    );
    ParagraphSpacing: decimal(
        Editor, "settings-editor-paragraph-spacing", Some("settings-editor-paragraph-spacing-hint"),
        editor.paragraph_spacing, 0.0, 3.0
    );
    LineWidth: whole(
        Editor, "settings-editor-line-width", Some("settings-hint-dips"), editor.line_width, 320.0, 2400.0
    );
    ListIndent: choice(
        Editor, "settings-editor-list-indent", None, editor.list_indent, ListIndent,
        [
            ListIndent::Align => "settings-opt-indent-align",
            ListIndent::Two => "settings-opt-indent-2",
            ListIndent::Four => "settings-opt-indent-4",
            ListIndent::Tab => "settings-opt-indent-tab",
        ]
    );
    AutoHideMarkdown: toggle(Editor, "settings-editor-auto-hide", None, editor.auto_hide_markdown);
    TypewriterPosition: percent(
        Editor, "settings-editor-typewriter-position", None, editor.typewriter_position, 100
    );
    TypewriterDeadZone: percent(
        Editor, "settings-editor-typewriter-dead-zone", None, editor.typewriter_dead_zone, 50
    );
    SpellCheck: toggle(Editor, "settings-editor-spell-check", None, editor.spell_check);
    SpellLanguages: custom(
        Editor, "settings-editor-spell-languages", Some("settings-editor-spell-languages-hint"),
        FieldKind::Text,
        |s| FieldValue::Text(s.editor.spell_languages.join(", ")),
        |s, v| {
            s.editor.spell_languages = language_list(v.text())?;
            Ok(())
        }
    );
    SortCompletedTasks: toggle(Editor, "settings-editor-sort-tasks", None, editor.sort_completed_tasks);
    SyntaxHighlighting: toggle(Editor, "settings-editor-syntax-highlighting", None, editor.syntax_highlighting);
    FormatTablesOnExit: toggle(Editor, "settings-editor-format-tables", None, editor.format_tables_on_exit);
    HideFrontMatter: toggle(Editor, "settings-editor-hide-front-matter", None, editor.hide_front_matter);
    ReadingSpeed: whole(
        Editor, "settings-editor-reading-speed", Some("settings-hint-wpm"), editor.reading_speed_wpm, 50.0, 1000.0
    );

    SidebarSortBy: choice(
        Sidebar, "settings-sidebar-sort", None, sidebar.sort_by, SortBy,
        [
            SortBy::Name => "settings-opt-sort-name",
            SortBy::DateModified => "settings-opt-sort-modified",
            SortBy::DateCreated => "settings-opt-sort-created",
            SortBy::Kind => "settings-opt-sort-kind",
        ]
    );
    SidebarSortDescending: toggle(Sidebar, "settings-sidebar-descending", None, sidebar.sort_descending);
    SidebarFoldersFirst: toggle(Sidebar, "settings-sidebar-folders-first", None, sidebar.folders_first);
    SidebarShowExtensions: toggle(Sidebar, "settings-sidebar-extensions", None, sidebar.show_extensions);
    SidebarShowHidden: toggle(Sidebar, "settings-sidebar-hidden", None, sidebar.show_hidden_items);
    SidebarHideImages: toggle(Sidebar, "settings-sidebar-hide-images", None, sidebar.hide_images);
    SidebarHideNonMarkdown: toggle(Sidebar, "settings-sidebar-hide-non-markdown", None, sidebar.hide_non_markdown);
    SidebarShowModified: toggle(Sidebar, "settings-sidebar-modified-date", None, sidebar.show_modified_date);

    AttachmentLocation: choice(
        Attachments, "settings-attachments-location", None, attachments.location, AttachmentLocation,
        [
            AttachmentLocation::NextToFile => "settings-opt-attach-next",
            AttachmentLocation::AssetsFolder => "settings-opt-attach-assets",
            AttachmentLocation::FileAssetsFolder => "settings-opt-attach-file-assets",
            AttachmentLocation::Custom => "settings-opt-attach-custom",
        ]
    );
    CustomFolder: custom(
        Attachments, "settings-attachments-custom-folder", None, FieldKind::Folder,
        |s| FieldValue::Text(s.attachments.custom_folder.clone()),
        |s, v| {
            let folder = optional_path(v.text())?;
            if folder.is_none() && s.attachments.location == AttachmentLocation::Custom {
                return Err(Problem::Empty);
            }
            s.attachments.custom_folder = folder.unwrap_or_default();
            Ok(())
        }
    );
    NamingTemplate: custom(
        Attachments, "settings-attachments-template", Some("settings-attachments-template-hint"),
        FieldKind::Text,
        |s| FieldValue::Text(s.attachments.naming_template.clone()),
        |s, v| {
            s.attachments.naming_template = naming_template(v.text())?;
            Ok(())
        }
    );
    LoadRemoteImages: toggle(Attachments, "settings-attachments-remote-images", None, attachments.load_remote_images);

    ExportPageSize: choice(
        Export, "settings-export-page-size", None, export.page_size, PageSize,
        [
            PageSize::A4 => "settings-opt-page-a4",
            PageSize::Letter => "settings-opt-page-letter",
        ]
    );
    ExportMargin: decimal(
        Export, "settings-export-margin", Some("settings-hint-mm"), export.margin_mm, 0.0, 100.0
    );
    ExportFontSize: decimal(
        Export, "settings-export-font-size", Some("settings-hint-pt"), export.base_font_size_pt, 6.0, 72.0
    );
    ExportPageNumbers: toggle(Export, "settings-export-page-numbers", None, export.page_numbers);
    ExportBlackText: toggle(Export, "settings-export-black-text", None, export.black_text);
    ExportLightTheme: toggle(Export, "settings-export-light-theme", None, export.light_theme);
    ExportIncludeTitle: toggle(Export, "settings-export-include-title", None, export.include_title);
    ExportDefaultFormat: choice(
        Export, "settings-export-default-format", None, export.default_format, ExportFormat,
        [
            ExportFormat::Pdf => "settings-opt-format-pdf",
            ExportFormat::Png => "settings-opt-format-png",
            ExportFormat::Jpg => "settings-opt-format-jpg",
            ExportFormat::Html => "settings-opt-format-html",
            ExportFormat::Epub => "settings-opt-format-epub",
            ExportFormat::Docx => "settings-opt-format-docx",
            ExportFormat::Odt => "settings-opt-format-odt",
            ExportFormat::Rtf => "settings-opt-format-rtf",
            ExportFormat::Txt => "settings-opt-format-txt",
            ExportFormat::MarkdownWithAttachments => "settings-opt-format-markdown",
            ExportFormat::Textbundle => "settings-opt-format-textbundle",
        ]
    );

    IndexMaxFiles: whole(
        Advanced, "settings-advanced-index-max-files", Some("settings-advanced-index-max-files-hint"),
        advanced.index_max_files, 1000.0, 10_000_000.0
    );
    HistoryRetention: whole(
        Advanced, "settings-advanced-history-retention", Some("settings-hint-days"),
        advanced.history_retention_days, 1.0, 3650.0
    );
}

impl FieldId {
    /// The definition of this field.
    pub(crate) fn def(self) -> &'static FieldDef {
        // The table is parallel to `ALL`, which lists the variants in declaration order.
        &FIELDS[self as usize]
    }

    /// The page the field is on.
    #[must_use]
    pub fn section(self) -> SettingsSection {
        self.def().section
    }

    /// Fluent id of the field's label (also its accessible name).
    #[must_use]
    pub fn label_id(self) -> &'static str {
        self.def().label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_read_as_another_kind_gives_the_neutral_answer() {
        assert!(FieldValue::Bool(true).flag());
        assert!(!FieldValue::Bool(false).flag() && !FieldValue::Choice(1).flag());
        assert_eq!(FieldValue::Choice(3).index(), 3);
        assert_eq!(FieldValue::Bool(true).index(), usize::MAX);
        assert_eq!(FieldValue::Text("Georgia".into()).text(), "Georgia");
        assert_eq!(FieldValue::Number(1.0).text(), "");
        assert_eq!(FieldValue::Number(2.5).number(), 2.5);
        assert_eq!(FieldValue::Text("2.5".into()).number(), 0.0);
    }

    #[test]
    fn a_choice_that_names_no_option_leaves_the_setting_as_it_was() {
        let mut settings = Settings::default();
        let def = FieldId::AttachmentLocation.def();
        assert_eq!(def.write(&mut settings, &FieldValue::Choice(2)), Ok(()));
        assert_eq!(def.read(&settings), FieldValue::Choice(2));
        // Past the last option, and a value of another kind: nothing is selected.
        assert_eq!(def.write(&mut settings, &FieldValue::Choice(99)), Ok(()));
        assert_eq!(def.read(&settings), FieldValue::Choice(2));
        let text = FieldValue::Text("2".into());
        assert_eq!(def.write(&mut settings, &text), Ok(()));
        assert_eq!(def.read(&settings), FieldValue::Choice(2));
    }
}
