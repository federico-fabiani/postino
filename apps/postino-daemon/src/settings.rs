//! Daemon-owned preferences persisted in `%APPDATA%\Postino\daemon.json`.
//!
//! The file is app-owned, small and has `deny_unknown_fields`: a typo is reported and the defaults
//! are used, never silently ignored. Writes go to a sibling temporary file and are replaced
//! atomically, so a crash cannot leave a half-written settings file. The daemon reads them at
//! start; the settings page and `postino daemon settings` edit them live (P10b2).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const FILE_NAME: &str = "daemon.json";
pub const MIN_DEBOUNCE_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonSettings {
    /// Show the daemon-owned tray icon.
    #[serde(default = "yes")]
    pub tray: bool,
    /// Show new-mail and send-failure notices.
    #[serde(default = "yes")]
    pub notify_enabled: bool,
    /// Include sender and subject in a new-mail notice. Off by default.
    #[serde(default)]
    pub notify_previews: bool,
    /// One notice per window; never less than [`MIN_DEBOUNCE_SECS`].
    #[serde(default = "default_debounce")]
    pub debounce_secs: u64,
}

fn yes() -> bool {
    true
}

fn default_debounce() -> u64 {
    MIN_DEBOUNCE_SECS
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self { tray: true, notify_enabled: true, notify_previews: false, debounce_secs: MIN_DEBOUNCE_SECS }
    }
}

/// A partial edit from the settings page or the CLI. Every field is optional, so one row can be
/// changed without sending the others back and racing a concurrent edit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonSettingsPatch {
    pub tray: Option<bool>,
    pub notify_enabled: Option<bool>,
    pub notify_previews: Option<bool>,
    pub debounce_secs: Option<u64>,
}

impl DaemonSettingsPatch {
    pub fn is_empty(&self) -> bool {
        self.tray.is_none() && self.notify_enabled.is_none() && self.notify_previews.is_none() && self.debounce_secs.is_none()
    }

    /// Apply the requested fields and report whether anything changed.
    pub fn apply_to(&self, settings: &mut DaemonSettings) -> bool {
        let mut changed = false;
        for (target, value) in [
            (&mut settings.tray, self.tray),
            (&mut settings.notify_enabled, self.notify_enabled),
            (&mut settings.notify_previews, self.notify_previews),
        ] {
            if let Some(value) = value {
                changed |= *target != value;
                *target = value;
            }
        }
        if let Some(value) = self.debounce_secs {
            changed |= settings.debounce_secs != value;
            settings.debounce_secs = value;
        }
        changed
    }
}

/// What the settings page reads. `tray_available` is the platform seam existing at all;
/// `tray_running` is the icon currently installed. The page can always tell the user which of the
/// two is missing instead of showing a switch that pretends to work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonSettingsView {
    pub tray: bool,
    pub notify_enabled: bool,
    pub notify_previews: bool,
    pub debounce_secs: u64,
    pub tray_available: bool,
    pub tray_running: bool,
}

impl DaemonSettings {
    /// Clamp a hand-edited or older file into the supported range.
    pub fn sanitize(&mut self) {
        self.debounce_secs = self.debounce_secs.max(MIN_DEBOUNCE_SECS);
    }

    pub fn view(&self, tray_available: bool, tray_running: bool) -> DaemonSettingsView {
        DaemonSettingsView {
            tray: self.tray,
            notify_enabled: self.notify_enabled,
            notify_previews: self.notify_previews,
            debounce_secs: self.debounce_secs,
            tray_available,
            tray_running,
        }
    }

    /// `%APPDATA%\Postino\daemon.json` on Windows, the config dir on other platforms.
    pub fn default_path() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|dirs| dirs.config_dir().join("Postino").join(FILE_NAME))
    }

    /// Read the file, or the defaults when it is missing. A malformed file never stops the daemon:
    /// the reason is logged without echoing the contents, and the defaults are returned.
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                log::warn!("daemon settings could not be read: {}", error.kind());
                return Self::default();
            }
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(mut settings) => {
                settings.sanitize();
                settings
            }
            Err(error) => {
                log::warn!("daemon settings are invalid and the defaults are in use: {error}");
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
        let Some(parent) = path.parent() else {
            return Err(SettingsError::Path("the settings path has no parent".to_owned()));
        };
        std::fs::create_dir_all(parent).map_err(|error| SettingsError::Io(error.kind()))?;
        let temp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        let mut file = OpenOptions::new().create(true).truncate(true).write(true).open(&temp).map_err(|error| SettingsError::Io(error.kind()))?;
        let encoded = serde_json::to_vec_pretty(self).map_err(|error| SettingsError::Encode(error.to_string()))?;
        file.write_all(&encoded).map_err(|error| SettingsError::Io(error.kind()))?;
        file.sync_all().map_err(|error| SettingsError::Io(error.kind()))?;
        drop(file);
        if let Err(error) = postino_platform::atomic_replace(&temp, path) {
            let _ = std::fs::remove_file(&temp);
            return Err(SettingsError::Io(error.kind()));
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SettingsError {
    #[error("daemon settings could not be read or written: {0:?}")]
    Io(std::io::ErrorKind),
    #[error("daemon settings could not be encoded: {0}")]
    Encode(String),
    #[error("invalid daemon settings path: {0}")]
    Path(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_private_and_debounced() {
        let settings = DaemonSettings::default();
        assert!(settings.tray);
        assert!(settings.notify_enabled);
        assert!(!settings.notify_previews);
        assert_eq!(settings.debounce_secs, MIN_DEBOUNCE_SECS);
    }

    #[test]
    fn round_trip_and_unknown_field_refused() {
        let Ok(dir) = tempfile::tempdir() else { return };
        let path = dir.path().join(FILE_NAME);
        let settings = DaemonSettings { tray: false, notify_enabled: true, notify_previews: true, debounce_secs: 120 };
        assert!(settings.save(&path).is_ok());
        assert_eq!(DaemonSettings::load(&path), settings);

        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        let unknown = raw.replace("\"tray\"", "\"trayy\"");
        assert!(std::fs::write(&path, unknown).is_ok());
        assert_eq!(DaemonSettings::load(&path), DaemonSettings::default());
    }

    #[test]
    fn debounce_is_clamped_and_missing_file_is_default() {
        let Ok(dir) = tempfile::tempdir() else { return };
        let path = dir.path().join(FILE_NAME);
        assert_eq!(DaemonSettings::load(&path), DaemonSettings::default());
        assert!(std::fs::write(&path, br#"{"debounce_secs": 5}"#).is_ok());
        assert_eq!(DaemonSettings::load(&path).debounce_secs, MIN_DEBOUNCE_SECS);
    }

    #[test]
    fn partial_patches_change_only_what_they_name_and_clamp_the_debounce() {
        let mut settings = DaemonSettings::default();
        let patch: DaemonSettingsPatch = serde_json::from_str(r#"{"tray":false,"debounce_secs":5}"#).unwrap();
        assert!(patch.apply_to(&mut settings));
        settings.sanitize();
        assert!(!settings.tray);
        assert!(settings.notify_enabled);
        assert_eq!(settings.debounce_secs, MIN_DEBOUNCE_SECS);

        let refused = serde_json::from_str::<DaemonSettingsPatch>(r#"{"previews":true}"#);
        assert!(refused.is_err(), "a typo must not be ignored");
        assert!(DaemonSettingsPatch::default().is_empty());
    }

    #[test]
    fn view_reports_the_tray_seam_and_the_live_icon_separately() {
        let view = DaemonSettings::default().view(true, false);
        assert!(view.tray);
        assert!(view.tray_available);
        assert!(!view.tray_running);
        assert!(!DaemonSettings::default().view(false, false).tray_available);
        let encoded = serde_json::to_value(&view).unwrap_or_default();
        for key in ["tray", "notify_enabled", "notify_previews", "debounce_secs", "tray_available", "tray_running"] {
            assert!(encoded.get(key).is_some(), "the view must carry {key}");
        }
    }
}
