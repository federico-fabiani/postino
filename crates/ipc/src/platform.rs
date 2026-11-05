//! The only platform-specific code in the IPC crate: the current user id, the owner-only token file
//! ACL and the Windows named-pipe security descriptor.
//!
//! Everything below the `windows` module is safe. The Windows module is the documented "minimal
//! platform adapter": raw Win32 calls for the current SID and a DACL made of exactly that SID. It
//! never reads a credential, never disables a check and never changes another object's security.

use std::io;
use std::path::Path;

/// A stable per-user key for endpoint names: the SID on Windows, the numeric uid on Unix. It is
/// hashed before it reaches a pipe or socket name.
pub fn current_user_key() -> io::Result<String> {
    platform_user_key()
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn platform_user_key() -> io::Result<String> {
    // SAFETY: getuid has no arguments, no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    Ok(uid.to_string())
}

/// The base directory for a Unix socket. `$XDG_RUNTIME_DIR` is already owner-only; the fallback
/// per-user directory is created with mode 0700 by the listener.
#[cfg(unix)]
pub fn runtime_dir() -> io::Result<std::path::PathBuf> {
    if let Some(directory) = std::env::var_os("XDG_RUNTIME_DIR")
        && !directory.is_empty()
    {
        return Ok(std::path::PathBuf::from(directory));
    }
    Ok(std::env::temp_dir().join(format!("postino-{}", platform_user_key()?)))
}

/// Verify that a directory used for a Unix socket is a real directory, owned by this user, with no
/// group/other permission. A pre-created or symlinked fallback must be refused, not adopted.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn verify_owner_only_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is not a real directory", path.display())));
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if metadata.uid() != uid {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is owned by another user", path.display())));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is not owner-only (mode {mode:o})", path.display())));
    }
    Ok(())
}

/// Make a token file owner-only. On Unix this is mode 0600; on Windows it replaces the DACL with one
/// full-control ACE for the current user.
pub fn harden_owner_only(path: &Path) -> io::Result<()> {
    platform_harden_owner_only(path)
}

#[cfg(unix)]
fn platform_harden_owner_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Whether a token file has exactly the owner-only shape this crate writes. Used by the client
/// before it trusts the file and by the tests as evidence.
///
/// On Unix `TokenFile::read` checks the mode inline, so the Unix build only reaches this from
/// tests; the Windows build is the one that trusts it.
#[cfg(any(windows, test))]
pub fn acl_is_owner_only(path: &Path) -> io::Result<bool> {
    platform_acl_is_owner_only(path)
}

#[cfg(all(unix, test))]
fn platform_acl_is_owner_only(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt as _;
    let metadata = std::fs::metadata(path)?;
    Ok(metadata.permissions().mode() & 0o077 == 0)
}

#[cfg(windows)]
mod windows {
    //! Raw Win32 SID and ACL calls. `unsafe` is confined here, and every block names its invariant.
    #![allow(unsafe_code)]

    use std::ffi::{OsStr, c_void};
    use std::io;
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetLengthSid,
        GetSecurityDescriptorDacl, GetTokenInformation, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// A SECURITY_ATTRIBUTES plus its absolute security descriptor, kept alive for the CreateNamedPipe
    /// call. Dropping it frees the descriptor with `LocalFree`, the allocator the conversion uses.
    pub struct PipeSecurity {
        attributes: SECURITY_ATTRIBUTES,
        descriptor: PSECURITY_DESCRIPTOR,
    }

    impl PipeSecurity {
        pub fn new() -> io::Result<Self> {
            let sddl = pipe_sddl()?;
            let descriptor = descriptor_from_sddl(&sddl)?;
            let attributes = SECURITY_ATTRIBUTES {
                nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            Ok(Self { attributes, descriptor })
        }

        pub fn as_raw(&mut self) -> *mut c_void {
            (&mut self.attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>()
        }
    }

    impl Drop for PipeSecurity {
        fn drop(&mut self) {
            if !self.descriptor.is_null() {
                // SAFETY: the descriptor came from ConvertStringSecurityDescriptorToSecurityDescriptorW
                // and is freed exactly once, here.
                unsafe {
                    LocalFree(self.descriptor);
                }
            }
        }
    }

    pub fn user_key() -> io::Result<String> {
        let sid = current_sid()?;
        sid_to_string(sid.as_ptr() as PSID)
    }

    pub fn harden_owner_only(path: &std::path::Path) -> io::Result<()> {
        let sid = user_key()?;
        let sddl = format!("D:P(A;;FA;;;{sid})");
        let descriptor = descriptor_from_sddl(&sddl)?;
        let mut present = 0;
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: descriptor is a valid absolute security descriptor from the conversion above.
        let ok = unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };
        if ok == 0 || present == 0 || dacl.is_null() {
            // SAFETY: descriptor is owned by this function until LocalFree below.
            unsafe {
                LocalFree(descriptor);
            }
            return Err(io::Error::other("the generated security descriptor has no DACL"));
        }
        let wide = wide_path(path);
        // SAFETY: wide is NUL-terminated; dacl points into descriptor, which stays alive for the call.
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null_mut(),
            )
        };
        // SAFETY: as above, freed once.
        unsafe {
            LocalFree(descriptor);
        }
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }

    pub fn acl_is_owner_only(path: &std::path::Path) -> io::Result<bool> {
        let sid = user_key()?;
        let wide = wide_path(path);
        let mut owner: PSID = ptr::null_mut();
        let mut dacl_return: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: wide is NUL-terminated; all output pointers are valid for the duration of the call.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut dacl_return,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS || descriptor.is_null() {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let owner_only = descriptor_is_owner_only(descriptor, owner, &sid);
        // SAFETY: descriptor is owned by this function and freed exactly once.
        unsafe {
            LocalFree(descriptor);
        }
        owner_only
    }

    /// Read the actual DACL of a live named pipe and require exactly one full-control ACE for the
    /// current user. PowerShell's FileSystem provider cannot address `\\.\pipe\...`; this Win32 call
    /// can.
    pub fn pipe_acl_is_owner_only(name: &str) -> io::Result<bool> {
        let sid = user_key()?;
        let wide = wide_path(std::path::Path::new(name));
        let mut owner: PSID = ptr::null_mut();
        let mut dacl_return: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: wide is NUL-terminated; all output pointers are valid for the duration of the call.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut dacl_return,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS || descriptor.is_null() {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let owner_only = descriptor_is_owner_only(descriptor, owner, &sid);
        // SAFETY: descriptor is owned by this function and freed exactly once.
        unsafe {
            LocalFree(descriptor);
        }
        owner_only
    }

    fn descriptor_is_owner_only(descriptor: PSECURITY_DESCRIPTOR, owner: PSID, sid: &str) -> io::Result<bool> {
        if !owner_only_sid(owner, sid)? {
            return Ok(false);
        }
        let mut present = 0;
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: descriptor is a valid security descriptor.
        let has_dacl = unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };
        if has_dacl == 0 || present == 0 || dacl.is_null() {
            return Ok(false);
        }
        let mut information = ACL_SIZE_INFORMATION { AceCount: 0, AclBytesInUse: 0, AclBytesFree: 0 };
        // SAFETY: dacl is valid and information is the matching struct for AclSizeInformation.
        let size_ok = unsafe {
            GetAclInformation(
                dacl,
                (&mut information as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
                u32::try_from(std::mem::size_of::<ACL_SIZE_INFORMATION>()).unwrap_or(0),
                AclSizeInformation,
            )
        };
        if size_ok == 0 || information.AceCount != 1 {
            return Ok(false);
        }
        let mut ace: *mut c_void = ptr::null_mut();
        // SAFETY: index 0 exists because AceCount is 1.
        let ace_ok = unsafe { GetAce(dacl, 0, &mut ace) };
        if ace_ok == 0 || ace.is_null() {
            return Ok(false);
        }
        // SAFETY: the ACL is ACCESS_ALLOWED-shaped in the file this crate writes; read the header
        // first, then interpret only when the type says it is an allowed ACE.
        let header = unsafe { (*(ace as *const ACCESS_ALLOWED_ACE)).Header };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE as u8 {
            return Ok(false);
        }
        let ace_sid = unsafe { (&(*(ace as *const ACCESS_ALLOWED_ACE)).SidStart) as *const u32 as PSID };
        owner_only_sid(ace_sid, sid)
    }

    /// The DACL the named pipe is created with: one generic-all ACE for the current user only.
    pub fn pipe_sddl() -> io::Result<String> {
        Ok(format!("D:P(A;;GA;;;{})", user_key()?))
    }

    /// Atomically replace `destination` with `source` on Windows. `std::fs::rename` refuses an
    /// existing destination; removing it first would leave a window where the token file is absent.
    pub fn replace_file(source: &std::path::Path, destination: &std::path::Path) -> io::Result<()> {
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};
        let source = wide_path(source);
        let destination = wide_path(destination);
        // SAFETY: both paths are NUL-terminated and the flags are valid.
        #[allow(unsafe_code)]
        let ok = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
        if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    fn owner_only_sid(sid: PSID, expected: &str) -> io::Result<bool> {
        if sid.is_null() {
            return Ok(false);
        }
        Ok(sid_to_string(sid)? == expected)
    }

    fn current_sid() -> io::Result<Vec<u8>> {
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: GetCurrentProcess is a pseudo-handle and token is a valid out pointer.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut size = 0u32;
        // SAFETY: a null buffer with size 0 asks for the required length; the call is expected to fail.
        unsafe {
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut size);
        }
        if size == 0 {
            // SAFETY: token was opened above and is closed exactly once here.
            unsafe {
                CloseHandle(token);
            }
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u8; usize::try_from(size).map_err(|_| io::Error::other("token user buffer is too large"))?];
        // SAFETY: buffer has `size` bytes and token is valid.
        let ok = unsafe { GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast::<c_void>(), size, &mut size) };
        // SAFETY: token is closed exactly once.
        unsafe {
            CloseHandle(token);
        }
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful TokenUser call filled a TOKEN_USER at the start of the buffer.
        let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
        let sid = user.User.Sid;
        if sid.is_null() {
            return Err(io::Error::other("the process token has no user SID"));
        }
        // SAFETY: sid is valid for at least GetLengthSid bytes.
        let length = usize::try_from(unsafe { GetLengthSid(sid) }).map_err(|_| io::Error::other("the user SID is too long"))?;
        if length == 0 || length > 1024 {
            return Err(io::Error::other("the user SID has an impossible length"));
        }
        // SAFETY: sid points to at least `length` bytes.
        Ok(unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), length) }.to_vec())
    }

    fn sid_to_string(sid: PSID) -> io::Result<String> {
        let mut string: *mut u16 = ptr::null_mut();
        // SAFETY: sid is a valid SID; string is an out pointer freed with LocalFree.
        if unsafe { ConvertSidToStringSidW(sid, &mut string) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let value = {
            // SAFETY: string is NUL-terminated on success.
            let slice = unsafe { std::slice::from_raw_parts(string, string_length(string)) };
            String::from_utf16_lossy(slice)
        };
        // SAFETY: string came from ConvertSidToStringSidW and is freed exactly once.
        unsafe {
            LocalFree(string as *mut c_void);
        }
        Ok(value)
    }

    fn string_length(string: *const u16) -> usize {
        let mut length = 0usize;
        // SAFETY: caller guarantees a NUL-terminated string. The bound stops a malformed pointer from
        // becoming an unbounded scan.
        while length < 512 && unsafe { *string.add(length) } != 0 {
            length += 1;
        }
        length
    }

    fn descriptor_from_sddl(sddl: &str) -> io::Result<PSECURITY_DESCRIPTOR> {
        let wide: Vec<u16> = OsStr::new(sddl).encode_wide().chain(std::iter::once(0)).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: wide is NUL-terminated and descriptor is a valid out pointer.
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(wide.as_ptr(), SDDL_REVISION_1, &mut descriptor, ptr::null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if descriptor.is_null() {
            return Err(io::Error::other("the security descriptor is empty"));
        }
        Ok(descriptor)
    }

    fn wide_path(path: &std::path::Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }
}

#[cfg(windows)]
fn platform_user_key() -> io::Result<String> {
    windows::user_key()
}

#[cfg(windows)]
fn platform_harden_owner_only(path: &Path) -> io::Result<()> {
    windows::harden_owner_only(path)
}

#[cfg(windows)]
fn platform_acl_is_owner_only(path: &Path) -> io::Result<bool> {
    windows::acl_is_owner_only(path)
}

#[cfg(windows)]
pub use windows::PipeSecurity;
#[cfg(windows)]
pub use windows::pipe_acl_is_owner_only;
#[cfg(windows)]
pub use windows::replace_file;

#[cfg(not(any(unix, windows)))]
fn platform_user_key() -> io::Result<String> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Postino IPC is implemented on Windows and Unix only"))
}

#[cfg(not(any(unix, windows)))]
fn platform_harden_owner_only(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Postino IPC is implemented on Windows and Unix only"))
}

#[cfg(not(any(unix, windows)))]
fn platform_acl_is_owner_only(_path: &Path) -> io::Result<bool> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Postino IPC is implemented on Windows and Unix only"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_user_key_is_not_empty() {
        let key = current_user_key().expect("user key");
        assert!(!key.is_empty());
        assert!(key.len() <= 200);
    }

    #[test]
    fn a_written_token_file_is_owner_only() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("mail.db.token");
        std::fs::write(&path, b"test\n").expect("write");
        harden_owner_only(&path).expect("harden");
        assert!(acl_is_owner_only(&path).expect("acl"));
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_directory_must_be_a_real_owner_only_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().expect("tempdir");
        let real = directory.path().join("real");
        std::fs::create_dir(&real).expect("real");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).expect("mode");
        verify_owner_only_dir(&real).expect("real owner-only dir");
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        assert!(verify_owner_only_dir(&link).is_err(), "a symlinked socket directory was accepted");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).expect("mode");
        assert!(verify_owner_only_dir(&real).is_err(), "a group-readable socket directory was accepted");
    }

    #[cfg(windows)]
    #[test]
    fn the_pipe_dacl_names_only_the_current_user() {
        let sddl = windows::pipe_sddl().expect("sddl");
        let user = current_user_key().expect("user");
        assert!(sddl.contains(&user), "{sddl}");
        for broad in ["WD", "AU", "BU", "SY", "BA"] {
            assert!(!sddl.contains(broad), "the pipe DACL must not grant {broad}: {sddl}");
        }
    }
}
