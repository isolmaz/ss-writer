//! SS Writer application core: everything the executable does that is not pixels.
//!
//! - [`args`]: command line, launch description, test flags.
//! - [`startup`]: Windows build check, single-instance hook, device creation in parallel with
//!   settings / session load, logging and crash handler.
//! - [`workers`]: the job pool with generations, per-window cancellation and panic isolation.
//! - [`documents`]: the registry of shared open documents, load / import / save / reload.
//! - [`autosave`] and [`emergency`]: save scheduling, backups, snapshots, crash copy.
//! - [`session`]: session restore plan, persistence, reopen-closed-tab, first-run workspace.
//! - [`commands`]: command categories and the [`commands::AppActions`] seam to the shell.
//! - [`i18n`]: embedded Fluent files and locale selection.
//! - [`app`]: [`app::AppCore`], the composition of the above, and [`app::run`].
//! - [`perf`]: the marks and the JSON report of `--perf-report` runs (driven by
//!   [`ui::perf_driver`]).
//!
//! The visible shell (windows, tabs, editor view, panels) is a later wave: it implements
//! [`app::ShellHost`], [`commands::AppActions`], [`startup::InstanceHook`] and
//! [`workers::Waker`].

pub mod app;
pub mod args;
pub mod autosave;
pub mod clock;
pub mod commands;
pub mod documents;
pub mod emergency;
pub mod i18n;
pub mod perf;
pub mod session;
pub mod startup;
pub mod ui;
pub mod workers;
