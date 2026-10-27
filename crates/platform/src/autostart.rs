//! The autostart seam: an opt-in, per-user, reversible switch that never installs a service.
//!
//! Windows stores the command as the `Postino` value under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. The registry Run key is a deliberate
//! deviation from the Task Scheduler proposed by the P10 brief: it needs no COM, no admin right and
//! no background service, and the current state can be read back exactly. ADR-0006 records it.
//! Tests only ever write a throwaway key under `HKCU\Software\Postino\Tests`.

use std::path::Path;

use thiserror::Error;

/// The registry key and value used on Windows.
pub const AUTOSTART_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const AUTOSTART_VALUE: &str = "Postino";

/// What the settings page and `postino daemon autostart status` show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AutostartStatus {
    pub enabled: bool,
    /// The exact command stored for this user, when enabled.
    pub command: Option<String>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AutostartError {
    /// The numeric Win32 code is kept in `code` so a caller can distinguish `ERROR_ACCESS_DENIED`
    /// (5) from a real failure without parsing an English message.
    #[error("autostart is not available: {message}")]
    Unavailable { code: Option<u32>, message: String },
    #[error("autostart is not supported on this platform")]
    Unsupported,
    #[error("invalid autostart command: {0}")]
    InvalidCommand(String),
}

/// The per-user autostart seam. `status` never mutates; `enable` and `disable` are idempotent.
pub trait Autostart: Send + Sync {
    fn status(&self) -> Result<AutostartStatus, AutostartError>;
    fn enable(&self) -> Result<AutostartStatus, AutostartError>;
    fn disable(&self) -> Result<AutostartStatus, AutostartError>;
}

/// Build the exact command stored in the Run key: the daemon binary and its store, quoted.
pub fn autostart_command(executable: &Path, store: &Path) -> Result<String, AutostartError> {
    let executable = executable.to_str().ok_or_else(|| AutostartError::InvalidCommand("the daemon path is not valid UTF-16 text".to_owned()))?;
    let store = store.to_str().ok_or_else(|| AutostartError::InvalidCommand("the store path is not valid UTF-16 text".to_owned()))?;
    if executable.contains('"') || store.contains('"') {
        return Err(AutostartError::InvalidCommand("paths must not contain a double quote".to_owned()));
    }
    if executable.is_empty() || store.is_empty() {
        return Err(AutostartError::InvalidCommand("paths must not be empty".to_owned()));
    }
    Ok(format!("\"{executable}\" --store \"{store}\""))
}

/// Registry-backed autostart on Windows; unsupported elsewhere, never a fallback file.
#[derive(Debug, Clone)]
pub struct RegistryAutostart {
    key: String,
    value: String,
    command: String,
}

impl RegistryAutostart {
    pub fn for_current_user(command: impl Into<String>) -> Self {
        Self { key: AUTOSTART_KEY.to_owned(), value: AUTOSTART_VALUE.to_owned(), command: command.into() }
    }

    /// Same contract under an explicit HKCU key. Tests and the CLI test hook use a throwaway key;
    /// production always calls [`RegistryAutostart::for_current_user`].
    pub fn for_key(command: impl Into<String>, key: impl Into<String>) -> Self {
        Self { key: key.into(), value: AUTOSTART_VALUE.to_owned(), command: command.into() }
    }

    fn read(&self) -> Result<AutostartStatus, AutostartError> {
        #[cfg(windows)]
        {
            let value = crate::win32::registry_read_string(&self.key, &self.value).map_err(unavailable)?;
            Ok(AutostartStatus { enabled: value.is_some(), command: value })
        }
        #[cfg(not(windows))]
        {
            let _ = (&self.key, &self.value);
            Err(AutostartError::Unsupported)
        }
    }
}

#[cfg(windows)]
fn unavailable(error: crate::win32::RegistryError) -> AutostartError {
    AutostartError::Unavailable { code: error.code, message: error.message }
}

impl Autostart for RegistryAutostart {
    fn status(&self) -> Result<AutostartStatus, AutostartError> {
        self.read()
    }

    fn enable(&self) -> Result<AutostartStatus, AutostartError> {
        #[cfg(windows)]
        {
            crate::win32::registry_write_string(&self.key, &self.value, &self.command).map_err(unavailable)?;
            let status = self.read()?;
            if status.command.as_deref() != Some(self.command.as_str()) {
                return Err(AutostartError::Unavailable { code: None, message: "the stored command did not read back".to_owned() });
            }
            Ok(status)
        }
        #[cfg(not(windows))]
        {
            let _ = &self.command;
            Err(AutostartError::Unsupported)
        }
    }

    fn disable(&self) -> Result<AutostartStatus, AutostartError> {
        #[cfg(windows)]
        {
            crate::win32::registry_delete_value(&self.key, &self.value).map_err(unavailable)?;
            self.read()
        }
        #[cfg(not(windows))]
        {
            Err(AutostartError::Unsupported)
        }
    }
}

/// Headless autostart: in-memory state with the same read-back contract as the registry.
#[derive(Debug, Default)]
pub struct RecorderAutostart {
    status: std::sync::Mutex<AutostartStatus>,
    fail: std::sync::atomic::AtomicBool,
}

impl RecorderAutostart {
    pub fn new() -> Self {
        Self::default()
    }

    /// A vault whose writes are denied, standing in for `ERROR_ACCESS_DENIED`.
    pub fn failing() -> Self {
        let recorder = Self::default();
        recorder.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        recorder
    }

    fn unavailable(&self) -> Option<AutostartError> {
        self.fail
            .load(std::sync::atomic::Ordering::SeqCst)
            .then(|| AutostartError::Unavailable { code: Some(5), message: "access denied".to_owned() })
    }
}

impl Autostart for RecorderAutostart {
    fn status(&self) -> Result<AutostartStatus, AutostartError> {
        self.status.lock().map(|status| status.clone()).map_err(|_| AutostartError::Unavailable { code: None, message: "state poisoned".to_owned() })
    }

    fn enable(&self) -> Result<AutostartStatus, AutostartError> {
        if let Some(error) = self.unavailable() {
            return Err(error);
        }
        let status = AutostartStatus { enabled: true, command: Some("postino-daemon --store test".to_owned()) };
        if let Ok(mut current) = self.status.lock() {
            *current = status.clone();
        }
        Ok(status)
    }

    fn disable(&self) -> Result<AutostartStatus, AutostartError> {
        if let Some(error) = self.unavailable() {
            return Err(error);
        }
        let status = AutostartStatus { enabled: false, command: None };
        if let Ok(mut current) = self.status.lock() {
            *current = status.clone();
        }
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_quotes_binary_and_store() {
        let command = autostart_command(Path::new(r"C:\Program Files\Postino\postino-daemon.exe"), Path::new(r"C:\Users\a b\mail.db"));
        assert_eq!(command.ok().as_deref(), Some("\"C:\\Program Files\\Postino\\postino-daemon.exe\" --store \"C:\\Users\\a b\\mail.db\""));
    }

    #[test]
    fn command_refuses_quotes() {
        assert!(matches!(autostart_command(Path::new("postino-daemon.exe"), Path::new("mail\"db")), Err(AutostartError::InvalidCommand(_))));
    }

    #[test]
    fn recorder_round_trip() {
        let autostart = RecorderAutostart::new();
        assert_eq!(autostart.status().ok(), Some(AutostartStatus { enabled: false, command: None }));
        assert!(autostart.enable().is_ok_and(|status| status.enabled));
        assert!(autostart.disable().is_ok_and(|status| !status.enabled));
    }

    #[test]
    fn denied_autostart_leaves_state_unchanged() {
        let autostart = RecorderAutostart::failing();
        assert!(autostart.enable().is_err());
        assert_eq!(autostart.status().ok(), Some(AutostartStatus { enabled: false, command: None }));
        assert!(autostart.disable().is_err());
        assert_eq!(autostart.status().ok(), Some(AutostartStatus { enabled: false, command: None }));
    }

    #[cfg(windows)]
    #[test]
    fn registry_round_trip_uses_test_key_only() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let key = format!("Software\\Postino\\Tests\\{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst));
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = crate::win32::registry_delete_tree(&self.0);
            }
        }
        let _cleanup = Cleanup(key.clone());
        let autostart = RegistryAutostart::for_key("postino-daemon.exe --store test", key);
        assert_eq!(autostart.status().ok(), Some(AutostartStatus { enabled: false, command: None }));
        let enabled = match autostart.enable() {
            // A locked-down account cannot create a key under HKCU; only the numeric
            // `ERROR_ACCESS_DENIED` code skips the write half, any other code fails the test.
            Err(AutostartError::Unavailable { code: Some(5), .. }) => return,
            Err(error) => panic!("autostart enable failed: {error}"),
            Ok(status) => status,
        };
        assert!(enabled.enabled);
        assert_eq!(autostart.status().ok().and_then(|status| status.command).as_deref(), Some("postino-daemon.exe --store test"));
        assert!(autostart.disable().is_ok_and(|status| !status.enabled));
        assert_eq!(autostart.status().ok(), Some(AutostartStatus { enabled: false, command: None }));
    }
}
