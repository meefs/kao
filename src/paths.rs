//! Resolved on-disk locations for kao's config and data, with cross-platform
//! fallbacks via the `dirs` crate.
//!
//! - **config**: `$XDG_CONFIG_HOME/kao/` on Linux, `~/Library/Application Support/kao/`
//!   on macOS, `%APPDATA%\kao\` on Windows.
//! - **data**: `$XDG_DATA_HOME/kao/` on Linux, `~/Library/Application Support/kao/`
//!   on macOS, `%APPDATA%\kao\` on Windows.
//!
//! When the platform helper can't resolve a base dir (no HOME, no APPDATA),
//! both fall through to `./kao/` so the app still runs out of CWD.

use std::path::PathBuf;

const APP_DIR: &str = "kao";

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR)
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR)
}
