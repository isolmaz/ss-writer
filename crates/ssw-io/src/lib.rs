//! File IO on Windows: encodings, atomic saves, watching, workspace, state store.

pub mod crash;
pub mod encoding;
pub mod fileops;
pub mod fs;
pub mod logging;
pub mod recovery;
pub mod save;
pub mod watch;
pub mod workspace;

pub mod diff;
pub mod folder_search;
pub mod history;
pub mod index;
pub mod net;
pub mod paths;
pub mod remote_images;
pub mod session;
pub mod settings_file;
pub mod store;
pub mod update;
