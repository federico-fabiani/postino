//! Content-addressed, encrypted attachment blobs.
//!
//! Attachment bytes are the largest payload in a mailbox and they are write-once: they do not belong
//! in SQLCipher pages. This module keeps them as one file per distinct content, named by the BLAKE3
//! hash of the **plaintext**, under `<root>/<first two hex chars>/<hash>.blob`.
//!
//! # File format, version 1
//!
//! ```text
//! offset  size  field
//!      0     4  magic "PSTB"
//!      4     1  version (1)
//!      5     1  flags (bit 0: the payload is zstd-compressed)
//!      6     8  original_len, u64 little-endian: the plaintext length, never the payload length
//!     14    24  XChaCha20-Poly1305 nonce
//!     38     n  ciphertext || Poly1305 tag
//! ```
//!
//! The associated data is `magic || version || flags || original_len || hash bytes`. The hash is in
//! the AAD, so a blob cannot be renamed to another hash, and the length is in it, so a truncated or
//! extended file is rejected instead of being decrypted with the wrong plaintext size. The flags
//! byte is in the AAD too: it decides whether the payload is decompressed, so leaving it out would
//! authenticate the length but not the interpretation.
//!
//! # Compression
//!
//! zstd level 3, used **only** when the compressed frame is smaller than the plaintext (flag bit 0).
//! Decompression always goes through [`zstd::bulk::Decompressor`] with a capacity of exactly the
//! declared plaintext length. A blob that claims 10 GiB fails against the caller's `max_bytes`
//! before a byte is decrypted, and a payload that decodes to more than the declared length is a
//! `CorruptBlob`, never an allocation.
//!
//! # Integrity and dedup
//!
//! `get` authenticates the ciphertext and then recomputes BLAKE3 over the decrypted plaintext: the
//! file name is the content hash, so a payload planted under a false name is `CorruptBlob` even
//! when it was sealed with the right subkey and the right AAD. `put` does not trust the name either:
//! an existing file is authenticated and compared with the new bytes before it counts as a dedup
//! hit, and a damaged or foreign file at that name is replaced by the atomic write.
//!
//! # Writes
//!
//! A put writes `.tmp-<random>` in the target shard with `create_new` and `sync_all`s it, then
//! publishes it with `hard_link`: the final name appears atomically and an existing name is never
//! removed, so a concurrent reader sees either the old complete file or the new one. A name that is
//! already present is a dedup hit when its bytes match; otherwise a replacing rename heals it (the
//! only path where a reader can observe a gap, and [`BlobStore::get`] waits through it). A filesystem
//! without hard links falls back to the replacing rename for every publish. A failed write removes
//! its temp file.
//!
//! Afterwards the shard directory is fsynced on Unix; on Windows the published file is reopened and
//! flushed, which is the portable half of the job and leaves the directory-entry window documented
//! in `crates/store/README.md`.
//!
//! Blobs are content-addressed and deduplicated: the same bytes are stored once even when ten
//! messages carry them.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use postino_crypto::Subkey;
use zeroize::Zeroizing;

use crate::{StoreError, StoreKey};

/// File magic: `Postino STored Blob`.
pub const MAGIC: &[u8; 4] = b"PSTB";

/// File format version understood by this build.
pub const VERSION: u8 = 1;

/// Flag bit 0: the sealed payload is zstd-compressed.
pub const FLAG_COMPRESSED: u8 = 0b0000_0001;

/// Offset of the version byte.
const VERSION_OFFSET: usize = MAGIC.len();
/// Offset of the flags byte.
const FLAGS_OFFSET: usize = VERSION_OFFSET + 1;
/// Offset of the little-endian `u64` plaintext length.
const LENGTH_OFFSET: usize = FLAGS_OFFSET + 1;
/// Offset of the 24-byte nonce: 14.
const NONCE_OFFSET: usize = LENGTH_OFFSET + 8;
/// Bytes before the ciphertext: magic, version, flags, original length and nonce.
pub const HEADER_LEN: usize = NONCE_OFFSET + postino_crypto::NONCE_LEN;

/// Length of a content hash in hex characters (BLAKE3, 32 bytes).
pub const HASH_HEX_LEN: usize = 64;

/// Hard ceiling for one blob, on write and on read. The engine caps a single attachment at 50 MiB;
/// this is the storage backstop that keeps a hand-written file from asking for a gigabyte.
pub const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;

/// zstd level. Attachments are mostly documents and images; level 3 is the speed/size knee.
const ZSTD_LEVEL: i32 = 3;

/// How many temp names a put tries before giving up on a random collision.
const TEMP_ATTEMPTS: usize = 4;

/// The HKDF context label for blob subkeys. Changing it makes every existing blob unreadable.
const KEY_CONTEXT: &str = "blob";

/// How many times a read retries a name that is momentarily absent, and the pause between tries.
///
/// An atomic replace is not atomic *against readers* on Windows: the platform can unlink the old
/// name a scheduling quantum before the new one appears. A concurrent publish or heal is therefore
/// a window, not a lost blob, and `get` waits through a few of them before reporting `BlobMissing`.
const MISSING_RETRIES: u32 = 4;
const MISSING_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(2);

/// The encrypted blob store: one directory tree plus the subkey derived from the store key.
///
/// `Debug` is safe: the subkey redacts itself.
#[derive(Debug)]
pub struct BlobStore {
    root: PathBuf,
    key: Subkey,
}

impl BlobStore {
    /// Create or open the blob directory and derive the blob subkey from the store key.
    ///
    /// A root that cannot be created, or exists as a file, is an error: silently falling back to
    /// in-memory or plaintext storage would lose attachments on the next open.
    pub fn open(root: impl Into<PathBuf>, key: &StoreKey) -> Result<Self, StoreError> {
        let root = root.into();
        // Distinguish "a file is in the way" from "the directory does not exist yet"; `create_dir_all`
        // reports both as an I/O error and only the first is a configuration mistake worth naming.
        match fs::metadata(&root) {
            Ok(metadata) if !metadata.is_dir() => return Err(StoreError::BlobRoot { path: root.display().to_string() }),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&root).map_err(|error| io_error(&root, error))?;
                let metadata = fs::metadata(&root).map_err(|error| io_error(&root, error))?;
                if !metadata.is_dir() {
                    return Err(StoreError::BlobRoot { path: root.display().to_string() });
                }
            }
            Err(error) => return Err(io_error(&root, error)),
        }
        let subkey = postino_crypto::derive_key(key.as_bytes(), KEY_CONTEXT)?;
        Ok(Self { root, key: subkey })
    }

    /// The directory every blob lives under.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Store `bytes` and return their lowercase BLAKE3 hex hash.
    ///
    /// An existing file at that name is authenticated and compared before it counts as a dedup hit.
    /// A new name is published with an atomic hard link; a corrupt or foreign file under an existing
    /// name is healed with a replacing publish. The hash is computed over the plaintext and reveals
    /// nothing about the key or the payload.
    pub fn put(&self, bytes: &[u8]) -> Result<String, StoreError> {
        // usize -> u64 is lossless on every target Rust supports (usize is at most 64 bits).
        let original_len = bytes.len() as u64;
        if original_len > MAX_BLOB_BYTES {
            return Err(StoreError::BlobTooLarge { declared: original_len, allowed: MAX_BLOB_BYTES });
        }
        let hash = blake3::hash(bytes).to_hex().to_string();
        let path = self.path_for(&hash)?;
        if path.is_file() && self.read(&hash, &path, original_len).is_ok_and(|existing| existing.as_slice() == bytes) {
            return Ok(hash);
        }
        let Some(directory) = path.parent() else {
            return Err(StoreError::BlobRoot { path: self.root.display().to_string() });
        };
        fs::create_dir_all(directory).map_err(|error| io_error(directory, error))?;
        let (flags, payload) = encode(bytes)?;
        let aad = associated_data(&hash, flags, original_len)?;
        let sealed = postino_crypto::seal(&self.key, &aad, &payload)?;
        let Some((nonce, ciphertext)) = sealed.split_at_checked(postino_crypto::NONCE_LEN) else {
            // `seal` always returns nonce || ciphertext; this is a defence against a future refactor.
            return Err(StoreError::CorruptBlob { hash, detail: "sealing produced a value shorter than its nonce" });
        };
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(MAGIC);
        header.push(VERSION);
        header.push(flags);
        header.extend_from_slice(&original_len.to_le_bytes());
        header.extend_from_slice(nonce);
        // A publish never removes an existing name: it hard-links the temp into place, and only a
        // heal of invalid content falls back to a replacing rename. That keeps the common concurrent
        // put on a path where a reader sees either the old complete file or the new one, never a gap.
        write_atomic(&path, &header, ciphertext, || self.read(&hash, &path, original_len).is_ok_and(|existing| existing.as_slice() == bytes))?;
        Ok(hash)
    }

    /// Read and authenticate one blob, refusing anything that would decode to more than `max_bytes`.
    ///
    /// `max_bytes` is the caller's cap on the *plaintext* length, normally the attachment's recorded
    /// size plus a small slack. The check happens against the declared length before decryption, and
    /// the allocation is bounded by it; [`MAX_BLOB_BYTES`] is an additional hard ceiling.
    pub fn get(&self, hash: &str, max_bytes: u64) -> Result<Zeroizing<Vec<u8>>, StoreError> {
        let path = self.path_for(hash)?;
        self.read(hash, &path, max_bytes)
    }

    /// Read a blob, tolerating the momentary absence a concurrent replace can create.
    fn read(&self, hash: &str, path: &Path, max_bytes: u64) -> Result<Zeroizing<Vec<u8>>, StoreError> {
        let mut attempt = 0;
        loop {
            match self.read_once(hash, path, max_bytes) {
                Err(error) if attempt + 1 < MISSING_RETRIES && is_missing(&error) => {
                    attempt += 1;
                    std::thread::sleep(MISSING_RETRY_DELAY);
                }
                result => return result,
            }
        }
    }

    /// The read half of [`BlobStore::get`], also used by [`BlobStore::put`] to authenticate an
    /// existing dedup candidate. Every bound and every check lives here, so dedup cannot take a
    /// shortcut around the format.
    fn read_once(&self, hash: &str, path: &Path, max_bytes: u64) -> Result<Zeroizing<Vec<u8>>, StoreError> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(StoreError::BlobMissing { hash: hash.to_owned() }),
            Err(error) => return Err(io_error(path, error)),
        };
        if !metadata.is_file() {
            return Err(StoreError::CorruptBlob { hash: hash.to_owned(), detail: "the blob path is not a regular file" });
        }
        let file_len = metadata.len();
        if file_len < (HEADER_LEN + postino_crypto::TAG_LEN) as u64 {
            return Err(StoreError::CorruptBlob { hash: hash.to_owned(), detail: "the file is shorter than a header and a tag" });
        }
        let mut file = File::open(path).map_err(|error| io_error(path, error))?;
        let mut header = [0u8; HEADER_LEN];
        file.read_exact(&mut header).map_err(|error| io_error(path, error))?;

        if header.get(..MAGIC.len()) != Some(MAGIC.as_slice()) {
            return Err(StoreError::CorruptBlob { hash: hash.to_owned(), detail: "the file does not start with the PSTB magic" });
        }
        let Some(version) = header.get(VERSION_OFFSET).copied() else {
            return Err(StoreError::CorruptBlob { hash: hash.to_owned(), detail: "no version byte" });
        };
        if version != VERSION {
            return Err(StoreError::CorruptBlob { hash: hash.to_owned(), detail: "unsupported blob version" });
        }
        let Some(flags) = header.get(FLAGS_OFFSET).copied() else {
            return Err(StoreError::CorruptBlob { hash: hash.to_owned(), detail: "no flags byte" });
        };
        if flags & !FLAG_COMPRESSED != 0 {
            return Err(StoreError::CorruptBlob { hash: hash.to_owned(), detail: "unknown flag bits" });
        }
        let Some(length_bytes) = header.get(LENGTH_OFFSET..NONCE_OFFSET).and_then(|bytes| <[u8; 8]>::try_from(bytes).ok()) else {
            return Err(StoreError::CorruptBlob { hash: hash.to_owned(), detail: "no plaintext length" });
        };
        let original_len = u64::from_le_bytes(length_bytes);
        let allowed = max_bytes.min(MAX_BLOB_BYTES);
        if original_len > allowed {
            return Err(StoreError::BlobTooLarge { declared: original_len, allowed });
        }
        // A writer only keeps the compressed payload when it is smaller than the plaintext, so no
        // valid file is longer than header + original length + tag. Checking this before allocating
        // is what makes the read bounded by `max_bytes`.
        let longest = (HEADER_LEN as u64).saturating_add(original_len).saturating_add(postino_crypto::TAG_LEN as u64);
        if file_len > longest {
            return Err(StoreError::CorruptBlob { hash: hash.to_owned(), detail: "the file is longer than its declared length" });
        }
        let Some(original_len_usize) = usize::try_from(original_len).ok() else {
            return Err(StoreError::BlobTooLarge { declared: original_len, allowed });
        };
        let Some(ciphertext_len) = usize::try_from(file_len - HEADER_LEN as u64).ok() else {
            return Err(StoreError::BlobTooLarge { declared: original_len, allowed });
        };
        // Read the nonce and ciphertext as one buffer: `postino_crypto::open` takes `nonce || ct`.
        let mut sealed = vec![0u8; postino_crypto::NONCE_LEN.saturating_add(ciphertext_len)];
        file.seek(SeekFrom::Start(NONCE_OFFSET as u64)).map_err(|error| io_error(path, error))?;
        file.read_exact(&mut sealed).map_err(|error| io_error(path, error))?;

        let aad = associated_data(hash, flags, original_len)?;
        let plaintext = postino_crypto::open(&self.key, &aad, &sealed).map_err(|_| StoreError::CorruptBlob {
            hash: hash.to_owned(),
            detail: "the ciphertext did not authenticate: wrong key, wrong hash or damaged bytes",
        })?;
        let plaintext = if flags & FLAG_COMPRESSED == 0 {
            plaintext
        } else {
            let mut decompressor = zstd::bulk::Decompressor::new().map_err(StoreError::Compression)?;
            let decoded = decompressor.decompress(&plaintext[..], original_len_usize).map_err(|_| StoreError::CorruptBlob {
                hash: hash.to_owned(),
                detail: "the compressed payload does not decode within its declared length",
            })?;
            Zeroizing::new(decoded)
        };
        if plaintext.len() != original_len_usize {
            return Err(StoreError::CorruptBlob { hash: hash.to_owned(), detail: "the decrypted length does not match the header" });
        }
        // The name is the content hash. The AAD already binds it, so this only catches a writer who
        // had the subkey and deliberately sealed different bytes under this name.
        if blake3::hash(&plaintext).to_hex().as_str() != hash {
            return Err(StoreError::CorruptBlob { hash: hash.to_owned(), detail: "the decrypted bytes do not hash to the blob name" });
        }
        Ok(plaintext)
    }

    /// True when a blob file exists for `hash`.
    pub fn has(&self, hash: &str) -> Result<bool, StoreError> {
        let path = self.path_for(hash)?;
        match fs::metadata(&path) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error(&path, error)),
        }
    }

    /// Every blob name, sorted. Names that are not exactly 64 lowercase hex characters plus
    /// `.blob` are ignored: a temporary file or a foreign file is not a blob and is never deleted by
    /// the garbage collector. The scan is bounded by the directory tree, not by a caller's input;
    /// deleting is bounded separately by the GC batch.
    pub fn list_hashes(&self) -> Result<Vec<String>, StoreError> {
        let mut hashes = Vec::new();
        let shards = fs::read_dir(&self.root).map_err(|error| io_error(&self.root, error))?;
        for shard in shards {
            let shard = shard.map_err(|error| io_error(&self.root, error))?;
            let path = shard.path();
            let file_type = shard.file_type().map_err(|error| io_error(&path, error))?;
            if file_type.is_symlink() || !file_type.is_dir() {
                // A symlinked shard could make a sweep unlink outside the blob root.
                continue;
            }
            let entries = fs::read_dir(&path).map_err(|error| io_error(&path, error))?;
            for entry in entries {
                let entry = entry.map_err(|error| io_error(&path, error))?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                let Some(hash) = name.strip_suffix(".blob") else { continue };
                if hash.len() == HASH_HEX_LEN && hash.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) {
                    hashes.push(hash.to_owned());
                }
            }
        }
        hashes.sort_unstable();
        hashes.dedup();
        Ok(hashes)
    }

    /// Delete one blob. Returns `false` when it was already gone. The shard directory is removed
    /// when it becomes empty; a failure to do so is ignored because it only costs an empty folder.
    ///
    /// Removing bytes does not remove attachment rows: the caller decides whether a metadata row
    /// still points at the hash.
    pub fn remove(&self, hash: &str) -> Result<bool, StoreError> {
        let path = self.path_for(hash)?;
        match fs::remove_file(&path) {
            Ok(()) => {
                if let Some(directory) = path.parent() {
                    let _ = fs::remove_dir(directory);
                }
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error(&path, error)),
        }
    }

    /// Validate `hash` and build its path. The hash is checked before any path is assembled, so
    /// `..`, separators, uppercase hex, short and long values never reach the filesystem.
    fn path_for(&self, hash: &str) -> Result<PathBuf, StoreError> {
        decode_hash(hash)?;
        let Some(shard) = hash.get(..2) else {
            return Err(StoreError::BadHash { length: hash.chars().count() });
        };
        Ok(self.root.join(shard).join(format!("{hash}.blob")))
    }
}

/// zstd-compress `bytes` when that actually saves space, and return the flags plus the payload to
/// seal. The payload is zeroising: compressed bytes still derive from the plaintext.
fn encode(bytes: &[u8]) -> Result<(u8, Zeroizing<Vec<u8>>), StoreError> {
    // A zstd frame is at least a header and a block header; below that it can never win.
    if bytes.len() > 64 {
        let compressed = zstd::bulk::compress(bytes, ZSTD_LEVEL).map_err(StoreError::Compression)?;
        if compressed.len() < bytes.len() {
            return Ok((FLAG_COMPRESSED, Zeroizing::new(compressed)));
        }
    }
    Ok((0, Zeroizing::new(bytes.to_vec())))
}

/// Write a blob through a temp file in the destination shard and publish it.
///
/// The publish uses `hard_link`, which creates the final directory entry atomically and fails if the
/// name exists, so a concurrent writer can never see a half-written or briefly missing blob. When the
/// name already exists and its bytes are ours (`already_published`), this is a dedup hit and the new
/// temp is dropped. When the existing bytes are invalid or foreign, a replacing rename heals the
/// name; that is the only path where a reader can observe a gap, and `BlobStore::read` waits through
/// it. A filesystem without hard links (some external drives) falls back to the same replacing
/// rename for every publish.
fn write_atomic(path: &Path, header: &[u8], ciphertext: &[u8], already_published: impl FnOnce() -> bool) -> Result<(), StoreError> {
    let Some(directory) = path.parent() else {
        return Err(StoreError::BlobRoot { path: path.display().to_string() });
    };
    let (temp, mut file) = create_temp(directory)?;
    let written = (|| -> std::io::Result<()> {
        file.write_all(header)?;
        file.write_all(ciphertext)?;
        file.sync_all()
    })();
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(io_error(&temp, error));
    }
    match fs::hard_link(&temp, path) {
        Ok(()) => {
            let _ = fs::remove_file(&temp);
            sync_published(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if already_published() {
                // Nothing was published; the existing entry is already durable.
                let _ = fs::remove_file(&temp);
                Ok(())
            } else if let Err(error) = fs::rename(&temp, path) {
                let _ = fs::remove_file(&temp);
                Err(io_error(path, error))
            } else {
                sync_published(path)
            }
        }
        Err(_) => {
            // No hard-link support on this filesystem; a replacing rename is the portable fallback.
            if let Err(error) = fs::rename(&temp, path) {
                let _ = fs::remove_file(&temp);
                return Err(io_error(path, error));
            }
            sync_published(path)
        }
    }
}

/// Persist the directory entry created by the publish.
///
/// The temp file was fsynced before it was published, so the bytes are durable; what remains is the
/// name. On Unix the shard directory is fsynced. Windows has no portable directory fsync and this
/// crate must not add unsafe Win32 calls, so it reopens the published file and flushes it: the data is
/// durable, but a power loss in the microseconds after the publish can still lose the directory
/// entry. The failure mode is a `BlobMissing` on the next read (never wrong bytes), and the engine
/// commits the attachment row only after this returns.
fn sync_published(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        if let Some(directory) = path.parent() {
            File::open(directory).and_then(|directory| directory.sync_all()).map_err(|error| io_error(directory, error))?;
        }
    }
    // On Windows the handle needs write access for `FlushFileBuffers`; opening without truncation
    // leaves the blob untouched.
    #[cfg(windows)]
    {
        OpenOptions::new().write(true).open(path).and_then(|file| file.sync_all()).map_err(|error| io_error(path, error))?;
    }
    Ok(())
}

/// Create a fresh `.tmp-<random>` file in `directory` with `create_new`.
fn create_temp(directory: &Path) -> Result<(PathBuf, File), StoreError> {
    for _ in 0..TEMP_ATTEMPTS {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)?;
        let path = directory.join(format!(".tmp-{}", hex(&random)));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error(&path, error)),
        }
    }
    Err(StoreError::TempName { path: directory.display().to_string() })
}

/// `magic || version || flags || original_len || hash`, the AAD of every v1 blob.
fn associated_data(hash: &str, flags: u8, original_len: u64) -> Result<Vec<u8>, StoreError> {
    let hash_bytes = decode_hash(hash)?;
    let mut aad = Vec::with_capacity(MAGIC.len() + 2 + 8 + hash_bytes.len());
    aad.extend_from_slice(MAGIC);
    aad.push(VERSION);
    aad.push(flags);
    aad.extend_from_slice(&original_len.to_le_bytes());
    aad.extend_from_slice(&hash_bytes);
    Ok(aad)
}

/// Decode 64 lowercase hex characters into the 32 hash bytes.
fn decode_hash(hash: &str) -> Result<[u8; 32], StoreError> {
    let bytes = hash.as_bytes();
    if bytes.len() != HASH_HEX_LEN {
        return Err(StoreError::BadHash { length: hash.chars().count() });
    }
    let mut out = [0u8; 32];
    // 64 bytes -> 32 exact pairs; the length check above guarantees the zip visits every slot.
    let (pairs, _) = bytes.as_chunks::<2>();
    for (slot, pair) in out.iter_mut().zip(pairs) {
        let Some(high) = pair.first().copied().and_then(nibble) else {
            return Err(StoreError::BadHash { length: hash.chars().count() });
        };
        let Some(low) = pair.get(1).copied().and_then(nibble) else {
            return Err(StoreError::BadHash { length: hash.chars().count() });
        };
        *slot = (high << 4) | low;
    }
    Ok(out)
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn io_error(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io { path: path.display().to_string(), source }
}

/// A read failure that a concurrent replace can cause: the name was absent for a moment.
fn is_missing(error: &StoreError) -> bool {
    match error {
        StoreError::BlobMissing { .. } => true,
        StoreError::Io { source, .. } => source.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_layout_matches_the_documented_offsets() {
        assert_eq!(HEADER_LEN, 38);
        assert_eq!(HEADER_LEN - postino_crypto::NONCE_LEN, 14, "the nonce starts after the length");
        assert_eq!(MAGIC, b"PSTB");
        assert_eq!(FLAG_COMPRESSED, 1);
    }

    #[test]
    fn a_hash_must_be_64_lowercase_hex_characters() {
        let valid = "0".repeat(64);
        assert!(decode_hash(&valid).is_ok());
        for invalid in [
            String::new(),
            "A".repeat(64),
            "g".repeat(64),
            "0".repeat(63),
            "0".repeat(65),
            format!("../{}", "0".repeat(61)),
            format!("..\\{}", "0".repeat(61)),
            format!("{}/{}", "0".repeat(31), "0".repeat(32)),
        ] {
            assert!(matches!(decode_hash(&invalid), Err(StoreError::BadHash { .. })), "{invalid:?} was accepted");
        }
    }

    #[test]
    fn a_rejected_hash_is_not_echoed() {
        let hostile = format!("{}\n\u{0007}../..", "Z".repeat(40));
        let error = decode_hash(&hostile).expect_err("not lowercase hex");
        let rendered = error.to_string();
        assert!(!rendered.contains('Z'), "the error echoes the rejected input: {rendered}");
        assert!(!rendered.contains(".."), "the error echoes path syntax: {rendered}");
        assert!(!rendered.contains('\n'), "the error carries a raw line break: {rendered}");
        assert!(rendered.contains(&hostile.chars().count().to_string()), "the length is useful for a caller: {rendered}");
    }
}
