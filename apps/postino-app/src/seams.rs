//! The operating-system seams this slice must not call directly from a test.
//!
//! Links, the native Save/Open dialogs and the file manager are the only places the desktop shell
//! leaves Rust. Each one is a trait so a headless test can inject a recorder instead of opening a
//! browser, a native dialog or a shell. Production adapters are tiny; they are never exercised by CI.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Open an already-vetted URL in the user's browser.
pub trait LinkOpener: Send + Sync {
    fn open(&self, url: &str) -> Result<(), String>;
}

/// Ask the person for one or more files to attach. Tests inject a recorder that never shows a
/// dialog; the demo injects an unavailable picker so a stray click cannot read the filesystem.
pub trait FilePicker: Send + Sync {
    /// `Ok(vec![])` means the person cancelled. An empty list adds nothing and is not an error.
    fn choose_files(&self) -> Result<Vec<PathBuf>, String>;
}

/// Ask for a destination and write bytes there. Tests inject a recorder that never shows a dialog.
pub trait FileSaver: Send + Sync {
    /// Ask for a destination. `Ok(None)` means the person cancelled.
    fn choose_path(&self, suggested_name: &str) -> Result<Option<PathBuf>, String>;

    /// Write `bytes` exactly as given, replacing the file. The caller has already handled overwrite
    /// confirmation for this exact path.
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), String>;

    /// Write `bytes` only when `path` does not exist. This closes the window between the overwrite
    /// check and the write: a file created by another process makes the call fail instead of being
    /// silently replaced.
    fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<(), String>;
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

/// The production multi-file picker. It only chooses paths; the app bounds each file and hands the
/// bytes to the engine, which owns the encrypted blob store.
pub struct RfdFilePicker;

impl FilePicker for RfdFilePicker {
    fn choose_files(&self) -> Result<Vec<PathBuf>, String> {
        Ok(rfd::FileDialog::new().pick_files().unwrap_or_default())
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
        // Write the replacement to a sibling temp file and rename it over the destination. A failed
        // write never truncates the original file.
        let mut temp_os = path.as_os_str().to_os_string();
        temp_os.push(format!(".{}.postino.tmp", std::process::id()));
        let temp = PathBuf::from(temp_os);
        if let Err(error) = std::fs::write(&temp, bytes) {
            let _ = std::fs::remove_file(&temp);
            return Err(error.to_string());
        }
        if let Err(error) = std::fs::rename(&temp, path) {
            let _ = std::fs::remove_file(&temp);
            return Err(error.to_string());
        }
        Ok(())
    }

    fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
        write_new_with(path, bytes, |file, bytes| {
            use std::io::Write as _;
            file.write_all(bytes)?;
            file.sync_all()
        })
    }
}

/// Create a new file, run the write, and remove the file this call created when the write fails.
/// The injected writer lets tests exercise a partial/failed write without a full disk.
fn write_new_with(path: &Path, bytes: &[u8], write: impl FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|error| error.to_string())?;
    if let Err(error) = write(&mut file, bytes) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error.to_string());
    }
    Ok(())
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

/// A browser that is never available. Tests and screenshot scenes inject it so a stray click cannot
/// start a real browser from a headless session; a normal `--demo` launch installs the real adapter.
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

    fn write_new(&self, _path: &Path, _bytes: &[u8]) -> Result<(), String> {
        Err("Nessun salvataggio configurato.".to_owned())
    }
}

/// A picker that is never available. The demo, screenshot scenes and tests inject it so a stray
/// click cannot open a native dialog or read a path without the test's explicit recorder.
pub struct UnavailableFilePicker;

impl FilePicker for UnavailableFilePicker {
    fn choose_files(&self) -> Result<Vec<PathBuf>, String> {
        Err("Scelta dei file non disponibile in questa sessione.".to_owned())
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

    fn test_directory() -> PathBuf {
        let directory = std::env::temp_dir().join("opencode").join("p6c-fix2-postino");
        let _ = std::fs::create_dir_all(&directory);
        directory
    }

    #[test]
    fn the_unavailable_adapters_fail_readably_without_side_effects() {
        let link = UnavailableLinkOpener.open("https://example.com").expect_err("no browser in tests");
        assert!(link.contains("non disponibile"));
        assert_eq!(CancellingFileSaver.choose_path("a.txt").expect("cancelled"), None);
        let picker = UnavailableFilePicker.choose_files().expect_err("no native dialog in tests");
        assert!(picker.contains("non disponibile"));
        let folder = UnavailableFolderOpener.open(Path::new(".")).expect_err("no shell in tests");
        assert!(folder.contains("non disponibile"));
    }

    #[test]
    fn a_failed_new_write_removes_its_partial_file() {
        let path = test_directory().join(format!("partial-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let result = write_new_with(&path, b"partial bytes", |file, bytes| {
            use std::io::Write as _;
            file.write_all(&bytes[..4])?;
            Err(std::io::Error::other("disk full"))
        });
        assert!(result.is_err());
        assert!(!path.exists(), "a failed write_new must remove the file it created");
    }

    #[test]
    fn overwrite_writes_through_a_temp_file_and_keeps_the_original_on_failure() {
        let path = test_directory().join(format!("overwrite-{}.bin", std::process::id()));
        std::fs::write(&path, b"old").expect("the original file");
        RfdFileSaver.write(&path, b"new bytes").expect("the overwrite");
        assert_eq!(std::fs::read(&path).expect("the replaced file"), b"new bytes");
        let temp = PathBuf::from(format!("{}.{}.postino.tmp", path.display(), std::process::id()));
        assert!(!temp.exists(), "the temp file must be renamed away");

        // A directory at the temp path makes the temp write fail before the rename. The original
        // must stay intact.
        std::fs::create_dir(&temp).expect("the blocking directory");
        let result = RfdFileSaver.write(&path, b"replacement");
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).expect("the original survives"), b"new bytes");
        let _ = std::fs::remove_dir(&temp);
        let _ = std::fs::remove_file(&path);
    }
}
