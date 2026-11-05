//! The update check and the optional in-app install, shared by the desktop app, the CLI and MCP.
//!
//! [`check`] (exposed as `update_check`) reads the latest published release tag from GitHub and
//! compares it with the running build. It is read-only, bounded and honest: it downloads nothing
//! and installs nothing. [`install`] (exposed as `update_install`) is the only operation that
//! writes: it downloads the MSI of that release, refuses it unless its SHA-256 matches the published
//! manifest, then hands it to `msiexec`. Both run on a worker thread, never in a UI frame.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use postino_net::release::{RELEASES_PAGE, Release, ReleaseError, is_newer};

/// The installer is capped here; the real MSI is around 35 MiB.
pub const MAX_INSTALLER_BYTES: u64 = 200 << 20;

/// What the running build is, what is published, and whether the second is newer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UpdateStatus {
    /// The version this build reports, for example `0.1.0`.
    pub current: String,
    /// The tag of the latest published release, for example `v0.2.0`.
    pub latest: String,
    /// `latest` names a version newer than `current`.
    pub available: bool,
    /// The release page to open. Never a download URL, and never a page outside the project.
    pub url: String,
}

/// What an install left behind: the release it came from and the installer it started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Installed {
    /// The release tag the installer came from, for example `v0.2.0`.
    pub version: String,
    /// The verified installer on disk.
    pub path: String,
}

/// Asks GitHub for the latest published release and compares it with this build.
///
/// Blocking (10 s at most): call it from a worker thread.
pub fn check() -> Result<UpdateStatus, ReleaseError> {
    let release = postino_net::release::latest()?;
    let current = env!("CARGO_PKG_VERSION").to_owned();
    let available = is_newer(&release.version, &current);
    Ok(UpdateStatus { current, latest: release.version, available, url: release.url })
}

/// Downloads the Windows installer of the latest release, verifies it and starts it.
///
/// Refuses a release that is not newer than this build, an installer whose SHA-256 does not match
/// `SHA256SUMS.txt` of the same release, and anything outside this project's release downloads.
/// Blocking (a 35 MiB download): call it from a worker thread. On Windows the setup runs detached,
/// so the caller is expected to exit right after: Windows Installer asks the Restart Manager to
/// close this program and its daemon when it needs the files.
pub fn install() -> Result<Installed, ReleaseError> {
    let release = postino_net::release::latest()?;
    if !is_newer(&release.version, env!("CARGO_PKG_VERSION")) {
        return Err(ReleaseError::NoRelease);
    }
    let msi = release.assets.iter().find(|asset| asset.name.ends_with("-windows-x64.msi")).ok_or(ReleaseError::NoInstaller)?;
    let manifest = release.assets.iter().find(|asset| asset.name == "SHA256SUMS.txt").ok_or(ReleaseError::NoInstaller)?;
    let (sums, _) = postino_net::release::download(&manifest.url, 64 << 10)?;
    let sums = String::from_utf8(sums).map_err(|_| ReleaseError::Unreadable("checksum manifest".into()))?;
    let expected =
        postino_net::release::hash_for(&sums, &msi.name).ok_or_else(|| ReleaseError::Download("the manifest does not name the installer".into()))?;
    let (bytes, got) = postino_net::release::download(&msi.url, MAX_INSTALLER_BYTES)?;
    if !got.eq_ignore_ascii_case(&expected) {
        return Err(ReleaseError::Checksum);
    }
    let directory = updates_dir()?;
    std::fs::create_dir_all(&directory).map_err(|error| ReleaseError::Io(error.to_string()))?;
    let path = directory.join(&msi.name);
    std::fs::write(&path, &bytes).map_err(|error| ReleaseError::Io(error.to_string()))?;
    start_installer(&path)?;
    Ok(Installed { version: release.version, path: path.display().to_string() })
}

/// Where the verified installers are kept: `%LOCALAPPDATA%\Postino\updates`, so an uninstall that
/// deletes local data takes them too.
fn updates_dir() -> Result<PathBuf, ReleaseError> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    Ok(base.join("Postino").join("updates"))
}

#[cfg(windows)]
fn start_installer(path: &Path) -> Result<(), ReleaseError> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    std::process::Command::new("msiexec.exe")
        .raw_arg(format!("/i \"{}\" /qb /norestart", path.display()))
        .creation_flags(DETACHED_PROCESS)
        .spawn()
        .map(|_| ())
        .map_err(|error| ReleaseError::Io(error.to_string()))
}

#[cfg(not(windows))]
fn start_installer(_path: &Path) -> Result<(), ReleaseError> {
    Err(ReleaseError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_tag_is_available_and_an_older_one_is_not() {
        let current = env!("CARGO_PKG_VERSION");
        let status = UpdateStatus {
            current: current.to_owned(),
            latest: "v99.0.0".to_owned(),
            available: is_newer("v99.0.0", current),
            url: RELEASES_PAGE.to_owned(),
        };
        assert!(status.available);
        assert!(status.url.starts_with(RELEASES_PAGE));

        let same = UpdateStatus {
            current: current.to_owned(),
            latest: current.to_owned(),
            available: is_newer(current, current),
            url: RELEASES_PAGE.to_owned(),
        };
        assert!(!same.available);
    }
}
