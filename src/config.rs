//! Persistent, user-adjustable settings, changed from the settings TUI (`settings_tui.rs`,
//! opened by holding Super) and stored as JSON under the XDG config directory so they survive
//! daemon restarts.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// How far h/j/k/l or arrow-key nudges move the cursor per press, in pixels -- see
/// `overlay.rs`'s `NUDGE_STEP` default and the menu's +/- adjustment.
const DEFAULT_NUDGE_STEP: f64 = 20.0;

/// Bounds for `nudge_step`, enforced by the menu's +/- handling -- below the low end a nudge
/// is imperceptible, above the high end it overshoots most targets in a single press.
pub const NUDGE_STEP_MIN: f64 = 5.0;
pub const NUDGE_STEP_MAX: f64 = 200.0;
pub const NUDGE_STEP_INCREMENT: f64 = 5.0;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub nudge_step: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self { nudge_step: DEFAULT_NUDGE_STEP }
    }
}

fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "omakeys")
        .map(|dirs| dirs.config_dir().join("config.json"))
}

/// Load settings from disk, falling back to defaults if the file doesn't exist yet or fails
/// to parse (e.g. a hand-edited or stale file from an older version) rather than erroring --
/// a broken settings file shouldn't stop the daemon from starting.
pub fn load() -> Settings {
    let Some(path) = config_path() else {
        log::warn!("omakeys: could not determine config directory, using default settings");
        return Settings::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|e| {
            log::warn!("omakeys: config at {} failed to parse ({e}), using defaults", path.display());
            Settings::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(e) => {
            log::warn!("omakeys: could not read config at {} ({e}), using defaults", path.display());
            Settings::default()
        }
    }
}

/// Persist settings to disk, creating the config directory if needed.
pub fn save(settings: &Settings) -> Result<()> {
    let path = config_path().context("could not determine config directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(settings)?;
    std::fs::write(&path, json).with_context(|| format!("writing config to {}", path.display()))
}
