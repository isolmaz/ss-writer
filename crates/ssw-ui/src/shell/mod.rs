//! Window shell view models: pane layout, title row, command row, tabs, sidebar, inspector,
//! status bar, banners and the dialogs/overlays that sit on top of the editor.
//!
//! One `pub mod` line per module; units append their own lines.

pub mod banners;
pub mod find;
pub mod folder_search;
pub mod goto;
pub mod inspector;
pub mod layout;
pub mod palette;
pub mod picker;
pub mod popups;
pub mod quick_open;
pub mod sidebar;
pub mod statusbar;
pub mod tabs;
pub mod titlebar;

pub mod export_dialog;
pub mod history;
pub mod settings;
