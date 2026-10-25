//! The operating-system seams this slice must not call directly from a test.
//!
//! Links, the native Save dialog and the file manager are the only places the desktop shell leaves
//! Rust. Each one is a trait so a headless test can inject a recorder instead of opening a browser,
//! a native dialog or a shell. Production adapters are tiny; they are never exercised by CI.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Open an already-vetted URL in the user's browser.
pub trait LinkOpener: Send + Sync {
    fn open(&self, url: &str) -> Result<(), String>;
}

/// Ask for a destination and write bytes there. Tests inject a recorder that never shows a dialog.
pub trait FileSaver: Send + Sync {
    /// Ask for a destination. `Ok(None)` means the person cancelled.
    fn choose_path(&self, suggested_name: &str) -> Result<Option<PathBuf>, String>;

    /// Write `bytes` exactly as given. The caller has already handled overwrite confirmation.
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), String>;
}

/// Open a directory in the platform file manager.
pub trait FolderOpener: Send + Sync {
    fn open(&self, path: &Path) -> Result<(), String>;
}

/// The production browser adapter. The URL has already passed the allowlist and the confirmation
/// sheet in the UI; this layer only hands it to the operating system.
pub struct SystemLinkOpener;

impl LinkOpener for SystemLinkOpener {
    fn open(&self, url: &str) -> Result<(), String> {
        webbrowser::open(url).map_err(|error| error.to_string())
    }
}

/// The production Save dialog. It only chooses a path; the app checks overwrite and calls
/// [`FileSaver::write`] once, so every byte reaches the destination unchanged.
pub struct RfdFileSaver;

impl FileSaver for RfdFileSaver {
    fn choose_path(&self, suggested_name: &str) -> Result<Option<PathBuf>, String> {
        Ok(rfd::FileDialog::new().set_file_name(suggested_name).save_file())
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
        std::fs::write(path, bytes).map_err(|error| error.to_string())
    }
}

/// The production file-manager adapter. It invokes the platform opener directly, never a shell.
pub struct SystemFolderOpener;

impl FolderOpener for SystemFolderOpener {
    fn open(&self, path: &Path) -> Result<(), String> {
        if !path.is_dir() {
            return Err("La cartella non esiste più.".to_owned());
        }
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut command = Command::new("explorer");
            command.arg(path);
            command
        };
        #[cfg(target_os = "macos")]
        let mut command = {
            let mut command = Command::new("open");
            command.arg(path);
            command
        };
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut command = {
            let mut command = Command::new("xdg-open");
            command.arg(path);
            command
        };
        command.spawn().map(|_| ()).map_err(|error| error.to_string())
    }
}

/// A browser that is never available. Tests must inject a recorder before exercising a link; a
/// screenshot or a demo session cannot start a real browser by accident.
pub struct UnavailableLinkOpener;

impl LinkOpener for UnavailableLinkOpener {
    fn open(&self, _url: &str) -> Result<(), String> {
        Err("Apertura del browser non disponibile in questa sessione.".to_owned())
    }
}

/// A saver that always cancels. The demo and screenshot constructors use it; a test that clicks Save
/// assigns its recorder first.
pub struct CancellingFileSaver;

impl FileSaver for CancellingFileSaver {
    fn choose_path(&self, _suggested_name: &str) -> Result<Option<PathBuf>, String> {
        Ok(None)
    }

    fn write(&self, _path: &Path, _bytes: &[u8]) -> Result<(), String> {
        Err("Nessun salvataggio configurato.".to_owned())
    }
}

/// A folder opener that refuses. Used by the demo and screenshot constructors.
pub struct UnavailableFolderOpener;

impl FolderOpener for UnavailableFolderOpener {
    fn open(&self, _path: &Path) -> Result<(), String> {
        Err("Apertura della cartella non disponibile in questa sessione.".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unavailable_adapters_fail_readably_without_side_effects() {
        let link = UnavailableLinkOpener.open("https://example.com").expect_err("no browser in tests");
        assert!(link.contains("non disponibile"));
        assert_eq!(CancellingFileSaver.choose_path("a.txt").expect("cancelled"), None);
        let folder = UnavailableFolderOpener.open(Path::new(".")).expect_err("no shell in tests");
        assert!(folder.contains("non disponibile"));
    }
}
