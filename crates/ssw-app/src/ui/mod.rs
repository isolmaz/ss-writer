//! The visible shell of SS Writer: the window content, its controller and the menus.
//!
//! - [`tree_source`]: the sidebar's tree over `ssw_io::workspace`.
//! - [`menus`]: the File / Edit / Format / View popup menus built from command ids.
//! - [`shell_view`]: the composed window content (layout, title row with tabs, sidebar, one
//!   editor per tab, banners, status bar), driven by [`ssw_ui::input::Event`]s.
//! - [`controller`]: windows, tabs and command execution ([`crate::commands::AppActions`]).
//! - [`banner_flows`]: which banners a window shows, what their buttons do, the recovery offer
//!   and the check of the open files on window activation.
//! - [`history`]: the version history panel and the comparison with the file on disk.
//! - [`perf_driver`]: the scripted performance scenarios behind `--perf-scenario`.
//! - [`test_script`] and [`scripted_services`]: the scripted end-to-end harness behind
//!   `--test-script` (the real controller, no native windows, scripted dialogs and shell).
//! - [`screenshot`]: offscreen rendering of the window content into PNG (`screenshot` steps).
//!
//! Window callbacks only push [`controller::Request`]s; the owner of the message loop drains
//! them outside of any window procedure.

pub mod banner_flows;
pub mod clipboard;
pub mod context_menus;
pub mod controller;
pub mod export;
pub mod files;
pub mod folds;
pub mod help;
pub mod history;
pub mod host;
pub mod import;
pub mod inspector;
pub mod menus;
pub mod overlays;
pub mod perf_driver;
pub mod platform;
pub mod remote_images;
pub mod screenshot;
pub mod scripted_services;
pub mod settings_window;
pub mod shell_view;
pub mod test_script;
pub mod tree_source;
pub mod window;
