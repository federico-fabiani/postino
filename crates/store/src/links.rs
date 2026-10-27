//! The one platform call in the store: a database file's hard-link count. A hard-linked SQLCipher
//! file has two names and therefore two independent `.lock` files; the store refuses that state
//! instead of letting two writers acquire different locks for one inode.
//!
//! Unix reads `nlink` from `std`; Windows needs `GetFileInformationByHandle` because the stable
//! `MetadataExt` does not expose the link count. This module is the minimal Windows adapter; the
//! rest of the store is safe.

#[cfg(unix)]
pub fn link_count(path: &std::path::Path) -> std::io::Result<u32> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(std::fs::metadata(path)?.nlink().try_into().unwrap_or(u32::MAX))
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub fn link_count(path: &std::path::Path) -> std::io::Result<u32> {
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};

    let file = std::fs::File::open(path)?;
    // SAFETY: a zeroed BY_HANDLE_FILE_INFORMATION is a valid all-zero value for the API.
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the file handle is valid for this call and `information` is the matching struct.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(information.nNumberOfLinks)
}

#[cfg(not(any(unix, windows)))]
pub fn link_count(_path: &std::path::Path) -> std::io::Result<u32> {
    Ok(1)
}
