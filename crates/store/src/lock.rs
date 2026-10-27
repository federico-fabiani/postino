//! The independent single-writer guard.
//!
//! SQLite already refuses two write locks, but the daemon needs a stronger, earlier statement: one
//! process *owns* the store, its scheduler and its network sessions, and a second writer must be
//! refused before it opens a connection. This is an advisory OS file lock on `<store>.lock` plus a
//! small JSON owner record in `<store>.owner` (pid, launch token, start instant, role, endpoint).
//!
//! The owner record is a separate file because Windows file locks are mandatory: a locked range
//! cannot be read by the process that wants to report who owns it. The lock file is therefore just
//! the lock; the state file is readable while the lock is held and is rewritten by the owner.
//!
//! The OS releases the lock when the process dies, so a crash needs no manual cleanup: the next
//! writer acquires the same file, overwrites the stale record and proceeds. A daemon restart is
//! therefore a normal event, not a repair. The launch token distinguishes two launches that happen
//! to reuse a pid; it is not a secret and never reaches a log.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};

use crate::{StoreError, now_ms};

/// The owner record is tiny; a hostile or damaged file must not become an allocation.
const MAX_OWNER_BYTES: u64 = 4 * 1024;

/// Who owns a store. `Debug` is safe: the launch token is an identity, not a credential.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WriterOwner {
    pub pid: u32,
    /// 16 random bytes as hex, regenerated for every acquisition.
    pub start_token: String,
    /// Unix milliseconds.
    pub started_at: i64,
    /// `daemon`, `cli`, `app` or `test`.
    pub label: String,
    /// Wire protocol when the owner is the daemon.
    #[serde(default)]
    pub protocol: Option<u32>,
    /// The daemon endpoint, when one is bound. A direct CLI writer leaves it null.
    #[serde(default)]
    pub endpoint: Option<String>,
}

impl WriterOwner {
    fn new(label: &str) -> Result<Self, StoreError> {
        let mut token = [0u8; 16];
        getrandom::fill(&mut token)?;
        let token = token.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self { pid: std::process::id(), start_token: token, started_at: now_ms(), label: label.to_owned(), protocol: None, endpoint: None })
    }

    /// A short, non-secret description for a refusal sentence.
    pub fn describe(&self) -> String {
        format!("pid {} ({}), started at {}", self.pid, self.label, self.started_at)
    }
}

/// An acquired writer lock. Dropping it releases the OS lock; the owner record stays in place so a
/// stale owner is still readable until the next writer overwrites it.
pub struct WriterLock {
    _lock_file: File,
    lock_path: PathBuf,
    owner_path: PathBuf,
    owner: WriterOwner,
}

impl WriterLock {
    /// Acquire the lock or return [`StoreError::StoreBusy`] with the recorded owner. A stale record
    /// whose process is gone is acquired normally.
    pub fn acquire(path: &Path) -> Result<Self, StoreError> {
        Self::acquire_as(path, "process", None)
    }

    pub fn acquire_as(path: &Path, label: &str, protocol: Option<u32>) -> Result<Self, StoreError> {
        let path = crate::canonical_store_path(path)?;
        let lock_path = lock_path(&path);
        let owner_path = owner_path(&path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| StoreError::Io { path: lock_path.display().to_string(), source })?;
        match FileExt::try_lock_exclusive(&file) {
            Ok(true) => {}
            Ok(false) => return Err(busy(&path, read_writer(&path))),
            Err(source) => {
                return Err(StoreError::Lock { path: lock_path.display().to_string(), reason: source.to_string() });
            }
        }
        let mut owner = WriterOwner::new(label)?;
        owner.protocol = protocol;
        write_owner_file(&owner_path, &owner)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600));
            let _ = std::fs::set_permissions(&owner_path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Self { _lock_file: file, lock_path, owner_path, owner })
    }

    pub fn owner(&self) -> &WriterOwner {
        &self.owner
    }

    pub fn path(&self) -> &Path {
        &self.lock_path
    }

    /// Record the bound endpoint after the token and pipe/socket exist. The writer lock is already
    /// held, so no other process can be between the two states.
    pub fn set_endpoint(&mut self, endpoint: impl Into<String>) -> Result<(), StoreError> {
        self.owner.endpoint = Some(endpoint.into());
        write_owner_file(&self.owner_path, &self.owner)
    }

    pub fn set_protocol(&mut self, protocol: u32) -> Result<(), StoreError> {
        self.owner.protocol = Some(protocol);
        write_owner_file(&self.owner_path, &self.owner)
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        // The owner record remains for a diagnostic until the next acquisition overwrites it; the
        // OS lock is what makes "owned" true.
        let _ = FileExt::unlock(&self._lock_file);
    }
}

impl std::fmt::Debug for WriterLock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("WriterLock").field("path", &self.lock_path).field("owner", &self.owner).finish()
    }
}

/// The lock file that belongs to a database: `mail.db` → `mail.db.lock`.
pub fn lock_path(store: &Path) -> PathBuf {
    with_suffix(store, ".lock")
}

/// The readable owner record: `mail.db` → `mail.db.owner`.
pub fn owner_path(store: &Path) -> PathBuf {
    with_suffix(store, ".owner")
}

fn with_suffix(store: &Path, suffix: &str) -> PathBuf {
    let mut name = store.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_else(|| "mail.db".into());
    name.push(suffix);
    match store.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

/// Whether the OS writer lock is currently free. It never writes the owner record and never keeps the
/// lock: a `true` answer is only a point-in-time probe. Used by `postino daemon stop` so it returns
/// only after the old daemon has really released the store.
pub fn writer_lock_is_free(store: &Path) -> bool {
    let Ok(store) = crate::canonical_store_path(store) else { return false };
    let path = lock_path(&store);
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
        // No lock file yet means no writer has ever owned this store.
        return !path.exists();
    };
    match FileExt::try_lock_exclusive(&file) {
        Ok(true) => {
            let _ = FileExt::unlock(&file);
            true
        }
        _ => false,
    }
}

/// Read the recorded owner without taking the lock. `None` means no readable record (never acquired
/// or a damaged file); it does not say whether the OS lock is held.
pub fn read_writer(path: &Path) -> Option<WriterOwner> {
    use std::io::Read as _;
    let path = crate::canonical_store_path(path).ok()?;
    let path = owner_path(&path);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_OWNER_BYTES {
        return None;
    }
    let file = std::fs::File::open(&path).ok()?;
    let mut raw = Vec::with_capacity(usize::try_from(metadata.len()).ok()?.min(MAX_OWNER_BYTES as usize));
    file.take(MAX_OWNER_BYTES.saturating_add(1)).read_to_end(&mut raw).ok()?;
    if raw.len() as u64 > MAX_OWNER_BYTES {
        return None;
    }
    serde_json::from_slice(&raw).ok()
}

fn write_owner_file(path: &Path, owner: &WriterOwner) -> Result<(), StoreError> {
    let encoded = serde_json::to_vec(owner)?;
    let temporary = path.with_extension("owner-tmp");
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)
            .map_err(|source| StoreError::Io { path: temporary.display().to_string(), source })?;
        file.write_all(&encoded).map_err(|source| StoreError::Io { path: temporary.display().to_string(), source })?;
        file.sync_all().map_err(|source| StoreError::Io { path: temporary.display().to_string(), source })?;
    }
    #[cfg(windows)]
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&temporary, path).map_err(|source| StoreError::Io { path: path.display().to_string(), source })?;
    Ok(())
}

fn busy(store: &Path, owner: Option<WriterOwner>) -> StoreError {
    let owner = owner.map(|owner| format!(" ({})", owner.describe())).unwrap_or_default();
    StoreError::StoreBusy { path: store.display().to_string(), owner }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_writer_is_refused_and_a_dropped_lock_is_reacquired() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = directory.path().join("mail.db");
        let first = WriterLock::acquire_as(&store, "daemon", Some(1)).expect("first");
        let path = first.path().to_path_buf();
        assert!(path.exists());
        assert!(owner_path(&store).exists());
        match WriterLock::acquire(&store) {
            Err(StoreError::StoreBusy { owner, .. }) => assert!(owner.contains("daemon"), "{owner}"),
            other => panic!("a second writer was not refused: {}", other.err().map(|error| error.to_string()).unwrap_or_default()),
        }
        let recorded = read_writer(&store).expect("owner record");
        assert_eq!(recorded.pid, std::process::id());
        drop(first);
        let second = WriterLock::acquire(&store).expect("the OS released the lock");
        assert_ne!(second.owner().start_token, recorded.start_token, "a new launch rotates the token");
    }

    #[test]
    fn a_stale_file_with_a_dead_pid_does_not_block_a_new_owner() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = directory.path().join("mail.db");
        let stale = WriterOwner {
            pid: 999_999,
            start_token: "00".repeat(16),
            started_at: 1,
            label: "daemon".to_owned(),
            protocol: Some(1),
            endpoint: Some(r"\\.\pipe\postino-stale".to_owned()),
        };
        std::fs::write(owner_path(&store), serde_json::to_vec(&stale).expect("encode")).expect("stale file");
        let lock = WriterLock::acquire(&store).expect("stale file is not a lock");
        assert_eq!(lock.owner().pid, std::process::id());
        assert_eq!(read_writer(&store).expect("owner").label, "process");
    }

    #[test]
    fn the_endpoint_is_recorded_after_acquisition() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = directory.path().join("mail.db");
        let mut lock = WriterLock::acquire_as(&store, "daemon", Some(1)).expect("lock");
        lock.set_endpoint(r"\\.\pipe\postino-test").expect("endpoint");
        let owner = read_writer(&store).expect("owner");
        assert_eq!(owner.protocol, Some(1));
        assert_eq!(owner.endpoint.as_deref(), Some(r"\\.\pipe\postino-test"));
    }

    #[test]
    fn store_open_read_only_locked_is_refused_and_never_writes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("mail.db");
        let key = crate::StoreKey::from_bytes([0x12; 32]);
        {
            let store = crate::Store::open(&path, &key).expect("create store");
            drop(store);
        }
        let read = crate::Store::open_read_only_locked(&path, &key).expect("read-only locked store");
        assert!(read.is_read_only());
        assert!(matches!(read.set_meta("x", "y"), Err(StoreError::Sqlite(_)) | Err(StoreError::ReadOnly)));
        match crate::Store::open_locked(&path, &key) {
            Err(StoreError::StoreBusy { .. }) => {}
            other => {
                panic!("the read-only diagnostic did not hold the writer lock: {}", other.err().map(|error| error.to_string()).unwrap_or_default())
            }
        }
        drop(read);
        crate::Store::open_locked(&path, &key).expect("the read-only lock is released");
    }

    #[test]
    fn a_huge_owner_record_is_not_read() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = directory.path().join("mail.db");
        std::fs::write(owner_path(&store), vec![b'x'; 1024 * 1024]).expect("huge owner file");
        assert!(read_writer(&store).is_none());
    }

    #[test]
    fn the_probe_reports_a_live_owner_without_taking_the_lock() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = directory.path().join("mail.db");
        assert!(writer_lock_is_free(&store), "a store with no lock file is free");
        let lock = WriterLock::acquire(&store).expect("lock");
        assert!(!writer_lock_is_free(&store), "a held store is not free");
        drop(lock);
        assert!(writer_lock_is_free(&store), "the probe sees the released lock");
    }

    #[test]
    fn store_open_locked_refuses_a_second_writer_and_releases_on_drop() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("mail.db");
        let key = crate::StoreKey::from_bytes([11u8; 32]);
        let first = crate::Store::open_locked(&path, &key).expect("first writer");
        match crate::Store::open_locked(&path, &key) {
            Err(StoreError::StoreBusy { .. }) => {}
            other => panic!("the second Store::open_locked was not refused: {}", other.err().map(|error| error.to_string()).unwrap_or_default()),
        }
        drop(first);
        crate::Store::open_locked(&path, &key).expect("the lock is released with the store");
    }
}
