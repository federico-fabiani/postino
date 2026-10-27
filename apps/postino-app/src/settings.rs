//! The few preferences the app owns across restarts: theme, reading-pane fraction, the last
//! folder and the opaque active account id. Nothing here may ever carry draft text, message bodies,
//! addresses or credentials.
//!
//! A missing or unreadable file is not an error the user has to see: every field has a default and
//! every loaded value is sanitised before it reaches the shell.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Bumped only when an older file can no longer be read as-is; an unknown version falls back to
/// the defaults instead of trying to guess.
pub const SETTINGS_VERSION: u32 = 1;
pub const DEFAULT_READING_PANE_RATIO: f32 = 0.52;
const MIN_READING_PANE_RATIO: f32 = 0.1;
const MAX_READING_PANE_RATIO: f32 = 0.9;
const MAX_FOLDER_ID: usize = 64;
const MAX_ACCOUNT_ID: usize = 64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub dark: bool,
    /// Follow the operating system theme instead of forcing `dark`.
    pub system_theme: bool,
    /// The reading pane as a fraction of the width left by the sidebar.
    pub reading_pane_ratio: f32,
    pub folder: String,
    pub reduced_motion: bool,
    /// The account whose mailbox was last open. Opaque id only, never an address.
    pub account: Option<String>,
    /// The first close-to-tray explanation was shown and accepted. Persisted so it is never repeated.
    pub tray_notice_shown: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            dark: false,
            system_theme: false,
            reading_pane_ratio: DEFAULT_READING_PANE_RATIO,
            folder: "inbox".to_owned(),
            reduced_motion: false,
            account: None,
            tray_notice_shown: false,
        }
    }
}

impl Settings {
    /// Replace anything a hand-edited or truncated file could carry with a safe default.
    #[must_use]
    pub fn sanitised(&self) -> Self {
        let mut settings = self.clone();
        if settings.version != SETTINGS_VERSION {
            return Self::default();
        }
        if !settings.reading_pane_ratio.is_finite() || !(MIN_READING_PANE_RATIO..=MAX_READING_PANE_RATIO).contains(&settings.reading_pane_ratio) {
            settings.reading_pane_ratio = DEFAULT_READING_PANE_RATIO;
        }
        settings.folder = settings.folder.trim().to_owned();
        if settings.folder.is_empty() || settings.folder.len() > MAX_FOLDER_ID || settings.folder.chars().any(|character| character.is_control()) {
            settings.folder = "inbox".to_owned();
        }
        let account = settings.account.take().and_then(|account| {
            let account = account.trim().to_owned();
            if account.is_empty()
                || account.len() > MAX_ACCOUNT_ID
                || !account.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                None
            } else {
                Some(account)
            }
        });
        settings.account = account;
        settings
    }

    /// Read the settings at `path`. Any failure — absent file, invalid JSON, an unreadable
    /// directory — falls back to the defaults without an error dialog or a log line that could
    /// contain the user's data.
    #[must_use]
    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str::<Self>(&text).map_or_else(|_| Self::default(), |settings| settings.sanitised())
    }

    /// Write the settings atomically: a temporary file in the same directory is renamed over the
    /// destination, so a failed write never leaves a half-written settings file behind.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut temp_os = path.as_os_str().to_os_string();
        temp_os.push(".tmp");
        let temp = PathBuf::from(temp_os);
        let text = serde_json::to_string_pretty(&self.sanitised()).map_err(std::io::Error::other)?;
        if let Err(error) = std::fs::write(&temp, text) {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&temp, path) {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
        Ok(())
    }

    /// Where a normal launch keeps its preferences. `None` only when the platform exposes no
    /// configuration directory at all, in which case the session simply runs on the defaults.
    #[must_use]
    pub fn path() -> Option<PathBuf> {
        config_dir().map(|directory| directory.join("Postino").join("settings.json"))
    }
}

fn config_dir() -> Option<PathBuf> {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(appdata));
    }
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(config));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
}

/// `POSTINO_REDUCE_MOTION=1` turns animations off for a session, for people who do not want to
/// hunt for a preference and for the accessibility review. Any other value leaves motion on.
#[must_use]
pub fn reduced_motion_from_env() -> bool {
    std::env::var("POSTINO_REDUCE_MOTION").is_ok_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

#[cfg(test)]
pub(crate) fn test_directory() -> PathBuf {
    let directory = std::env::temp_dir().join("opencode").join("p2b-postino");
    let _ = std::fs::create_dir_all(&directory);
    directory
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> PathBuf {
        test_directory().join(format!("{name}-{}.json", std::process::id()))
    }

    #[test]
    fn the_defaults_match_the_documented_first_launch() {
        let settings = Settings::default();
        assert_eq!(settings.version, SETTINGS_VERSION);
        assert!(!settings.dark);
        assert!(!settings.system_theme);
        assert_eq!(settings.reading_pane_ratio, 0.52);
        assert_eq!(settings.folder, "inbox");
        assert!(!settings.reduced_motion);
        assert_eq!(settings.account, None);
    }

    #[test]
    fn a_saved_file_round_trips() {
        let path = path("round-trip");
        let saved = Settings {
            dark: true,
            system_theme: true,
            reading_pane_ratio: 0.47,
            folder: "archive".to_owned(),
            reduced_motion: true,
            account: Some("acc_0123456789abcdef".to_owned()),
            ..Settings::default()
        };
        saved.save_to(&path).expect("the settings file");
        assert_eq!(Settings::load_from(&path), saved);
        assert!(!PathBuf::from(format!("{}.tmp", path.display())).exists(), "the atomic write must clean its temp file");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_or_corrupt_file_falls_back_to_the_defaults() {
        let missing = path("missing");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(Settings::load_from(&missing), Settings::default());

        let corrupt = path("corrupt");
        std::fs::write(&corrupt, "{ this is not json").expect("a corrupt file");
        assert_eq!(Settings::load_from(&corrupt), Settings::default());
        let _ = std::fs::remove_file(&corrupt);

        let truncated = path("truncated");
        std::fs::write(&truncated, r#"{"dark": true, "reading_pane_ratio": 0.0, "folder": ""}"#).expect("a partial file");
        let loaded = Settings::load_from(&truncated);
        assert!(loaded.dark, "a valid field is kept");
        assert_eq!(loaded.reading_pane_ratio, DEFAULT_READING_PANE_RATIO, "an impossible width falls back");
        assert_eq!(loaded.folder, "inbox", "an empty folder falls back");
        let _ = std::fs::remove_file(&truncated);
    }

    #[test]
    fn a_future_schema_falls_back_instead_of_guessing() {
        let settings = Settings { version: SETTINGS_VERSION + 1, dark: true, ..Settings::default() };
        assert_eq!(settings.sanitised(), Settings::default());
    }

    #[test]
    fn the_file_only_contains_preferences() {
        let path = path("no-mail");
        Settings { dark: true, ..Settings::default() }.save_to(&path).expect("the settings file");
        let text = std::fs::read_to_string(&path).expect("the settings file");
        for forbidden in ["\"body\"", "\"subject\"", "\"to\"", "\"cc\"", "\"bcc\"", "password", "attachment"] {
            assert!(!text.contains(forbidden), "settings must not persist {forbidden}: {text}");
        }
        let fields = serde_json::from_str::<serde_json::Value>(&text).expect("valid JSON");
        let keys = fields.as_object().map(|object| object.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
        assert_eq!(keys.len(), 8, "only the eight documented settings: {keys:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_invalid_account_id_is_dropped() {
        let long = "a".repeat(65);
        for account in ["", "with space", "a:b", "../x", long.as_str()] {
            let settings = Settings { account: Some((*account).to_owned()), ..Settings::default() };
            assert_eq!(settings.sanitised().account, None, "{account:?}");
        }
    }
}
