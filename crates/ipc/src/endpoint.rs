//! Per-user, per-store endpoint names and the token path that belongs to them.
//!
//! The name is derived from the current user id and a hash of the absolute store path. Two stores in
//! one directory therefore get different pipes/sockets, and two users can never collide.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{IpcError, platform};

/// A local endpoint. There is deliberately no TCP variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "path")]
pub enum Endpoint {
    #[serde(rename = "named_pipe")]
    NamedPipe(String),
    #[serde(rename = "unix_socket")]
    UnixSocket(PathBuf),
}

impl Endpoint {
    /// The endpoint for one store and the current user.
    pub fn for_store(store: &Path) -> Result<Self, IpcError> {
        let path = canonical_store_path(store);
        let store_hash = hash_name(&path.to_string_lossy(), 16);
        let user_hash = hash_name(&platform::current_user_key().map_err(|error| IpcError::Configuration(error.to_string()))?, 16);
        #[cfg(windows)]
        {
            let _ = user_hash;
            // The SID itself is long and may contain characters Windows rejects in a pipe name; the
            // hash is stable for the user and the ACL carries the actual SID.
            Ok(Self::NamedPipe(format!(r"\\.\pipe\postino-{user_hash}-{store_hash}")))
        }
        #[cfg(unix)]
        {
            let directory = platform::runtime_dir().map_err(|error| IpcError::Configuration(error.to_string()))?;
            let path = directory.join(format!("postino-{user_hash}-{store_hash}.sock"));
            if path.to_string_lossy().len() > 107 {
                return Err(IpcError::Configuration(format!(
                    "the Unix socket path {} is longer than 107 bytes; set XDG_RUNTIME_DIR to a shorter directory",
                    path.display()
                )));
            }
            Ok(Self::UnixSocket(path))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (user_hash, store_hash);
            Err(IpcError::Configuration("Postino IPC is implemented on Windows and Unix only".to_owned()))
        }
    }

    /// A test or diagnostic endpoint. The caller is responsible for choosing a name nobody else
    /// uses; production code always uses [`Endpoint::for_store`].
    pub fn named_pipe(name: impl Into<String>) -> Result<Self, IpcError> {
        let name = name.into();
        if !name.starts_with(r"\\.\pipe\") || name.len() > 200 || name.contains('\0') {
            return Err(IpcError::Configuration("a named pipe must be a \\\\.\\pipe\\ name of at most 200 characters".to_owned()));
        }
        Ok(Self::NamedPipe(name))
    }

    pub fn unix_socket(path: impl Into<PathBuf>) -> Result<Self, IpcError> {
        let path = path.into();
        if !path.is_absolute() || path.to_string_lossy().contains('\0') || path.to_string_lossy().len() > 100 {
            return Err(IpcError::Configuration("a Unix socket must be an absolute path of at most 100 characters".to_owned()));
        }
        Ok(Self::UnixSocket(path))
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::NamedPipe(_) => "named_pipe",
            Self::UnixSocket(_) => "unix_socket",
        }
    }

    pub fn is_local_only(&self) -> bool {
        true
    }

    /// A safe short description for a lock file or a status line. It never contains a token.
    pub fn describe(&self) -> String {
        match self {
            Self::NamedPipe(name) => name.clone(),
            Self::UnixSocket(path) => path.display().to_string(),
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::NamedPipe(_) => Path::new(r"\\.\pipe\"),
            Self::UnixSocket(path) => path,
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.describe())
    }
}

/// The endpoint, token file and store path a client needs. The writer lock is separate: it belongs
/// to `postino-store`, so a client can probe the store without depending on this crate's internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPaths {
    pub store: PathBuf,
    pub endpoint: Endpoint,
    pub token_file: PathBuf,
}

impl DaemonPaths {
    pub fn for_store(store: &Path) -> Result<Self, IpcError> {
        let absolute = canonical_store_path(store);
        let endpoint = Endpoint::for_store(&absolute)?;
        let mut token_name = absolute.file_name().map(|name| name.to_os_string()).unwrap_or_else(|| "mail.db".into());
        token_name.push(".token");
        let token_file = absolute.with_file_name(token_name);
        Ok(Self { store: absolute, endpoint, token_file })
    }
}

/// Canonicalise a store path before deriving endpoint/token names. This is duplicated from
/// `postino-store` on purpose: the transport crate does not depend on the store.
fn canonical_store_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    if let Ok(canonical) = std::fs::canonicalize(&absolute) {
        return normalize_windows_path(canonical);
    }
    let Some(parent) = absolute.parent() else { return absolute };
    let Some(name) = absolute.file_name() else { return absolute };
    std::fs::canonicalize(parent).map(|canonical| normalize_windows_path(canonical).join(name)).unwrap_or(absolute)
}

#[cfg(windows)]
fn normalize_windows_path(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        if let Some(unc) = rest.strip_prefix("UNC\\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        return PathBuf::from(rest);
    }
    path
}

#[cfg(not(windows))]
fn normalize_windows_path(path: PathBuf) -> PathBuf {
    path
}

fn hash_name(value: &str, hex_chars: usize) -> String {
    let hash = blake3::hash(value.as_bytes()).to_hex().to_string();
    hash.chars().take(hex_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_stores_have_different_endpoints_and_token_files() {
        let first = DaemonPaths::for_store(Path::new("one/mail.db")).expect("first");
        let second = DaemonPaths::for_store(Path::new("two/mail.db")).expect("second");
        assert_ne!(first.endpoint, second.endpoint);
        assert_ne!(first.token_file, second.token_file);
        assert_eq!(first.endpoint.kind(), if cfg!(windows) { "named_pipe" } else { "unix_socket" });
    }

    #[test]
    fn the_endpoint_is_stable_for_one_store() {
        let first = Endpoint::for_store(Path::new("a/mail.db")).expect("first");
        let second = Endpoint::for_store(Path::new("a/mail.db")).expect("second");
        assert_eq!(first, second);
    }

    #[test]
    fn there_is_no_remote_variant() {
        let endpoint = Endpoint::for_store(Path::new("a/mail.db")).expect("endpoint");
        assert!(endpoint.is_local_only());
        assert!(!format!("{endpoint:?}").contains("Tcp"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_store_derives_the_same_endpoint_and_token() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = directory.path().join("mail.db");
        std::fs::write(&store, b"x").expect("file");
        let alias = directory.path().join("alias.db");
        std::os::unix::fs::symlink(&store, &alias).expect("symlink");
        let first = DaemonPaths::for_store(&store).expect("real");
        let second = DaemonPaths::for_store(&alias).expect("alias");
        assert_eq!(first.endpoint, second.endpoint);
        assert_eq!(first.token_file, second.token_file);
    }
}
